//! A computer made of a local Docker container.
//!
//! WHY THIS EXISTS AT ALL. box.ascii.dev is hosted-only and needs an account; that is a dependency
//! on somebody else's signup before a coworker can have a computer. This one needs a Docker daemon
//! and nothing else, so the trait can be exercised on a laptop, in a test, and in CI — and a
//! coworker can be given a computer today rather than after a key arrives.
//!
//! `docs/PLAN.md` §7 decided a Docker `Computer` is *additive by construction*. This file is that
//! bet being collected: not one line of the harness, the executor or the projection changed.
//!
//! THE CONTAINER FILESYSTEM PERSISTS ACROSS STOP AND START, which is the same promise
//! box.ascii.dev makes and the reason a coworker's machine can sleep between turns. It does NOT
//! survive `destroy` — that removes the container and its writable layer, exactly as the hosted
//! one deletes a disk.
//!
//! ON SHELLING OUT TO THE CLI RATHER THAN SPEAKING THE ENGINE API. The API would avoid a process
//! per call, but it means a socket path, an API version to track and a client dependency, in
//! exchange for a saving that does not matter next to the container's own startup. `docker` on the
//! PATH is the whole configuration.
//!
//! EGRESS TUNNEL (hexuria/box#30). NativeChat "Review an action" needs
//! `capabilities.egress_tunnel.ready` from guest `GET /v1/info`. `ready` is false until a
//! laptop client attaches to the guest WS. Docker cannot publish a port after create, so
//! the WS is only published when the host wants the tunnel at create (or recreate):
//!
//! ```text
//! docker port <box> 8790
//! # 127.0.0.1:NNNN
//! docker inspect -f '{{range .Config.Env}}{{println .}}{{end}}' <box> \
//!   | sed -n 's/^BOX_EGRESS_TUNNEL_BEARER=//p' > /tmp/box-egress.bearer
//! box-egress-tunnel client \
//!   --url ws://127.0.0.1:NNNN \
//!   --bearer-file /tmp/box-egress.bearer
//! ```
//!
//! A container started without 8790 must be recreated. The image must ship `box-egress-tunnel`
//! (`grok-box:local` rebuild). Never publish 8791/8792 — those stay guest-internal.

use async_trait::async_trait;
use tokio::process::Command;

use crate::{
    BoxError, BoxResult, CommandOutput, Computer, CuaAction, EgressTunnel, ImageStatus, Screenshot,
    StartedCommand, no_screen,
};

/// A small image with a shell and the usual utilities. Overridable, because a coworker that needs
/// a toolchain should get one rather than installing it on every turn.
pub const DEFAULT_IMAGE: &str = "debian:stable-slim";

/// Ports published when a box is created.
///
/// Docker cannot publish a port on a container that is already running, so the set has to be
/// decided up front. These are the ports a person actually serves something on; `expose_port`
/// refuses anything else with a reason rather than appearing to succeed.
pub const PUBLISHED_PORTS: &[u16] = &[3000, 5173, 8000, 8080];

/// noVNC (6080) and grok-box exec/host (1337/1340). Only published when the image is a desktop.
/// Not 8790: that WS is opened only when the host wants the egress tunnel, so a default
/// desktop does not expose an unused authenticated socket.
pub const DESKTOP_PORTS: &[u16] = &[6080, 1337, 1340];

/// Guest egress-tunnel WS (hexuria/box#30). The guest listens `0.0.0.0:8790`; we publish
/// only `127.0.0.1::8790`. Never 8791/8792 (guest-internal CONNECT / proxy). Docker cannot
/// add a publish after create — a live box without this port must be recreated.
pub const EGRESS_TUNNEL_PORT: u16 = 8790;

/// x11vnc on grok-box uses the first 8 characters. Same value is put on the noVNC URL.
const DESKTOP_VNC_PASSWORD: &str = "opengrok";

/// The label that names a box's data volumes, so a later `recreate` mounts the same ones.
const VOLUMES_LABEL: &str = "dev.opengrok.volumes";

/// Where a box keeps what a person would miss: the desktop user's home (browser profile,
/// desktop, dotfiles) and the workspace the tools write to.
const HOME_DIR: &str = "/home/box";
const WORKSPACE_DIR: &str = "/workspace";

/// The user the desktop image runs as; copied data has to end up owned by it.
const BOX_USER: &str = "box";

/// A box's two named volumes. Chosen at create, written to the container's labels, read back for
/// `recreate` and `destroy_with_data`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoxVolumes {
    pub home: String,
    pub workspace: String,
}

impl BoxVolumes {
    pub fn fresh() -> Self {
        let suffix = uuid_like();
        Self {
            home: format!("ogbox-{suffix}-home"),
            workspace: format!("ogbox-{suffix}-ws"),
        }
    }

    fn label(&self) -> String {
        format!("{VOLUMES_LABEL}={},{}", self.home, self.workspace)
    }

    fn parse(label: &str) -> Option<Self> {
        let (home, workspace) = label.trim().split_once(',')?;
        (!home.is_empty() && !workspace.is_empty()).then(|| Self {
            home: home.to_string(),
            workspace: workspace.to_string(),
        })
    }
}

/// `image:local` is a convention for "built on this machine": there is nothing to pull, the
/// newest build is whatever `docker build` last tagged.
pub fn image_is_local(image: &str) -> bool {
    image
        .rsplit_once(':')
        .is_some_and(|(_, tag)| tag == "local")
}

