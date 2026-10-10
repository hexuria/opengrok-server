//! The coworker's computer.
//!
//! ONE TRAIT, SEVERAL COMPUTERS. A coworker's computer is a seam, not a vendor: the harness asks
//! for a shell, a file, a port; something behind this trait provides them. The first
//! implementation drives box.ascii.dev through a typed v1 client (`ascii::Client`, shapes from
//! `docs/box/`); a local Docker one for tests and self-hosting comes next.
//! The client already models its own computer this way (`BoxEndpoint { host, port, authToken }`),
//! so keeping the seam here is what lets the same coworker run on either.
//!
//! WHY STREAMING IS ITS OWN METHOD. box.ascii.dev executes a command either synchronously
//! (blocking to 600s) or detached with a poll-only status endpoint — there is no live socket for
//! stdout. A caller that wants to show output while it happens must therefore poll, and pretending
//! otherwise behind a nice `Stream` would hide the latency from the person choosing a timeout. So
//! `run` is honest about being a single result, and `watch` is honest about being a poll.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub mod ascii;
pub mod bundle;
pub mod devtools;
pub mod docker;
pub mod viewer;

pub use ascii::{AsciiBoxes, Client as AsciiClient};
pub use docker::DockerComputer;

/// Guest `/v1/info` `capabilities.egress_tunnel` (hexuria/box).
///
/// `enabled` is `BOX_EGRESS_TUNNEL=1`: the guest started the CONNECT proxy on
/// `127.0.0.1:8791` (guest-internal; never published) and launched Chromium
/// with `--proxy-server`. The laptop client attaches to the WS on guest
/// `0.0.0.0:8790`, which Docker publishes as `127.0.0.1::8790` only when the
/// host wants the tunnel. `ready` means that client is attached. OpenGrok does
/// not dial that WS; we only report availability and gate leave-box tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EgressTunnel {
    pub enabled: bool,
    pub ready: bool,
}

/// How long a guest's `/v1/info` is waited on before the tunnel counts as not there.
pub const GUEST_INFO_PATIENCE: std::time::Duration = std::time::Duration::from_secs(1);

/// How long a guest that spent all of `GUEST_INFO_PATIENCE` without answering is not asked again
/// by `shown_egress`.
pub const SILENT_GUEST_FOR: std::time::Duration = std::time::Duration::from_secs(10);

/// The guest's tunnel answer for a status to SHOW (the Computer pane, the settings page): asked
/// on every read, except of a guest that let `/v1/info` take all of `GUEST_INFO_PATIENCE` in the
/// last `SILENT_GUEST_FOR` — the pane polls three routes every two seconds, and each one paid that
/// wait again (3 Oct 2026). Only that wait is silence (`Computer::egress_probe`): timing the whole
/// probe called a slow `docker inspect` a silent guest. A box stopped, reset or woken since is a
/// new guest (`Computer::generation`). Never for a decision: a turn asks
/// `Computer::egress_tunnel` itself, as a remembered "no" there would skip the consent card.
pub async fn shown_egress(computer: &dyn Computer, box_id: &str) -> Option<EgressTunnel> {
    use std::sync::{LazyLock, Mutex};
    use std::time::Instant;
    type Silent = std::collections::HashMap<String, (Instant, u64)>;
    static SILENT: LazyLock<Mutex<Silent>> = LazyLock::new(Mutex::default);
    let generation = computer.generation(box_id);
    if let Ok(silent) = SILENT.lock()
        && let Some((since, of)) = silent.get(box_id)
        && *of == generation
        && since.elapsed() < SILENT_GUEST_FOR
    {
        return None;
    }
    let (cap, said_nothing) = computer.egress_probe(box_id).await;
    if let Ok(mut silent) = SILENT.lock() {
        if said_nothing {
            silent.insert(box_id.to_string(), (Instant::now(), generation));
        } else {
            silent.remove(box_id);
        }
    }
    cap
}

impl EgressTunnel {
    /// `None` when the capability is absent or malformed. Both booleans must be
    /// present; a partial object is not `ready: false` and is not available.
    pub fn from_info(info: &serde_json::Value) -> Option<Self> {
        let cap = info.get("capabilities")?.get("egress_tunnel")?;
        Some(Self {
            enabled: cap.get("enabled").and_then(serde_json::Value::as_bool)?,
            ready: cap.get("ready").and_then(serde_json::Value::as_bool)?,
        })
    }

