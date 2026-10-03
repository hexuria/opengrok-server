//! The reverse-exec permission gate — the safety core of the channel that runs commands on the
//! USER'S OWN machine (their Mac), not a disposable box.
//!
//! Built GATE-FIRST and on its own: this is pure decision logic with no transport, no daemon and no
//! way to run anything, so the rules can be proven closed-by-default before a single command can
//! flow. A Claude-Code-style model (Uriah's call): a per-machine `mode`, plus an allowlist and a
//! denylist of command patterns added on demand.
//!
//! CLOSED BY DEFAULT. The default mode is `Never` (the channel is off), an unknown command in `Ask`
//! mode is `Ask` (a person decides, never a silent yes), and deny always beats allow. The only path
//! to an automatic yes is an explicit allowlist rule under `Ask`, or the deliberately-enabled
//! `Bypass`. See `docs/archive/reverse-exec-design.md`.
//!
//! The server's routes, its reads of the stored rules and the transport to the daemon are
//! `opengrok-server`'s `local_exec`, which re-exports this: the gate is what a person's consent
//! lets a coworker run, judged with the shell reader beside it (`shell`).

use serde::{Deserialize, Serialize};

use crate::shell;

/// The consent mode for ONE machine's reverse-exec channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LocalExecMode {
    /// The channel is OFF. Every command is denied. This is the default until the user turns it on.
    #[default]
    Never,
    /// Consult the lists: deny-match denies, allow-match allows, anything else asks a person.
    Ask,
    /// Allow everything, skipping the lists — a deliberate, machine-wide choice, like Claude Code's
    /// bypass. Still audited (every command is logged, even here).
    Bypass,
}

/// A machine's reverse-exec permission policy. Absent ⇒ the default (`Never`, no rules).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LocalExecPolicy {
    pub mode: LocalExecMode,
    /// Command patterns that auto-ALLOW under `Ask` (added on demand: "always allow").
    #[serde(default)]
    pub allow: Vec<String>,
    /// Command patterns that auto-DENY under `Ask` (added on demand: "always deny").
    #[serde(default)]
    pub deny: Vec<String>,
    /// Allows that live only in this process. Dropped on restart. Not in GET /policy.
    #[serde(default, skip)]
    pub session_allow: Vec<String>,
}

/// The gate's verdict for one command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalExecDecision {
    /// Run it automatically (an allowlist rule, or `Bypass`).
    Allow,
    /// Refuse it. Never runs. `why` is what the model and the person are told; `rule` is the deny
    /// rule that matched, for the audit row only (#224) — `None` when no rule did (mode `never`,
    /// a line too long to read).
    Deny { why: String, rule: Option<String> },
    /// Suspend — a person decides for THIS command. Never treated as a yes.
    Ask,
}

/// The raw-text prefix match the gate used before it read shell syntax: equal, or `pattern` plus a
/// space. Kept ONLY as a second way for a deny rule to match, so reading the line more closely can
/// never make a stored deny (`curl x | sh`) refuse less than it did. Never used for allow: on the
/// raw line it allowed `ls; rm -rf ~` under a rule of `ls` (#204).
fn matches(pattern: &str, command: &str) -> bool {
    let pattern = pattern.trim();
    let command = command.trim();
    if pattern.is_empty() {
        return false;
    }
    command == pattern || command.starts_with(&format!("{pattern} "))
}

/// The first word of a command pattern, after a path prefix (`/usr/bin/sudo`,
/// `C:\Windows\System32\sudo.exe`). Used only to decide whether a standing
/// allow is forbidden; matching at run time is `decide`.
fn first_command(pattern: &str) -> &str {
    let token = pattern.split_whitespace().next().unwrap_or("");
    token.rsplit(['/', '\\']).next().unwrap_or(token)
}

/// Whether this standing rule may be persisted. Deny is never refused here —
/// remembering "never run sudo" is a safety net. Allow of `sudo` (and
/// `sudo.exe`, any path, any arguments) is refused, because a standing allow
/// on `sudo` would silently cover `sudo rm -rf /`. So is an allow that is not
/// one plain command (`cd src && cargo test`): the gate would never match it,
/// and storing it inert hides that from the person who asked for it (#203).
///
/// The one writer of standing rules is `POST /local-exec/policy/rule`. The
/// AG-UI card's answer is only `approved: bool` (`agui::routes::AnswerRequest`)
/// and writes no rule; a client's Always/Never must post to that endpoint, so
/// any future writer goes through this function too. The store writes rows,
/// never policy.
pub fn standing_rule_refusal(kind: &str, pattern: &str) -> Option<&'static str> {
    if kind != "allow" {
        return None;
    }
    let command = first_command(pattern);
    if command.eq_ignore_ascii_case("sudo") || command.eq_ignore_ascii_case("sudo.exe") {
        Some("sudo cannot be a standing allow")
    } else if shell::read(pattern).plain.is_none() {
        Some(
            "an allow rule must be one plain command: no ; && || | & or newline, no $( ) or \
             backticks, no redirection to a path, no VAR= in front, and not a program that runs \
             another (sh, eval, env, sudo, xargs…)",
        )
    } else {
        None
    }
}