#[derive(Debug, Clone)]
pub struct DockerComputer {
    pub image: String,
    /// Marks the containers we made, so `destroy` cannot remove somebody else's.
    pub label: String,
    /// A second label naming the RUN that made the box (`dev.opengrok.run=<tag>`), from
    /// `OG_BOX_RUN_TAG`. The gate sets one per invocation and removes everything carrying it on
    /// exit, so a smoke that hires and forgets leaves nothing behind. `None` outside such runs.
    pub run_tag: Option<String>,
    /// Pins host egress intent for tests. `None` reads `OG_EGRESS_TUNNEL_ENABLED` /
    /// `SAND_EGRESS_TUNNEL_ENABLED` (strict `"1"`, same words as the gateway helper) at
    /// create time. The in-app `egressTunnelEnabled` toggle is host intent for the *verb*;
    /// it cannot publish a port on an already-created container.
    egress_tunnel: Option<bool>,
}

impl Default for DockerComputer {
    fn default() -> Self {
        Self::new()
    }
}

impl DockerComputer {
    pub fn new() -> Self {
        Self {
            image: std::env::var("OG_DOCKER_IMAGE").unwrap_or_else(|_| DEFAULT_IMAGE.to_string()),
            label: "dev.opengrok.box".to_string(),
            run_tag: std::env::var("OG_BOX_RUN_TAG")
                .ok()
                .filter(|tag| !tag.is_empty()),
            egress_tunnel: None,
        }
    }

    pub fn with_image(mut self, image: impl Into<String>) -> Self {
        self.image = image.into();
        self
    }

    /// Pin host egress intent so unit tests do not inherit CI's `OG_EGRESS_TUNNEL_ENABLED=1`.
    pub fn with_egress_tunnel(mut self, on: bool) -> Self {
        self.egress_tunnel = Some(on);
        self
    }

    /// Host wants the guest CONNECT-proxy + WS. Same env words as the gateway helper
    /// (`OG_…` / `SAND_…` strictly `"1"`). Settings live in the gateway crate; Docker
    /// cannot see them here, and they cannot hot-add a publish anyway.
    pub fn wants_egress(&self) -> bool {
        self.egress_tunnel
            .unwrap_or_else(host_wants_egress_from_env)
    }

    /// Run `docker` and return its output, mapping the ways it can fail.
    async fn docker(&self, args: &[&str]) -> BoxResult<String> {
        let output = Command::new("docker")
            .args(args)
            .output()
            .await
            .map_err(|error| BoxError::Unreachable(format!("could not run docker: {error}")))?;

        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        // Docker says "No such container" for a box that is gone — and `docker inspect` says "No
        // such object" for the same thing. A caller retries a refusal but not a missing box, so the
        // two must not be conflated; match both wordings, case-insensitively.
        let lowered = stderr.to_lowercase();
        if lowered.contains("no such container") || lowered.contains("no such object") {
            return Err(BoxError::NoSuchBox);
        }
        Err(BoxError::Refused {
            status: output.status.code().unwrap_or(-1).unsigned_abs() as u16,
            body: stderr.chars().take(500).collect(),
        })
    }

    /// A grok-box image (or `OG_DOCKER_DESKTOP=1`) runs its own entrypoint with a noVNC desktop.
    /// Headless `debian:stable-slim` stays `sleep infinity` so existing boxes do not change.
    pub fn wants_desktop(&self) -> bool {
        std::env::var("OG_DOCKER_DESKTOP").as_deref() == Ok("1") || self.image.contains("grok-box")
    }

    fn published_ports(&self) -> Vec<u16> {
        let mut ports = PUBLISHED_PORTS.to_vec();
        if self.wants_desktop() {
            ports.extend_from_slice(DESKTOP_PORTS);
            if self.wants_egress() {
                ports.push(EGRESS_TUNNEL_PORT);
            }
        }
        ports
    }

    /// The arguments that create a box. Split out so the shape is testable without a daemon.
    pub fn create_args(&self, ttl_seconds: Option<u64>) -> BoxResult<Vec<String>> {
        self.create_args_on(ttl_seconds, &BoxVolumes::fresh())
    }