    /// Gateway verb: host wants the tunnel AND `capabilities.egress_tunnel.ready`.
    /// No box, failed `/v1/info`, or `enabled` without a laptop client → false.
    /// NativeChat must not paint the toggle live until a client is attached.
    #[must_use]
    pub fn advertised(host_wants: bool, box_cap: Option<Self>) -> bool {
        host_wants && box_cap.is_some_and(|cap| cap.ready)
    }
}

/// What a command did. `truncated` is carried rather than dropped: a tail is not the output, and a
/// coworker reasoning over a silently clipped log reaches confident wrong conclusions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub timed_out: bool,
}

/// A command still running, identified so its output can be polled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartedCommand {
    pub process_id: String,
    pub running: bool,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
}

#[derive(Debug, thiserror::Error)]
pub enum BoxError {
    #[error("the box is unreachable: {0}")]
    Unreachable(String),
    #[error("the box refused: {status} {body}")]
    Refused { status: u16, body: String },
    #[error("no box with that id")]
    NoSuchBox,
    /// The host could not mint a secret for the box (no OS randomness).
    #[error("could not mint a box secret: {0}")]
    Secret(String),
    /// The request reached the box and its answer never came back whole: the connection dropped
    /// mid-call, or the reply could not be read. What the box did meanwhile is unknown, so a
    /// caller that must not repeat a side effect treats it as done. A recipe counted as "not
    /// played" here could be played again on top of the one that had been typing (#120).
    #[error("the connection to the box was lost mid-request: {0}")]
    Interrupted(String),
}

impl BoxError {
    /// A stable code the client maps to copy, independent of the (rewordable) message. Matches the
    /// server/client contract: invalid_key | quota_exceeded | provider_unreachable | provider_error.
    pub fn code(&self) -> &'static str {
        match self {
            BoxError::Unreachable(_) | BoxError::Interrupted(_) => "provider_unreachable",
            BoxError::Refused { status, .. } if *status == 401 || *status == 403 => "invalid_key",
            BoxError::Refused { status, .. } if *status == 402 || *status == 429 => {
                "quota_exceeded"
            }
            BoxError::Refused { .. } => "provider_error",
            BoxError::NoSuchBox => "provider_error",
            BoxError::Secret(_) => "provider_error",
        }
    }
}

pub type BoxResult<T> = std::result::Result<T, BoxError>;

/// A box that is off but can be brought back with `resume`: the disk is kept, nothing is running.
/// Each provider has its own word for it — Docker says `exited`/`paused`/`created`, box.ascii.dev
/// says `archived` (its auto-stop ARCHIVES; a box is never "stopped" there) — and a client that
/// only knows one of them leaves the others sitting "This computer is archived." forever.
pub fn is_asleep(state: &str) -> bool {
    matches!(
        state,
        "stopped" | "paused" | "exited" | "created" | "archived"
    )
}

/// A box on its way somewhere: not up yet, and not to be resumed again. box.ascii.dev answers
/// `POST /resume` with 202 and `provisioning`, then `provisioned`/`ready` — a single 202 is not a
/// running box, and a command sent before `ready` is refused with 409 `box_starting`. `archiving`
/// is here too: a box mid-archive cannot be resumed until it has finished becoming `archived`.
pub fn is_starting(state: &str) -> bool {
    matches!(
        state,
        "provisioning"
            | "provisioned"
            | "init"
            | "cloning"
            | "restarting"
            | "resuming"
            | "archiving"
    )
}

/// Which of a computer's screens a call is for. Bots that share a computer share its one screen,
/// browser and keyboard (`Shared`), unless one was told to use its own (#376): then it has a
/// screen of its own on the same computer, with its own browser and profile (hexuria/box
/// `box-screen`), named here by the Bot's id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum Screen {
    #[default]
    Shared,
    Own(String),
}

impl Screen {
    /// Its word on the wire (`GET /coworkers/{id}/computer`'s `screen`): `own` or `shared`.
    #[must_use]
    pub fn word(&self) -> &'static str {
        if self.bot().is_some() {
            "own"
        } else {
            "shared"
        }
    }

    /// The Bot whose own screen this is; `None` for the shared one.
    #[must_use]
    pub fn bot(&self) -> Option<&str> {
        match self {
            Screen::Shared => None,
            Screen::Own(bot) => Some(bot),
        }
    }
}

/// A computer, the box on it a Bot works in, and the screen of it the Bot uses: where a fill types.
pub type Target = (std::sync::Arc<dyn Computer>, String, Screen);