/// The longest line `Ask` reads. The gate runs on every Ask-mode call, twice per bot tool call,
/// and a line reaches it from a model's tool arguments or a 2 MB request body, so the reading
/// must have a bound one call cannot push the server past. A longer line is refused, never
/// asked about: on the user's own path an Ask runs it with no deny rule read.
pub const MAX_JUDGED_BYTES: usize = 64 * 1024;

/// What the model is told when a deny rule refuses a command. See `decide`.
pub const DENIED_BY_A_RULE: &str = "a deny rule on this computer refused this command, so it did \
    not run. Do not reword it to get past the rule; tell the person what you needed it for.";

/// THE GATE. The one place a command on the user's own machine is judged. Everything that would run
/// a reverse-exec command MUST pass through here first, on the server, before anything is queued.
///
/// - `Never` (default): deny, always.
/// - `Bypass`: allow (the lists are skipped by the user's deliberate choice; still audited).
/// - `Ask`: a line over `MAX_JUDGED_BYTES` is denied unread. A deny rule matching ANY simple
///   command in the line denies (deny wins); else an allow or session-allow rule covering the
///   line allows, and only a line that is ONE plain simple command can be covered; else ask.
///   See `shell` for how the line is read.
pub fn decide(policy: &LocalExecPolicy, command: &str) -> LocalExecDecision {
    match policy.mode {
        LocalExecMode::Never => LocalExecDecision::Deny {
            why: "this machine's reverse-exec channel is off (mode: never) — turn it on to run commands here".to_string(),
            rule: None,
        },
        LocalExecMode::Bypass => LocalExecDecision::Allow,
        LocalExecMode::Ask => {
            if command.len() > MAX_JUDGED_BYTES {
                return LocalExecDecision::Deny {
                    why: format!(
                        "this command is {} bytes; the gate reads at most {MAX_JUDGED_BYTES} \
                         before it decides, and refuses a longer one rather than run it unread — \
                         split it into shorter commands",
                        command.len()
                    ),
                    rule: None,
                };
            }
            let line = shell::read(command);
            let denied = if policy.deny.is_empty() {
                None
            } else {
                let runs = shell::programs(command, line.plain.as_deref());
                policy.deny.iter().find(|pattern| {
                    matches(pattern, command) || shell::denies(pattern, &runs)
                })
            };
            let allowed = || match &line.plain {
                Some(words) => policy
                    .allow
                    .iter()
                    .chain(policy.session_allow.iter())
                    .any(|pattern| shell::allows(pattern, words)),
                None => false,
            };
            // THE MODEL IS NOT TOLD WHICH RULE (#224). Naming it handed the model the exact words
            // to rephrase around, and a deny rule is a string match, not a sandbox. The refusal
            // still reaches it as a result it can reason about (non-negotiable 8): what happened,
            // and what to do instead. The rule goes on the audit row, where the person reads it.
            if let Some(pattern) = denied {
                LocalExecDecision::Deny {
                    why: DENIED_BY_A_RULE.to_string(),
                    rule: Some(pattern.trim().to_string()),
                }
            } else if allowed() {
                LocalExecDecision::Allow
            } else {
                LocalExecDecision::Ask
            }
        }
    }
}

/// The simple commands in `line`, as written, for the daemon's `simpleCommands`: the server's own
/// split, so the list a machine's local approval sees is the line the gate read (#203).
pub fn simple_commands(line: &str) -> Vec<String> {
    shell::read(line).simple_commands
}

impl LocalExecMode {
    /// From the stored word; anything unrecognised (or absent) is the closed default, `Never`.
    pub fn from_stored(mode: &str) -> Self {
        match mode {
            "ask" => Self::Ask,
            "bypass" => Self::Bypass,
            _ => Self::Never,
        }
    }

    pub fn as_stored(&self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::Ask => "ask",
            Self::Bypass => "bypass",
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/local_exec.rs"]
mod tests;