    /// `create_args` on a chosen pair of volumes — `recreate` passes the old box's, so the new
    /// container comes up on the same home and workspace.
    pub fn create_args_on(
        &self,
        ttl_seconds: Option<u64>,
        volumes: &BoxVolumes,
    ) -> BoxResult<Vec<String>> {
        let mut args = vec![
            "run".to_string(),
            "-d".to_string(),
            "--label".to_string(),
            format!("{}=1", self.label),
        ];
        if let Some(tag) = &self.run_tag {
            args.push("--label".to_string());
            args.push(format!("dev.opengrok.run={tag}"));
        }
        // The data lives in named volumes, not the container layer, so a box can be recreated
        // on a newer image without losing what the person and the coworker put there. A fresh
        // named volume is seeded from the image's own directory on first mount, so the desktop
        // config the image ships still arrives. Headless boxes keep only the workspace.
        args.push("--label".to_string());
        args.push(volumes.label());
        args.push("-v".to_string());
        args.push(format!("{}:{WORKSPACE_DIR}", volumes.workspace));
        if self.wants_desktop() {
            args.push("-v".to_string());
            args.push(format!("{}:{HOME_DIR}", volumes.home));
        }
        for port in self.published_ports() {
            // Bound to loopback: a coworker's box must not be reachable from the network by
            // accident, and a person opening a preview is on this machine.
            args.push("-p".to_string());
            args.push(format!("127.0.0.1::{port}"));
        }
        if self.wants_desktop() {
            // grok-box refuses tokens shorter than 16 characters; `secret_token` is 64 hex.
            args.push("-e".to_string());
            args.push(format!("BOX_TOKEN=og-{}", secret_token()?));
            args.push("-e".to_string());
            args.push(format!("BOX_VNC_PASSWORD={DESKTOP_VNC_PASSWORD}"));
            args.push("-e".to_string());
            args.push("BOX_DESKTOP=1".to_string());
            args.push("-e".to_string());
            args.push("BOX_DESKTOP_REQUIRED=1".to_string());
            args.push("-e".to_string());
            args.push("BOX_CHROME=0".to_string());
            args.push("-e".to_string());
            args.push("BOX_ALLOW_INSECURE_DEV=1".to_string());
            if self.wants_egress() {
                args.push("-e".to_string());
                args.push("BOX_EGRESS_TUNNEL=1".to_string());
                // Dev path: BOX_ALLOW_INSECURE_DEV is already on, so an env bearer is
                // accepted on loopback. Same length posture as BOX_TOKEN (grok-box
                // refuses <16). Recovered after restart by inspecting Config.Env,
                // same as box_token — OpenGrok does not dial the WS.
                args.push("-e".to_string());
                args.push(format!("BOX_EGRESS_TUNNEL_BEARER=og-{}", secret_token()?));
            }
            args.push(self.image.clone());
            return Ok(args);
        }
        args.push(self.image.clone());
        // `sleep infinity` keeps the container alive with no service in it; the TTL is enforced by
        // the shell so a forgotten box stops on its own rather than running until somebody notices.
        args.push("sh".to_string());
        args.push("-c".to_string());
        args.push(match ttl_seconds {
            Some(seconds) => format!("sleep {seconds}"),
            None => "sleep infinity".to_string(),
        });
        Ok(args)
    }
}

#[async_trait]
impl Computer for DockerComputer {
    async fn create(&self, ttl_seconds: Option<u64>) -> BoxResult<String> {
        let args = self.create_args(ttl_seconds)?;
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        let id = self.docker(&borrowed).await?;
        // Docker prints the full 64-character id; the short form is what a person sees everywhere
        // else, and either works as a reference.
        Ok(id.chars().take(12).collect())
    }