/// What kind of box has focus on the page in front of a computer's browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Focus {
    /// `password`, `text` (another place to type text), `other` or `none`; `None` while the
    /// page has not said.
    pub kind: Option<String>,
    /// How many reports the browser has made: it moves when focus does, so a fill that clicked
    /// a field waits for it before trusting `kind`.
    pub seq: u64,
}

/// What the box's screen looks like right now: the whole display as a PNG, base64 so it can
/// ride a JSON frame and a model message without decoding on the way.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Screenshot {
    pub mime: String,
    pub png_base64: String,
    pub width: u32,
    pub height: u32,
}

/// One computer-use action, in the box's own vocabulary (hexuria/box's client has a method per
/// variant). Coordinates are pixels of the display.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum CuaAction {
    Click {
        x: i32,
        y: i32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        button: Option<u8>,
    },
    DoubleClick {
        x: i32,
        y: i32,
    },
    Move {
        x: i32,
        y: i32,
    },
    Drag {
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
    },
    Type {
        text: String,
    },
    Key {
        key: String,
    },
    Scroll {
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
    },
}

impl CuaAction {
    /// A few words for a status line: "clicking at 120,40", "typing".
    pub fn describe(&self) -> String {
        match self {
            Self::Click { x, y, .. } => format!("clicking at {x},{y}"),
            Self::DoubleClick { x, y } => format!("double-clicking at {x},{y}"),
            Self::Move { x, y } => format!("moving to {x},{y}"),
            Self::Drag { x1, y1, x2, y2 } => format!("dragging {x1},{y1} to {x2},{y2}"),
            Self::Type { .. } => "typing".to_string(),
            Self::Key { key } => format!("pressing {key}"),
            Self::Scroll { .. } => "scrolling".to_string(),
        }
    }
}

/// What a box runs against what the provider would create today. `stale` is the whole
/// question a person asks before clicking Update.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ImageStatus {
    /// The image the box was created from, as the provider identifies it (a digest for Docker).
    pub running: String,
    /// The image a box created now would get.
    pub latest: String,
}

impl ImageStatus {
    pub fn stale(&self) -> bool {
        self.running != self.latest
    }
}

/// The refusal for a lifecycle step this provider does not do (update, keep data across a
/// recreate). Read by a person, so it says which step.
pub fn not_supported(what: &str) -> BoxError {
    BoxError::Refused {
        status: 501,
        body: format!("this computer's provider cannot {what}"),
    }
}

/// The refusal every provider without a desktop gives for a screen action.
pub fn no_screen() -> BoxError {
    BoxError::Refused {
        status: 501,
        body: "this computer has no screen".to_string(),
    }
}

/// A computer a coworker can work on.
#[async_trait]
pub trait Computer: Send + Sync {
    /// Bring a computer up. Returns the id the coworker's row will remember it by.
    async fn create(&self, ttl_seconds: Option<u64>) -> BoxResult<String>;

    /// Run to completion. For anything that might outlive a request, use `start` + `watch`.
    async fn run(
        &self,
        box_id: &str,
        command: &str,
        timeout_seconds: u32,
    ) -> BoxResult<CommandOutput>;

    /// Start something long-running and return immediately.
    async fn start(&self, box_id: &str, command: &str) -> BoxResult<StartedCommand>;

    /// Whether [`Computer::devtools`] can work here. Asked before anything in the box is
    /// touched on a pipe's behalf, so a provider without one never has its browser replaced.
    fn offers_a_pipe(&self) -> bool {
        false
    }

    /// Chromium on a DevTools pipe this process holds (see [`devtools`]). Only a provider that
    /// can run the box's `box-chromium-pipe` offers it; the rest have no passkeys.
    async fn devtools(
        &self,
        _box_id: &str,
        _screen: &Screen,
        _url: &str,
    ) -> BoxResult<devtools::DevTools> {
        Err(BoxError::Refused {
            status: 501,
            body: "this computer has no DevTools pipe".to_string(),
        })
    }

    /// Ask again. The tail is bounded by the provider; see the note about truncation above.
    async fn watch(&self, box_id: &str, process_id: &str) -> BoxResult<StartedCommand>;

    async fn read_file(&self, box_id: &str, path: &str) -> BoxResult<String>;

    /// Read a file's bytes without UTF-8 conversion. Binary files like screenshots or recordings
    /// must use this instead of `read_file`, which would silently corrupt them with lossy UTF-8
    /// decoding. The trait provides this seam because `read_file` returns a String and would hide
    /// the corruption from callers that never see the mangling happen.
    async fn read_file_bytes(&self, _box_id: &str, _path: &str) -> BoxResult<Vec<u8>> {
        Err(not_supported("read binary files"))
    }

