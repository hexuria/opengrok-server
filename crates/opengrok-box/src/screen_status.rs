//! What a Bot's screen is doing right now, as one word and one sentence, for the Computer pane and
//! the screen window to show over the viewer until its first picture arrives, instead of a black
//! box (hexuria/opengrok-server#386). The server reads it into `GET /coworkers/{id}/computer`'s
//! `screenStatus` and `screenStatusText`; everything here is decided from facts the server already
//! has, plus two kinds it did not keep before:
//!
//! - WORK IN PROGRESS ON A COMPUTER (`Busy`): a start, restart or reset runs inside the request or
//!   the tool call that asked for it, and nothing recorded that it was under way, so a second
//!   viewer (the separate screen window, or the pane when the Bot itself restarted its computer)
//!   saw only black. It is kept by the computer's scope, not its box id, which a reset changes.
//! - HOW FAR A BOT'S OWN SCREEN GOT (`ScreenProgress`): the provider's own record of its last
//!   `box-screen up`, so the status can say "starting its screen" without waiting for it, and the
//!   pane can say a Bot is on the shared screen because its computer is too old for own screens
//!   rather than claiming an own screen it does not have.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// How far a Bot's own screen on one box has come, as its provider last saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenProgress {
    /// Never asked for on this box (since this server started, or since the box changed).
    NotAsked,
    /// Being started now.
    Starting,
    /// Up, and the Bot works on it. Also what a provider without own screens answers.
    Given,
    /// The Bot works on the box's main screen instead: its image is from before own screens.
    OldImage,
    /// The Bot works on the box's main screen instead: the box already runs as many own screens as
    /// it has room for.
    Full,
}

/// Work on a computer that takes its screen away for a while.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Busy {
    Starting,
    Restarting,
    Resetting,
}

/// Past this a mark is taken to belong to work that died without clearing it (a panic, a server
/// killed mid-reset) and is not shown: an overlay that never lifts is worse than none.
const BUSY_STALE: Duration = Duration::from_secs(10 * 60);

static BUSY: LazyLock<Mutex<HashMap<String, (Busy, Instant)>>> = LazyLock::new(Mutex::default);

/// The key a computer's work is kept under: its scope, which outlives the box a reset replaces.
pub fn scope_key(scope: &str, scope_id: &str) -> String {
    format!("{scope}:{scope_id}")
}

/// Marks `key`'s computer as busy with `busy` until dropped. The work holds it across its awaits;
/// a later mark for the same computer replaces it, and dropping either clears whatever is there.
#[must_use = "the mark is cleared when this is dropped"]
pub struct BusyMark {
    key: String,
}

pub fn mark_busy(key: String, busy: Busy) -> BusyMark {
    if let Ok(mut marks) = BUSY.lock() {
        marks.retain(|_, (_, since)| since.elapsed() < BUSY_STALE);
        marks.insert(key.clone(), (busy, Instant::now()));
    }
    BusyMark { key }
}

impl Drop for BusyMark {
    fn drop(&mut self) {
        if let Ok(mut marks) = BUSY.lock() {
            marks.remove(&self.key);
        }
    }
}

/// What `key`'s computer is busy with now, if anything.
pub fn busy(key: &str) -> Option<Busy> {
    let marks = BUSY.lock().ok()?;
    let (busy, since) = marks.get(key)?;
    (since.elapsed() < BUSY_STALE).then_some(*busy)
}

/// What the status is decided from.
#[derive(Debug, Clone, Copy)]
pub struct Facts<'a> {
    /// The Bot's name, as the sentence calls it.
    pub name: &'a str,
    pub busy: Option<Busy>,
    /// An update's phase (`pulling`, `transferring`, `starting`, `failed`) and its reason.
    pub update: Option<(&'a str, Option<&'a str>)>,
    /// The computer's state: `running`, `starting`, `stopped`, `absent`, or another provider word.
    pub state: &'a str,
    /// The Bot's own screen, or `Given` for a Bot on the shared screen by design.
    pub screen: ScreenProgress,
    /// Whether there is a screen page to load (`vncUrl`).
    pub has_page: bool,
    /// The computer's error message, when the server holds one.
    pub error: Option<&'a str>,
}