    async fn run(
        &self,
        box_id: &str,
        command: &str,
        timeout_seconds: u32,
    ) -> BoxResult<CommandOutput> {
        let output = Command::new("docker")
            .args(["exec", box_id, "sh", "-c", command])
            .output();

        // The timeout is ours, not Docker's: `docker exec` has none, so without this a command
        // that never returns holds a run open forever.
        let output = match tokio::time::timeout(
            std::time::Duration::from_secs(timeout_seconds.clamp(1, 600).into()),
            output,
        )
        .await
        {
            Ok(result) => result
                .map_err(|error| BoxError::Unreachable(format!("could not run docker: {error}")))?,
            Err(_) => {
                return Ok(CommandOutput {
                    // Conventional for "killed by timeout", and never 0: a coworker reading 0
                    // would conclude the command succeeded.
                    exit_code: 124,
                    stdout: String::new(),
                    stderr: String::new(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                    timed_out: true,
                });
            }
        };

        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if !output.status.success() && stderr.contains("No such container") {
            return Err(BoxError::NoSuchBox);
        }

        Ok(CommandOutput {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr,
            // Docker hands back the whole output; nothing here clips it, so saying it was
            // truncated would be a lie.
            stdout_truncated: false,
            stderr_truncated: false,
            timed_out: false,
        })
    }

    async fn start(&self, box_id: &str, command: &str) -> BoxResult<StartedCommand> {
        // The process id is a name we choose, because `docker exec -d` does not report one. It
        // names the log files, which is how `watch` finds the output later.
        let process_id = format!("p{}", uuid_like());
        let script = format!(
            "mkdir -p /tmp/og && ({command}) >/tmp/og/{process_id}.out 2>/tmp/og/{process_id}.err; \
             echo $? >/tmp/og/{process_id}.code"
        );
        self.docker(&["exec", "-d", box_id, "sh", "-c", &script])
            .await?;

        Ok(StartedCommand {
            process_id,
            running: true,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
        })
    }

    async fn watch(&self, box_id: &str, process_id: &str) -> BoxResult<StartedCommand> {
        // One exec rather than three: a poll that costs three container round-trips is a poll
        // people turn off.
        //
        // `exit 0` at the end is load-bearing. While the command is still running there is no exit
        // code file yet, so that last `cat` fails and takes the script's status with it — which
        // would report a perfectly healthy in-progress poll as a refusal from the box.
        let script = format!(
            "cat /tmp/og/{process_id}.out 2>/dev/null; \
             echo '---OG-SPLIT---'; \
             cat /tmp/og/{process_id}.err 2>/dev/null; \
             echo '---OG-SPLIT---'; \
             cat /tmp/og/{process_id}.code 2>/dev/null; \
             exit 0"
        );
        let output = self.docker(&["exec", box_id, "sh", "-c", &script]).await?;
        let mut parts = output.split("---OG-SPLIT---");
        let stdout = parts.next().unwrap_or_default().trim_end().to_string();
        let stderr = parts.next().unwrap_or_default().trim().to_string();
        let exit_code = parts
            .next()
            .and_then(|code| code.trim().parse::<i32>().ok());

        Ok(StartedCommand {
            process_id: process_id.to_string(),
            // The exit code file appears only when the command finished, which is what makes this
            // an honest answer rather than a guess.
            running: exit_code.is_none(),
            stdout,
            stderr,
            exit_code,
        })
    }

    async fn read_file(&self, box_id: &str, path: &str) -> BoxResult<String> {
        self.docker(&["exec", box_id, "cat", path]).await
    }

    async fn read_file_bytes(&self, box_id: &str, path: &str) -> BoxResult<Vec<u8>> {
        let output = Command::new("docker")
            .args(["exec", box_id, "cat", path])
            .output()
            .await
            .map_err(|error| BoxError::Unreachable(format!("could not run docker: {error}")))?;

        if output.status.success() {
            return Ok(output.stdout);
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        // Mirror the docker helper's error handling for consistency.
        let lowered = stderr.to_lowercase();
        if lowered.contains("no such container") || lowered.contains("no such object") {
            return Err(BoxError::NoSuchBox);
        }
        Err(BoxError::Refused {
            status: output.status.code().unwrap_or(-1).unsigned_abs() as u16,
            body: stderr.chars().take(500).collect(),
        })
    }

    async fn write_file(&self, box_id: &str, path: &str, content: &str) -> BoxResult<()> {
        // Written through stdin rather than interpolated into a shell string: content with a quote
        // in it would otherwise become part of the command.
        use tokio::io::AsyncWriteExt;

        let mut child = Command::new("docker")
            .args([
                "exec",
                "-i",
                box_id,
                "sh",
                "-c",
                &format!("mkdir -p \"$(dirname '{path}')\" && cat > '{path}'"),
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|error| BoxError::Unreachable(format!("could not run docker: {error}")))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(content.as_bytes())
                .await
                .map_err(|error| BoxError::Unreachable(error.to_string()))?;
            stdin
                .shutdown()
                .await
                .map_err(|error| BoxError::Unreachable(error.to_string()))?;
        }

        let output = child
            .wait_with_output()
            .await
            .map_err(|error| BoxError::Unreachable(error.to_string()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("No such container") {
                return Err(BoxError::NoSuchBox);
            }
            return Err(BoxError::Refused {
                status: output.status.code().unwrap_or(-1).unsigned_abs() as u16,
                body: stderr.chars().take(500).collect(),
            });
        }
        Ok(())
    }

    async fn expose_port(&self, box_id: &str, port: u16, _title: &str) -> BoxResult<String> {
        // Docker cannot publish a port on a running container, so a port that was not published at
        // creation cannot be exposed now. Saying so is better than returning a URL that refuses
        // every connection.
        if !self.published_ports().contains(&port) {
            return Err(BoxError::Refused {
                status: 400,
                body: format!(
                    "port {port} was not published when this box was created; \
                     this computer publishes {:?}",
                    self.published_ports()
                ),
            });
        }
        let mapping = self.docker(&["port", box_id, &port.to_string()]).await?;
        // `docker port` answers like `127.0.0.1:49154`, one line per binding.
        let bound = mapping
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_string();
        if bound.is_empty() {
            return Err(BoxError::Refused {
                status: 404,
                body: format!("nothing is bound to port {port} on this box"),
            });
        }
        Ok(format!("http://{bound}"))
    }

    async fn stop(&self, box_id: &str) -> BoxResult<()> {
        self.docker(&["stop", box_id]).await.map(|_| ())
    }

    async fn resume(&self, box_id: &str) -> BoxResult<()> {
        self.docker(&["start", box_id]).await.map(|_| ())
    }

    /// The container and its data volumes: "the disk goes with it" is the trait's promise, and
    /// a retired coworker must not leave a home directory behind. `recreate` never comes here —
    /// it removes only the container, so the volumes carry over.
    async fn destroy(&self, box_id: &str) -> BoxResult<()> {
        let volumes = self.volumes_of(box_id).await.ok().flatten();
        self.docker(&["rm", "-f", box_id]).await?;
        if let Some(volumes) = volumes {
            let _ = self
                .docker(&["volume", "rm", "-f", &volumes.home, &volumes.workspace])
                .await;
        }
        Ok(())
    }

    fn image(&self) -> String {
        self.image.clone()
    }

    async fn image_status(&self, box_id: &str) -> BoxResult<ImageStatus> {
        Ok(ImageStatus {
            running: self.running_image(box_id).await?,
            latest: self.latest_image().await?,
        })
    }

    async fn pull_latest(&self) -> BoxResult<bool> {
        if image_is_local(&self.image) {
            return Ok(false);
        }
        self.docker(&["pull", &self.image]).await.map(|_| true)
    }

    /// Stop the old box, make sure its data is in volumes (copying it there once if it predates
    /// them), start a new box on those volumes, remove the old one. The old box is only stopped
    /// while its data is read, so the window a person notices is the copy, not the pull.
    async fn recreate(&self, old_box_id: &str) -> BoxResult<String> {
        let (volumes, needs_copy) = match self.volumes_of(old_box_id).await? {
            Some(volumes) => (volumes, false),
            None => (BoxVolumes::fresh(), true),
        };
        self.docker(&["stop", old_box_id]).await?;
        if needs_copy {
            for volume in [&volumes.home, &volumes.workspace] {
                self.docker(&["volume", "create", volume]).await?;
            }
            self.copy_dir(old_box_id, WORKSPACE_DIR, &volumes.workspace)
                .await?;
            if self.wants_desktop() {
                self.copy_dir(old_box_id, HOME_DIR, &volumes.home).await?;
            }
        }
        // Whether the home was just copied or has lived in this volume for a while, the newest
        // image's desktop defaults fill in whatever is missing.
        if self.wants_desktop() {
            self.seed_defaults(&volumes.home).await?;
        }
        let args = self.create_args_on(None, &volumes)?;
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        let id = self.docker(&borrowed).await?;
        // Only once the new box exists: a failed create above leaves the old one stopped but
        // intact, which `resume` brings back.
        let _ = self.docker(&["rm", "-f", old_box_id]).await;
        Ok(id.chars().take(12).collect())
    }

    async fn state(&self, box_id: &str) -> BoxResult<String> {
        // `docker inspect` prints the container's own status word: running, exited, created, paused,
        // restarting, removing, dead. A container that is gone is `NoSuchBox` here, which is `absent`
        // to the caller — a missing box is a fact, not a failure.
        match self
            .docker(&["inspect", "-f", "{{.State.Status}}", box_id])
            .await
        {
            Ok(status) if !status.is_empty() => Ok(status),
            Ok(_) => Ok("absent".to_string()),
            Err(BoxError::NoSuchBox) => Ok("absent".to_string()),
            Err(other) => Err(other),
        }
    }

    async fn screen_url(&self, box_id: &str) -> BoxResult<Option<String>> {
        match self.docker(&["port", box_id, "6080"]).await {
            Ok(mapping) => Ok(host_port(&mapping).map(|port| {
                format!(
                    "http://127.0.0.1:{port}/vnc.html?autoconnect=true&resize=scale&reconnect=true&password={DESKTOP_VNC_PASSWORD}"
                )
            })),
            Err(BoxError::NoSuchBox) | Err(BoxError::Refused { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn screenshot(&self, box_id: &str) -> BoxResult<Screenshot> {
        let shot = self
            .guest(box_id)
            .await?
            .screenshot()
            .await
            .map_err(guest_error)?;
        Ok(Screenshot {
            mime: shot.mime,
            png_base64: shot.png_base64,
            width: shot.width,
            height: shot.height,
        })
    }

    async fn run_recipe(
        &self,
        box_id: &str,
        request: &serde_json::Value,
    ) -> BoxResult<serde_json::Value> {
        self.guest(box_id)
            .await?
            .recipe(request)
            .await
            .map_err(guest_error)
    }

    async fn act(&self, box_id: &str, action: &CuaAction) -> BoxResult<()> {
        let guest = self.guest(box_id).await?;
        let done = match action {
            CuaAction::Click { x, y, button } => guest.click(*x, *y, *button).await,
            CuaAction::DoubleClick { x, y } => guest.double_click(*x, *y, None).await,
            CuaAction::Move { x, y } => guest.move_pointer(*x, *y).await,
            CuaAction::Drag { x1, y1, x2, y2 } => guest.drag(*x1, *y1, *x2, *y2, None).await,
            CuaAction::Type { text } => guest.type_text(text).await,
            CuaAction::Key { key } => guest.key(key).await,
            CuaAction::Scroll { x, y, dx, dy } => guest.scroll(*x, *y, *dx, *dy).await,
        };
        done.map(|_| ()).map_err(guest_error)
    }

    async fn egress_tunnel(&self, box_id: &str) -> Option<EgressTunnel> {
        let guest = match self.guest(box_id).await {
            Ok(guest) => guest,
            Err(error) => {
                tracing::debug!(
                    %error,
                    %box_id,
                    "guest /v1/info unreachable; egress tunnel not ready"
                );
                return None;
            }
        };
        let info = match tokio::time::timeout(std::time::Duration::from_secs(1), guest.info()).await
        {
            Ok(Ok(info)) => info,
            Ok(Err(error)) => {
                tracing::debug!(
                    %error,
                    %box_id,
                    "guest /v1/info refused; egress tunnel not ready"
                );
                return None;
            }
            Err(_) => {
                tracing::debug!(
                    %box_id,
                    "guest /v1/info timed out; egress tunnel not ready"
                );
                return None;
            }
        };
        EgressTunnel::from_info(&info)
    }
}

/// The guest agents inside a desktop box (`box-exec` on 1337, `box-host` on 1340), spoken to
/// with hexuria/box's own client. What this side owns is only what the client cannot know:
/// the ports Docker published for them, and the `BOX_TOKEN` the box was created with — read
/// back from the container's environment, so a server restart does not lose it. The egress
/// bearer is stored the same way (`BOX_EGRESS_TUNNEL_BEARER`); OpenGrok does not dial that
/// WS, so ops recover it with `docker inspect` for the laptop client.
impl DockerComputer {
    async fn guest(&self, box_id: &str) -> BoxResult<grok_box::GrokBox> {
        let exec_url = self.published_url(box_id, 1337).await?;
        let host_url = self.published_url(box_id, 1340).await?;
        let token = self.box_token(box_id).await?;
        grok_box::GrokBox::connect(exec_url, host_url, token).map_err(guest_error)
    }

    async fn box_token(&self, box_id: &str) -> BoxResult<String> {
        let env = self
            .docker(&[
                "inspect",
                box_id,
                "--format",
                "{{range .Config.Env}}{{println .}}{{end}}",
            ])
            .await?;
        env_from_inspect(&env, "BOX_TOKEN").ok_or_else(no_screen)
    }

    async fn published_url(&self, box_id: &str, port: u16) -> BoxResult<String> {
        let mapping = match self.docker(&["port", box_id, &port.to_string()]).await {
            Ok(mapping) => mapping,
            Err(BoxError::Refused { .. }) => return Err(no_screen()),
            Err(error) => return Err(error),
        };
        host_port(&mapping)
            .map(|host| format!("http://127.0.0.1:{host}"))
            .ok_or_else(no_screen)
    }
}

/// Reading a box's volumes and moving its data — the pieces behind update and reset.
impl DockerComputer {
    /// The volumes a box was created on, from its labels; `None` for a box made before data
    /// lived in volumes (its data is in the container layer and needs the one-time copy).
    async fn volumes_of(&self, box_id: &str) -> BoxResult<Option<BoxVolumes>> {
        let label = self
            .docker(&[
                "inspect",
                box_id,
                "--format",
                &format!("{{{{index .Config.Labels \"{VOLUMES_LABEL}\"}}}}"),
            ])
            .await?;
        Ok(BoxVolumes::parse(&label))
    }

    /// The image id a box runs.
    async fn running_image(&self, box_id: &str) -> BoxResult<String> {
        self.docker(&["inspect", "--format", "{{.Image}}", box_id])
            .await
    }

    /// The id of the image a new box would get — what `docker run <image>` resolves to now.
    async fn latest_image(&self) -> BoxResult<String> {
        self.docker(&["image", "inspect", "--format", "{{.Id}}", &self.image])
            .await
    }

    /// The shell pipeline that copies one directory out of a (stopped) container into a volume,
    /// owned by the box user: `docker cp` streams a tar, a throwaway container on our own image
    /// unpacks it. Pure, so the shape is testable.
    pub fn copy_dir_command(&self, from_box: &str, dir: &str, volume: &str) -> String {
        // Chromium's Singleton{Lock,Socket,Cookie} name the old container's hostname and pid;
        // carried over they read as "the profile is in use on another computer". They are
        // per-process, never data, so the copy drops them.
        format!(
            "docker cp {from_box}:{dir} - | docker run --rm -i -u 0 --entrypoint sh -v {volume}:/dst {} \
             -c 'tar -x -C /dst --strip-components=1 \
                 && find /dst -maxdepth 3 -name \"Singleton*\" -exec rm -f {{}} + \
                 && chown -R {BOX_USER}:{BOX_USER} /dst'",
            self.image
        )
    }

    /// The image's own desktop config (panel, launchers) into a home volume, only where the
    /// volume has nothing: a home copied from an older box never got Docker's first-mount seed,
    /// and a box updated to an image with new launchers should see them — while a panel the
    /// person rearranged is theirs. Pure, so the shape is testable.
    pub fn seed_defaults_command(&self, volume: &str) -> String {
        // The lock cleanup lives here, not only in the copy: a volume reused across a recreate
        // still holds the OLD container's Singleton{Lock,Socket,Cookie}, and the new one has a
        // new hostname, so Chromium would call the profile "in use on another computer".
        //
        // The desktop's own config (`.config/xfce4`: panel, dock, wallpaper) is the IMAGE's, so
        // an update brings the newest look — that is what "update" means for a desktop. What a
        // person made is elsewhere (Desktop, the browser profile, the workspace) and is kept;
        // everything else under `.config` is only filled in where missing.
        format!(
            "docker run --rm -u 0 --entrypoint sh -v {volume}:/dst {} \
             -c 'find /dst -maxdepth 3 -name \"Singleton*\" -exec rm -f {{}} + ; \
                 mkdir -p /dst/.config && rm -rf /dst/.config/xfce4 \
                 && cp -r {HOME_DIR}/.config/xfce4 /dst/.config/xfce4 \
                 && cp -rn {HOME_DIR}/.config/. /dst/.config/ \
                 && chown -R {BOX_USER}:{BOX_USER} /dst/.config'",
            self.image
        )
    }

    async fn seed_defaults(&self, volume: &str) -> BoxResult<()> {
        let output = Command::new("sh")
            .args(["-c", &self.seed_defaults_command(volume)])
            .output()
            .await
            .map_err(|error| BoxError::Unreachable(format!("could not run docker: {error}")))?;
        if output.status.success() {
            return Ok(());
        }
        Err(BoxError::Refused {
            status: output.status.code().unwrap_or(-1).unsigned_abs() as u16,
            body: format!(
                "seeding the desktop defaults failed: {}",
                String::from_utf8_lossy(&output.stderr)
                    .chars()
                    .take(500)
                    .collect::<String>()
            ),
        })
    }

    async fn copy_dir(&self, from_box: &str, dir: &str, volume: &str) -> BoxResult<()> {
        let output = Command::new("sh")
            .args(["-c", &self.copy_dir_command(from_box, dir, volume)])
            .output()
            .await
            .map_err(|error| BoxError::Unreachable(format!("could not run docker: {error}")))?;
        if output.status.success() {
            return Ok(());
        }
        Err(BoxError::Refused {
            status: output.status.code().unwrap_or(-1).unsigned_abs() as u16,
            body: format!(
                "copying {dir} failed: {}",
                String::from_utf8_lossy(&output.stderr)
                    .chars()
                    .take(500)
                    .collect::<String>()
            ),
        })
    }
}

/// The guest's refusal, kept readable for the model: "HTTP 400: x=5000 exceeds width 1280".
fn guest_error(error: grok_box::Error) -> BoxError {
    match error {
        grok_box::Error::Http {
            status, message, ..
        } => BoxError::Refused {
            status,
            body: message,
        },
        other => BoxError::Unreachable(format!("box-exec: {other}")),
    }
}

fn host_port(mapping: &str) -> Option<u16> {
    let line = mapping.lines().next()?.trim();
    let host = line.rsplit_once(':')?.1;
    host.parse().ok()
}

/// Gateway helper's env words, kept here because `opengrok-box` must not depend on the server.
/// Strict `"1"` — `"true"` is off, matching Grok host `=== "1"`.
fn host_wants_egress_from_env() -> bool {
    env_is_one("OG_EGRESS_TUNNEL_ENABLED") || env_is_one("SAND_EGRESS_TUNNEL_ENABLED")
}

fn env_is_one(name: &str) -> bool {
    std::env::var(name).as_deref() == Ok("1")
}

/// One `KEY=` line from `docker inspect` Config.Env. Shared by `BOX_TOKEN` and
/// `BOX_EGRESS_TUNNEL_BEARER` so a restart recovers both the same way.
fn env_from_inspect(blob: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    blob.lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .map(str::to_string)
}

/// A short unique-enough token for naming a process's log files.
///
/// Not a UUID crate: this names two files inside one container, and the id only has to be unique
/// among that container's own concurrent processes.
/// A secret the guest compares in constant time — so it has to be unguessable.
/// `uuid_like` is hex nanoseconds: fine for naming a file, not for a bearer. The
/// egress-tunnel token and `BOX_TOKEN` were both minted from the clock, microseconds
/// apart, so knowing one narrowed the other. 32 bytes from the OS CSPRNG, as hex:
/// 64 characters, well past grok-box's 16-character floor.
fn secret_token() -> BoxResult<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|err| BoxError::Secret(format!("no OS randomness: {err}")))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_box_is_created_on_loopback_and_labelled_as_ours() {
        let args = DockerComputer::new()
            .create_args(Some(60))
            .expect("create args");
        assert!(args.contains(&"run".to_string()));
        assert!(args.contains(&"-d".to_string()));
        // Labelled, so destroy cannot remove a container somebody else made.
        assert!(args.iter().any(|arg| arg == "dev.opengrok.box=1"));
        assert!(
            !args.iter().any(|arg| arg.starts_with("dev.opengrok.run=")),
            "no run tag unless one was given"
        );
        let tagged = DockerComputer {
            run_tag: Some("gate-42".to_string()),
            ..DockerComputer::new()
        };
        assert!(
            tagged
                .create_args(None)
                .expect("create args")
                .iter()
                .any(|arg| arg == "dev.opengrok.run=gate-42")
        );
        // Every published port bound to loopback, never 0.0.0.0.
        for port in PUBLISHED_PORTS {
            assert!(
                args.iter().any(|arg| arg == &format!("127.0.0.1::{port}")),
                "port {port} should be published on loopback"
            );
        }
        assert!(args.iter().any(|arg| arg.contains("sleep 60")));
    }

    /// A box with no TTL still has to be created, but it must not silently become a 0-second one.
    #[test]
    fn a_box_without_a_ttl_sleeps_forever_rather_than_not_at_all() {
        let args = DockerComputer::new()
            .create_args(None)
            .expect("create args");
        assert!(args.iter().any(|arg| arg == "sleep infinity"), "{args:?}");
    }

    #[test]
    fn the_image_can_be_chosen() {
        let args = DockerComputer::new()
            .with_image("rust:1-slim")
            .create_args(None)
            .expect("create args");
        assert!(args.contains(&"rust:1-slim".to_string()));
    }

    #[test]
    fn a_grok_box_image_keeps_its_entrypoint_and_publishes_novnc() {
        let args = DockerComputer::new()
            .with_image("grok-box:local")
            .create_args(None)
            .expect("create args");
        assert!(args.contains(&"grok-box:local".to_string()));
        assert!(
            args.iter().any(|arg| arg == "127.0.0.1::6080"),
            "noVNC must be published, got {args:?}"
        );
        assert!(
            !args.iter().any(|arg| arg.contains("sleep")),
            "desktop image must keep its entrypoint, got {args:?}"
        );
        assert!(args.iter().any(|arg| arg == "BOX_DESKTOP=1"));
        assert!(args.iter().any(|arg| arg == "BOX_CHROME=0"));
        assert!(args.iter().any(|arg| arg.starts_with("BOX_TOKEN=og-")));
    }

    #[test]
    fn desktop_create_publishes_egress_ws_when_the_host_wants_it() {
        let args = DockerComputer::new()
            .with_image("grok-box:local")
            .with_egress_tunnel(true)
            .create_args(None)
            .expect("create args");
        assert!(
            args.iter().any(|arg| arg == "127.0.0.1::8790"),
            "guest WS must be loopback-published, got {args:?}"
        );
        assert!(
            !args
                .iter()
                .any(|arg| arg.contains("::8791") || arg.contains("::8792")),
            "8791/8792 stay guest-internal, got {args:?}"
        );
        assert!(
            args.iter().any(|arg| arg == "BOX_EGRESS_TUNNEL=1"),
            "{args:?}"
        );
        let bearer = args
            .iter()
            .find(|arg| arg.starts_with("BOX_EGRESS_TUNNEL_BEARER="))
            .expect("create must pass a bearer");
        let value = bearer
            .strip_prefix("BOX_EGRESS_TUNNEL_BEARER=")
            .expect("prefix");
        assert!(
            value.len() >= 16,
            "grok-box refuses short bearers, got {value:?}"
        );
        assert!(value.starts_with("og-"), "{value}");
    }

    #[test]
    fn desktop_create_does_not_open_the_egress_ws_when_the_host_does_not_want_it() {
        let args = DockerComputer::new()
            .with_image("grok-box:local")
            .with_egress_tunnel(false)
            .create_args(None)
            .expect("create args");
        assert!(
            !args.iter().any(|arg| arg.contains("8790")),
            "unused WS must stay closed, got {args:?}"
        );
        assert!(
            !args.iter().any(|arg| arg.contains("BOX_EGRESS_TUNNEL")),
            "{args:?}"
        );
    }

    #[test]
    fn a_headless_box_never_publishes_the_egress_ws() {
        let args = DockerComputer::new()
            .with_image("debian:stable-slim")
            .with_egress_tunnel(true)
            .create_args(None)
            .expect("create args");
        assert!(!args.iter().any(|arg| arg.contains("8790")), "{args:?}");
        assert!(
            !args.iter().any(|arg| arg.contains("BOX_EGRESS_TUNNEL")),
            "{args:?}"
        );
    }

    #[test]
    fn egress_bearer_is_recovered_from_container_env_like_box_token() {
        let blob = "PATH=/usr/bin\nBOX_TOKEN=og-abc\nBOX_EGRESS_TUNNEL_BEARER=og-tunnel-secret-1\n";
        assert_eq!(
            env_from_inspect(blob, "BOX_TOKEN").as_deref(),
            Some("og-abc")
        );
        assert_eq!(
            env_from_inspect(blob, "BOX_EGRESS_TUNNEL_BEARER").as_deref(),
            Some("og-tunnel-secret-1")
        );
        assert_eq!(env_from_inspect(blob, "BOX_EGRESS_TUNNEL"), None);
    }

    #[test]
    fn a_headless_image_still_sleeps() {
        let args = DockerComputer::new()
            .with_image("debian:stable-slim")
            .create_args(None)
            .expect("create args");
        assert!(args.iter().any(|arg| arg == "sleep infinity"), "{args:?}");
        assert!(!args.iter().any(|arg| arg == "127.0.0.1::6080"));
    }

    #[test]
    fn a_box_keeps_its_data_in_named_volumes_it_is_labelled_with() {
        let volumes = BoxVolumes {
            home: "ogbox-abc-home".into(),
            workspace: "ogbox-abc-ws".into(),
        };
        let args = DockerComputer::new()
            .with_image("grok-box:local")
            .create_args_on(None, &volumes)
            .expect("create args");
        assert!(
            args.iter().any(|a| a == "ogbox-abc-ws:/workspace"),
            "{args:?}"
        );
        assert!(
            args.iter().any(|a| a == "ogbox-abc-home:/home/box"),
            "{args:?}"
        );
        assert!(
            args.iter()
                .any(|a| a == "dev.opengrok.volumes=ogbox-abc-home,ogbox-abc-ws"),
            "{args:?}"
        );
        assert_eq!(
            BoxVolumes::parse("ogbox-abc-home,ogbox-abc-ws"),
            Some(volumes)
        );
        assert_eq!(BoxVolumes::parse(""), None);

        // A headless box has no desktop user; only the workspace travels.
        let args = DockerComputer::new()
            .with_image("debian:stable-slim")
            .create_args_on(None, &BoxVolumes::fresh())
            .expect("create args");
        assert!(!args.iter().any(|a| a.ends_with(":/home/box")), "{args:?}");
        assert!(args.iter().any(|a| a.ends_with(":/workspace")), "{args:?}");
    }

    #[test]
    fn a_local_tag_is_never_pulled() {
        assert!(image_is_local("grok-box:local"));
        assert!(!image_is_local("ghcr.io/hexuria/box:latest"));
        assert!(!image_is_local("debian:stable-slim"));
    }

    #[test]
    fn the_one_time_copy_streams_the_old_dir_into_the_volume_as_the_box_user() {
        let command = DockerComputer::new()
            .with_image("grok-box:local")
            .copy_dir_command("abc123", "/home/box", "ogbox-x-home");
        assert!(command.starts_with("docker cp abc123:/home/box - | docker run --rm -i -u 0"));
        assert!(command.contains("-v ogbox-x-home:/dst grok-box:local"));
        assert!(command.contains("--strip-components=1"));
        assert!(command.contains("chown -R box:box /dst"));
        // Chromium's per-process locks would say "in use on another computer" on the new box.
        assert!(command.contains(r#"-name "Singleton*""#), "{command}");
    }

    /// The image's panel and launchers arrive where the volume has none; a rearranged panel
    /// is never overwritten (`cp -n`).
    #[test]
    fn the_desktop_defaults_are_seeded_without_overwriting() {
        let command = DockerComputer::new()
            .with_image("grok-box:local")
            .seed_defaults_command("ogbox-x-home");
        assert!(command.contains("-v ogbox-x-home:/dst grok-box:local"));
        assert!(
            command.contains("cp -rn /home/box/.config/. /dst/.config/"),
            "{command}"
        );
        assert!(command.contains("chown -R box:box /dst/.config"));
        // The desktop's config follows the image; the rest is only filled in.
        assert!(command.contains("rm -rf /dst/.config/xfce4"), "{command}");
        assert!(
            command.contains("cp -r /home/box/.config/xfce4 /dst/.config/xfce4"),
            "{command}"
        );
        // Every recreate, not only a copy: a reused volume holds the old container's locks.
        assert!(command.contains(r#"-name "Singleton*""#), "{command}");
    }

    #[test]
    fn docker_port_mapping_yields_the_host_port() {
        assert_eq!(host_port("127.0.0.1:58041"), Some(58041));
        assert_eq!(host_port("0.0.0.0:6080\n"), Some(6080));
        assert_eq!(host_port(""), None);
    }

    /// A port that was not published cannot be exposed later, and saying so beats handing back a
    /// URL that refuses every connection.
    #[tokio::test]
    async fn an_unpublished_port_is_refused_with_the_reason() {
        let error = DockerComputer::new()
            .expose_port("nonexistent", 9999, "app")
            .await
            .expect_err("should refuse");
        match error {
            BoxError::Refused { body, .. } => {
                assert!(body.contains("not published"), "{body}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }
}