    async fn write_file(&self, box_id: &str, path: &str, content: &str) -> BoxResult<()>;

    /// Write a file's bytes without UTF-8 conversion — the mirror of `read_file_bytes`. A
    /// binary file (a .docx zip, a rendered PNG) sent through `write_file` is lossy-decoded
    /// before it is ever sent. Defaulted so the trait's many test stubs stay text-only.
    async fn write_file_bytes(&self, _box_id: &str, _path: &str, _bytes: &[u8]) -> BoxResult<()> {
        Err(not_supported("write binary files"))
    }

    /// Publish a port and get a URL a person can open.
    async fn expose_port(&self, box_id: &str, port: u16, title: &str) -> BoxResult<String>;

    /// Stop billing, keep the disk. `resume` brings the same filesystem back.
    async fn stop(&self, box_id: &str) -> BoxResult<()>;
    /// Ask for the box back. This only STARTS a resume on providers that restore asynchronously;
    /// use `wake` when the caller needs the box up.
    async fn resume(&self, box_id: &str) -> BoxResult<()>;

    /// Bring a sleeping box up and WAIT for it: resume it if it is asleep, then poll `state` until
    /// it is `running` (or gone, or in error, or `patience` is spent). Returns the last state seen,
    /// so the caller checks for `"running"` rather than trusting that a resume was accepted — on
    /// box.ascii.dev an accepted resume is a 202 and `provisioning`, and commands sent before the box
    /// is `ready` are refused with 409 `box_starting`.
    ///
    /// A box already starting is not resumed again; a box mid-`archiving` is waited on and resumed
    /// once it is `archived`. A resume refused while the provider already reports the box on its
    /// way up is not an error. A transport error from `state` ends the wake with that error —
    /// callers treat a wake as best-effort and go on to try the box — rather than retrying inside.
    ///
    /// A box that was resumed and is `exited` again on two polls in a row started and died — a
    /// container whose entrypoint exits at once — and waiting the full patience for it would only
    /// delay saying so: the wake ends there with that state. Only `exited` counts: `stopped` is
    /// also what box.ascii.dev's `state` says for a transient refusal, and `archived` can lag a
    /// resume the provider has already accepted. So the early end is Docker's; an ascii box that
    /// never comes up costs the patience, once per turn, and then says the computer is down.
    async fn wake(&self, box_id: &str, patience: std::time::Duration) -> BoxResult<String> {
        let started = std::time::Instant::now();
        let mut resumed = false;
        let mut asleep_since_resume = 0u8;
        let mut state = self.state(box_id).await?;
        loop {
            if state == "running" || state == "absent" || state == "error" {
                return Ok(state);
            }
            if resumed && state == "exited" {
                asleep_since_resume += 1;
                if asleep_since_resume >= 2 {
                    return Ok(state);
                }
            } else {
                asleep_since_resume = 0;
            }
            if is_starting(&state) {
                // On its way (or still archiving): nothing to send, only patience.
            } else if is_asleep(&state) && !resumed {
                match self.resume(box_id).await {
                    Ok(()) => resumed = true,
                    Err(error) => {
                        // A second resume against a box that just accepted one is refused; the
                        // state tells us whether that refusal matters.
                        let now = self.state(box_id).await?;
                        if is_asleep(&now) {
                            return Err(error);
                        }
                        resumed = true;
                        state = now;
                        continue;
                    }
                }
            }
            if started.elapsed() >= patience {
                return Ok(state);
            }
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            state = self.state(box_id).await?;
        }
    }

    /// Permanent. The disk goes with it.
    async fn destroy(&self, box_id: &str) -> BoxResult<()>;

    /// Remove a box and keep its disk: what a box from `recreate` gets when it loses its scope, as
    /// it runs on the disk of the box it replaced, which may be the winner's too. A provider that
    /// can `recreate` must say how; for one that cannot, it is never asked.
    async fn discard(&self, _box_id: &str) -> BoxResult<()> {
        Err(not_supported("remove a box but keep its disk"))
    }

    /// The box's live run-state, as a lowercase word the boot UI can render honestly:
    /// `"running"` (up and serving), `"absent"` (no such box — released or never created), or the
    /// provider's own word for anything in between (`"exited"`, `"stopped"`, `"created"`, …). The
    /// client treats every non-`"running"` value as "no live screen" and reports it, so this is the
    /// signal that lets a dead box say it is dead instead of spinning "Booting up" forever. Cheap to
    /// poll. Never errors on a missing box — that is `"absent"`, not a failure.
    async fn state(&self, box_id: &str) -> BoxResult<String>;