/// The status word and its sentence. `ready` has no sentence: the screen itself is the answer.
/// The words: `ready`, `starting-computer`, `starting-screen`, `restarting`, `updating`,
/// `resetting`, `stopped`, `absent`, `failed`.
pub fn status(facts: &Facts) -> (&'static str, String) {
    let name = facts.name;
    match facts.busy {
        Some(Busy::Resetting) => return ("resetting", format!("Resetting {name}'s computer…")),
        Some(Busy::Restarting) => return ("restarting", format!("Restarting {name}'s computer…")),
        _ => {}
    }
    // A failed update leaves the old computer as it was: running, its screen is the answer, and
    // the pane's update row says what went wrong.
    let failed_on_a_running_computer =
        matches!(facts.update, Some(("failed", _))) && facts.state == "running";
    if let Some((phase, error)) = facts.update.filter(|_| !failed_on_a_running_computer) {
        return match phase {
            "failed" => {
                let why = error.unwrap_or("it stopped partway");
                (
                    "failed",
                    format!("{name}'s computer could not be updated: {why}"),
                )
            }
            "transferring" => (
                "updating",
                format!("Updating {name}'s computer: moving its files to the new one…"),
            ),
            "starting" => (
                "updating",
                format!("Updating {name}'s computer: starting the new one…"),
            ),
            _ => (
                "updating",
                format!("Updating {name}'s computer: getting the newest image…"),
            ),
        };
    }
    if facts.busy == Some(Busy::Starting) || facts.state == "starting" {
        return ("starting-computer", format!("Opening {name}'s computer…"));
    }
    match facts.state {
        "running" => {
            let up = facts.has_page
                && !matches!(
                    facts.screen,
                    ScreenProgress::NotAsked | ScreenProgress::Starting
                );
            if up {
                ("ready", String::new())
            } else {
                ("starting-screen", format!("Starting {name}'s screen…"))
            }
        }
        "stopped" => (
            "stopped",
            format!("{name}'s computer is shut down. It starts on its next turn, or with Start."),
        ),
        "absent" => match facts.error {
            Some(error) => ("failed", format!("{name} has no computer: {error}")),
            None => (
                "absent",
                format!("{name} has no computer yet. It gets one when it first needs it."),
            ),
        },
        _ => match facts.error {
            Some(error) => (
                "failed",
                format!("{name}'s computer cannot be reached: {error}"),
            ),
            None => (
                "failed",
                format!("{name}'s computer cannot be reached right now."),
            ),
        },
    }
}

/// An update older than this with no progress is a crashed one (the server's `UPDATE_STALE_AFTER_MS`),
/// and says nothing about the screen.
const UPDATE_STALE_AFTER_MS: i64 = 15 * 60 * 1000;

/// The phase and reason of the update record `GET …/computer` carries as `update`, when it still
/// says something: a failed one always, one in progress while it keeps moving.
pub fn update_of(update: &serde_json::Value, now_ms: i64) -> Option<(&str, Option<&str>)> {
    let phase = update.get("phase")?.as_str()?;
    let moved = update
        .get("updatedAtMs")
        .and_then(serde_json::Value::as_i64);
    let fresh = moved.is_some_and(|at| now_ms - at < UPDATE_STALE_AFTER_MS);
    let error = update.get("error").and_then(serde_json::Value::as_str);
    (phase == "failed" || fresh).then_some((phase, error))
}

/// Put `screenStatus` and `screenStatusText` on a `GET …/computer` body.
pub fn stamp_status(body: &mut serde_json::Value, facts: &Facts) {
    let (word, text) = status(facts);
    body["screenStatus"] = serde_json::json!(word);
    body["screenStatusText"] = serde_json::json!(text);
}

/// Put `screen` (which screen the Bot is on now) and, when it is not the one it would have,
/// `screenNote` saying why, on a `GET …/computer` body.
pub fn stamp_screen(
    body: &mut serde_json::Value,
    name: &str,
    asked_own: bool,
    progress: ScreenProgress,
) {
    body["screen"] = serde_json::json!(screen_word(asked_own, progress));
    if let Some(note) = screen_note(name, progress).filter(|_| asked_own) {
        body["screenNote"] = serde_json::json!(note);
    }
}

/// `GET /coworkers/{id}/computer`'s `screen`: which screen the Bot is on now. `own-pending` is a
/// Bot that would have its own screen but works on the shared one until its computer is updated.
pub fn screen_word(asked_own: bool, progress: ScreenProgress) -> &'static str {
    match (asked_own, progress) {
        (false, _) | (true, ScreenProgress::Full) => "shared",
        (true, ScreenProgress::OldImage) => "own-pending",
        (true, _) => "own",
    }
}

/// Why a Bot that would have its own screen is on the shared one, for the pane to say.
pub fn screen_note(name: &str, progress: ScreenProgress) -> Option<String> {
    match progress {
        ScreenProgress::OldImage => Some(format!(
            "{name}'s own screen starts after a computer update. Until then it works on the \
             shared screen."
        )),
        ScreenProgress::Full => Some(format!(
            "This computer already runs as many Bots' own screens as it has room for, so {name} \
             works on the shared screen."
        )),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "../tests/unit/screen_status.rs"]
mod tests;