    /// Whether this box has a desktop — decided from how the box is made (Docker: the desktop
    /// ports it was created with), not from whether it is awake right now. A turn offers screen
    /// tools on the strength of this and wakes the box the first time one is used. `false` by
    /// default: a provider that cannot `act` or `screenshot` must not offer a screen it cannot
    /// serve.
    async fn offers_a_screen(&self, _box_id: &str) -> bool {
        false
    }

    /// The URL of the page in front of the box's browser, as the box's own extension reports it
    /// (hexuria/box `GET /v1/chrome/active-tab`), asked before a fill types into the page.
    /// `Ok(None)` when the box answered that no browser page is open, which is a fact a fill
    /// acts on; an error when it cannot say at all: no screen, or an image from before the
    /// report, which is the default.
    async fn active_tab_url(&self, _box_id: &str, _screen: &Screen) -> BoxResult<Option<String>> {
        Err(BoxError::Unreachable(
            "this computer does not say which page is in front".to_string(),
        ))
    }

    /// What kind of box has focus on the page in front (hexuria/box `GET /v1/chrome/active-tab`,
    /// its `focus` and `seq`), asked before each value of a fill is typed. An error when the
    /// computer cannot say, which is the default.
    async fn focused_field(&self, _box_id: &str, _screen: &Screen) -> BoxResult<Focus> {
        Err(BoxError::Unreachable(
            "this computer does not say which box has focus".to_string(),
        ))
    }

    /// A URL a person can open to SEE this box's screen (noVNC), or `None` when it has none — which
    /// is the default, because most of our computers are headless (shell + files, no desktop). A
    /// provider that can surface a graphical desktop (box.ascii.dev) overrides this; the client draws
    /// the screen when it is `Some`, and says "no screen" when it is `None`, so we never invent one.
    async fn screen_url(&self, _box_id: &str, _screen: &Screen) -> BoxResult<Option<String>> {
        Ok(None)
    }

    /// The display as a PNG. Default: no screen, so a refusal the model can read.
    async fn screenshot(&self, _box_id: &str, _screen: &Screen) -> BoxResult<Screenshot> {
        Err(no_screen())
    }

    /// Click, type, press, scroll or drag on the display. Default: no screen.
    async fn act(&self, _box_id: &str, _screen: &Screen, _action: &CuaAction) -> BoxResult<()> {
        Err(no_screen())
    }

    /// Open a page in the box's own browser. The desktop image ships `box-chromium`, which
    /// joins the running Chromium's profile; `start` is the detached exec every provider has.
    /// A Bot's own screen is a provider's to start (the docker box's `box-screen`); one that
    /// cannot says so.
    async fn open_url(&self, box_id: &str, screen: &Screen, url: &str) -> BoxResult<()> {
        if screen.bot().is_some() {
            return Err(not_supported("give a Bot a screen of its own"));
        }
        let quoted = url.replace('\'', "'\\''");
        self.start(box_id, &format!("box-chromium '{quoted}'"))
            .await?;
        Ok(())
    }

    /// Stop a Bot's own screen (its browser, its display), keeping its profile for the next
    /// time: it went back to the shared screen. Default: nothing to stop.
    async fn close_screen(&self, _box_id: &str, _bot: &str) -> BoxResult<()> {
        Ok(())
    }

    /// The image (or template) a box created now would run. Empty when the provider has no such
    /// notion.
    fn image(&self) -> String {
        String::new()
    }

    /// What this box runs against what a new one would get. Default: no way to tell, which the
    /// caller shows as "up to date" rather than inventing an update.
    async fn image_status(&self, _box_id: &str) -> BoxResult<ImageStatus> {
        Err(not_supported("compare images"))
    }

    /// Fetch the newest image so the next create (or `recreate`) runs it. `Ok(false)` means there
    /// was nothing to fetch — a local-only image is already whatever was built last.
    async fn pull_latest(&self) -> BoxResult<bool> {
        Ok(false)
    }

    /// A fresh box on the newest image that keeps the old box's data (home and workspace), then
    /// removes the old one. Returns the new id. The step behind "Update this computer".
    async fn recreate(&self, _old_box_id: &str) -> BoxResult<String> {
        Err(not_supported("update a box in place"))
    }

    /// Run a whole taught recipe in one call — hexuria/box's `POST /v1/cua/recipe` body in,
    /// its receipt out (`ok`, `ran`, `stopped_at`, per-step results, an end screenshot).
    /// Default: no screen.
    async fn run_recipe(
        &self,
        _box_id: &str,
        _screen: &Screen,
        _request: &serde_json::Value,
    ) -> BoxResult<serde_json::Value> {
        Err(no_screen())
    }

    /// Which kind of computer this is, for advertising the options to a client:
    /// `"local-docker"` (a VM on the server host) or `"ascii"` (a box.ascii.dev box). Defaults to
    /// local-docker; the ascii provider overrides it.
    fn kind(&self) -> &'static str {
        "local-docker"
    }

    /// Guest `GET /v1/info` `capabilities.egress_tunnel`, or `None` when the
    /// guest cannot be asked or does not advertise the capability. Default
    /// `None` (not available). A Docker desktop probes box-host; it does not
    /// dial the tunnel WS or `GET /v1/egress`.
    async fn egress_tunnel(&self, _box_id: &str) -> Option<EgressTunnel> {
        None
    }

    /// `egress_tunnel`, and whether the guest was reached and then let its `/v1/info` take all
    /// of `GUEST_INFO_PATIENCE` — the one silence `shown_egress` remembers. Getting to the guest
    /// (its ports, its token) is not the guest's silence. Default: never silent.
    async fn egress_probe(&self, box_id: &str) -> (Option<EgressTunnel>, bool) {
        (self.egress_tunnel(box_id).await, false)
    }

    /// How many times this provider has stopped, started, rebuilt or removed the box in this
    /// process. What was learned of a box under one count (its screen's port, a guest that said
    /// nothing) is not the box's under the next: Docker gives a box new ports at every start and
    /// frees them at every stop. `0` from a provider that cannot tell.
    fn generation(&self, _box_id: &str) -> u64 {
        0
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::time::Duration;

    /// A box whose reported states are scripted: the last one repeats forever.
    struct Scripted {
        states: Mutex<VecDeque<&'static str>>,
        resumes: AtomicUsize,
        /// What its guest says of the tunnel, after how long, and whether that wait was the
        /// guest's own silence; how often it was asked; its generation, counted as Docker's is.
        guest: (Option<EgressTunnel>, std::time::Duration, bool),
        asked: AtomicUsize,
        generation: AtomicU64,
    }

    impl Scripted {
        fn new(states: &[&'static str]) -> Self {
            Self {
                states: Mutex::new(states.iter().copied().collect()),
                resumes: AtomicUsize::new(0),
                guest: (None, std::time::Duration::ZERO, false),
                asked: AtomicUsize::new(0),
                generation: AtomicU64::new(0),
            }
        }

        fn with_guest(mut self, says: Option<EgressTunnel>, after: Duration, silent: bool) -> Self {
            self.guest = (says, after, silent);
            self
        }

        fn asked(&self) -> usize {
            self.asked.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl Computer for Scripted {
        async fn create(&self, _ttl: Option<u64>) -> BoxResult<String> {
            Ok("box".into())
        }
        async fn run(&self, _b: &str, _c: &str, _t: u32) -> BoxResult<CommandOutput> {
            Err(BoxError::NoSuchBox)
        }
        async fn start(&self, _b: &str, _c: &str) -> BoxResult<StartedCommand> {
            Err(BoxError::NoSuchBox)
        }
        async fn watch(&self, _b: &str, _p: &str) -> BoxResult<StartedCommand> {
            Err(BoxError::NoSuchBox)
        }
        async fn read_file(&self, _b: &str, _p: &str) -> BoxResult<String> {
            Err(BoxError::NoSuchBox)
        }
        async fn write_file(&self, _b: &str, _p: &str, _c: &str) -> BoxResult<()> {
            Err(BoxError::NoSuchBox)
        }
        async fn expose_port(&self, _b: &str, _p: u16, _t: &str) -> BoxResult<String> {
            Err(BoxError::NoSuchBox)
        }
        async fn stop(&self, _b: &str) -> BoxResult<()> {
            self.generation.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn resume(&self, _b: &str) -> BoxResult<()> {
            self.resumes.fetch_add(1, Ordering::SeqCst);
            self.generation.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn destroy(&self, _b: &str) -> BoxResult<()> {
            self.generation.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn state(&self, _b: &str) -> BoxResult<String> {
            let mut states = self.states.lock().unwrap();
            let next = if states.len() > 1 {
                states.pop_front().unwrap()
            } else {
                states.front().copied().unwrap_or("absent")
            };
            Ok(next.to_string())
        }
        async fn egress_tunnel(&self, _b: &str) -> Option<EgressTunnel> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.guest.1).await;
            self.guest.0
        }
        async fn egress_probe(&self, b: &str) -> (Option<EgressTunnel>, bool) {
            (self.egress_tunnel(b).await, self.guest.2)
        }
        fn generation(&self, _b: &str) -> u64 {
            self.generation.load(Ordering::SeqCst)
        }
    }

    /// The pane polls three routes every two seconds, and a guest that spent the probe's whole
    /// patience saying nothing cost each of them that wait (3 Oct 2026). Shown, it is asked once
    /// in `SILENT_GUEST_FOR`; a guest that answers is asked on every read, as the pane shows the
    /// tunnel as it is now.
    #[tokio::test]
    async fn a_status_does_not_wait_again_on_a_guest_that_said_nothing() {
        let silent = Scripted::new(&["running"]).with_guest(None, GUEST_INFO_PATIENCE, true);
        assert_eq!(shown_egress(&silent, "bx_silent").await, None);
        let began = std::time::Instant::now();
        for _ in 0..3 {
            assert_eq!(shown_egress(&silent, "bx_silent").await, None);
        }
        let waited = began.elapsed();
        assert!(
            waited < GUEST_INFO_PATIENCE,
            "waited {waited:?} on a silent guest"
        );
        assert_eq!(silent.asked(), 1);

        let ready = Some(EgressTunnel {
            enabled: true,
            ready: true,
        });
        let answering = Scripted::new(&["running"]).with_guest(ready, Duration::ZERO, false);
        for _ in 0..3 {
            assert_eq!(shown_egress(&answering, "bx_answering").await, ready);
        }
        assert_eq!(answering.asked(), 3);
    }

    /// Reaching the guest is a `docker inspect` before `/v1/info` is asked, and on a loaded host
    /// that alone can take longer than the guest is given. Timing the whole probe called that a
    /// silent guest and stopped asking it for ten seconds; only the guest's own wait counts.
    #[tokio::test]
    async fn a_slow_way_to_the_guest_is_not_a_silent_guest() {
        let slow = Duration::from_millis(1200);
        let unreached = Scripted::new(&["running"]).with_guest(None, slow, false);
        assert_eq!(shown_egress(&unreached, "bx_slow_inspect").await, None);
        assert_eq!(shown_egress(&unreached, "bx_slow_inspect").await, None);
        assert_eq!(unreached.asked(), 2, "a slow way there is asked again");
    }

    /// A box stopped, reset or woken has a guest that has not been asked yet: what the last one
    /// did not say is not remembered against it. Each silence here takes the whole patience,
    /// as a real one does.
    #[tokio::test]
    async fn a_silent_guest_is_asked_again_once_its_box_is_stopped_reset_or_woken() {
        let silent = Scripted::new(&["exited", "running"]);
        let boxes = silent.with_guest(None, GUEST_INFO_PATIENCE, true);
        let read = || shown_egress(&boxes, "bx_lifecycle");
        assert_eq!((read().await, read().await), (None, None));
        assert_eq!(boxes.asked(), 1, "remembered as silent");
        boxes.stop("bx_lifecycle").await.unwrap();
        assert_eq!((read().await, read().await), (None, None));
        assert_eq!(
            boxes.asked(),
            2,
            "asked again after a stop, then remembered"
        );
        boxes.destroy("bx_lifecycle").await.unwrap();
        read().await;
        assert_eq!(boxes.asked(), 3, "asked again after a reset's destroy");
        let woke = boxes.wake("bx_lifecycle", Duration::from_secs(10)).await;
        assert_eq!(woke.unwrap(), "running");
        read().await;
        assert_eq!(boxes.asked(), 4, "asked again after a wake started the box");
    }

    /// A stopped box is started once and the wake returns as soon as it is running.
    #[tokio::test]
    async fn a_sleeping_box_is_started_once_and_the_wake_ends_when_it_runs() {
        let boxes = Scripted::new(&["exited", "running"]);
        let reached = boxes
            .wake("box", std::time::Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(reached, "running");
        assert_eq!(boxes.resumes.load(Ordering::SeqCst), 1);
    }

    /// A provider word other than `exited` — box.ascii.dev says `stopped` for a transient
    /// refusal — does not end the wake early: the patience is what rides that out.
    #[tokio::test]
    async fn a_box_that_only_says_stopped_after_a_start_is_waited_for() {
        let boxes = Scripted::new(&["stopped", "stopped", "stopped"]);
        let began = std::time::Instant::now();
        let reached = boxes
            .wake("box", std::time::Duration::from_secs(4))
            .await
            .unwrap();
        assert_eq!(reached, "stopped");
        assert!(
            began.elapsed() >= std::time::Duration::from_secs(4),
            "gave up after {:?}, before the patience ran out",
            began.elapsed()
        );
    }

    /// A box that starts and dies at once — the stale-lock crash of 20 Sep 2026 — used to cost
    /// the whole patience (90 s per turn). Two `exited` polls after the start end the wake.
    #[tokio::test]
    async fn a_box_that_dies_right_after_starting_is_given_up_on_in_two_polls() {
        let boxes = Scripted::new(&["exited", "exited", "exited"]);
        let began = std::time::Instant::now();
        let reached = boxes
            .wake("box", std::time::Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(reached, "exited");
        assert_eq!(boxes.resumes.load(Ordering::SeqCst), 1);
        assert!(
            began.elapsed() < std::time::Duration::from_secs(10),
            "gave up after {:?}, not within two polls",
            began.elapsed()
        );
    }

    /// The action names and fields are the box's own request bodies; a rename here would be a
    /// silent 400 from every box.
    #[test]
    fn a_cua_action_serializes_in_the_boxs_vocabulary() {
        let click = serde_json::to_value(CuaAction::Click {
            x: 17,
            y: 781,
            button: None,
        })
        .unwrap();
        assert_eq!(
            click,
            serde_json::json!({"action": "click", "x": 17, "y": 781})
        );

        let drag = serde_json::to_value(CuaAction::Drag {
            x1: 1,
            y1: 2,
            x2: 3,
            y2: 4,
        })
        .unwrap();
        assert_eq!(drag["action"], "drag");
        assert_eq!(
            (drag["x1"].as_i64(), drag["y2"].as_i64()),
            (Some(1), Some(4))
        );

        let parsed: CuaAction =
            serde_json::from_value(serde_json::json!({"action": "double_click", "x": 5, "y": 6}))
                .unwrap();
        assert_eq!(parsed, CuaAction::DoubleClick { x: 5, y: 6 });
    }

    #[test]
    fn describe_says_where_not_what_was_typed() {
        assert_eq!(
            CuaAction::Click {
                x: 17,
                y: 781,
                button: None
            }
            .describe(),
            "clicking at 17,781"
        );
        // Typed text can be a password; the status line never repeats it.
        assert_eq!(
            CuaAction::Type {
                text: "hunter2".into()
            }
            .describe(),
            "typing"
        );
        assert_eq!(
            CuaAction::Key {
                key: "Return".into()
            }
            .describe(),
            "pressing Return"
        );
    }

    #[test]
    fn no_screen_is_a_refusal_not_an_outage() {
        assert!(matches!(no_screen(), BoxError::Refused { status: 501, .. }));
    }

    #[test]
    fn egress_tunnel_from_info_reads_enabled_and_ready() {
        let info = serde_json::json!({
            "capabilities": {
                "exec": { "enabled": true, "ready": true },
                "egress_tunnel": { "enabled": true, "ready": false }
            }
        });
        assert_eq!(
            EgressTunnel::from_info(&info),
            Some(EgressTunnel {
                enabled: true,
                ready: false
            })
        );
        assert!(
            EgressTunnel::from_info(&serde_json::json!({"capabilities": {"exec": {}}})).is_none(),
            "old guests without the capability are not ready"
        );
        assert!(
            EgressTunnel::from_info(&serde_json::json!({
                "capabilities": { "egress_tunnel": { "enabled": true } }
            }))
            .is_none(),
            "partial objects are unavailable, not ready:false"
        );
    }

    #[test]
    fn advertised_is_host_wants_and_box_ready() {
        let ready = EgressTunnel {
            enabled: true,
            ready: true,
        };
        let not_ready = EgressTunnel {
            enabled: true,
            ready: false,
        };
        assert!(!EgressTunnel::advertised(false, None));
        assert!(
            !EgressTunnel::advertised(true, None),
            "no /v1/info → not available; do not claim reroute works"
        );
        assert!(EgressTunnel::advertised(true, Some(ready)));
        assert!(
            !EgressTunnel::advertised(true, Some(not_ready)),
            "enabled guest with no laptop client is not available"
        );
        assert!(!EgressTunnel::advertised(false, Some(ready)));
    }
}
