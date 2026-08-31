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
//! `Bypass`. See `docs/reverse-exec-design.md`.

use serde::{Deserialize, Serialize};

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
}

/// The gate's verdict for one command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalExecDecision {
    /// Run it automatically (an allowlist rule, or `Bypass`).
    Allow,
    /// Refuse it, with a human-readable reason. Never runs.
    Deny(String),
    /// Suspend — a person decides for THIS command. Never treated as a yes.
    Ask,
}

impl LocalExecDecision {
    pub fn is_allow(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// Does `pattern` match `command`? Prefix match on a WORD BOUNDARY — `pattern` matches `command`
/// when they are equal or `command` begins with `pattern` followed by a space. So `git status`
/// matches `git status --short` but not `git statusx`, and a broad `git` matches `git anything`.
/// Both sides are whitespace-trimmed first. Deliberately simple and conservative: a rule can only
/// widen to whole extra arguments, never to a different command that merely shares a prefix.
fn matches(pattern: &str, command: &str) -> bool {
    let pattern = pattern.trim();
    let command = command.trim();
    if pattern.is_empty() {
        return false;
    }
    command == pattern || command.starts_with(&format!("{pattern} "))
}

/// THE GATE. The one place a command on the user's own machine is judged. Everything that would run
/// a reverse-exec command MUST pass through here first, on the server, before anything is queued.
///
/// - `Never` (default): deny, always.
/// - `Bypass`: allow (the lists are skipped by the user's deliberate choice; still audited).
/// - `Ask`: a denylist match denies (deny wins), else an allowlist match allows, else ask.
pub fn decide(policy: &LocalExecPolicy, command: &str) -> LocalExecDecision {
    match policy.mode {
        LocalExecMode::Never => LocalExecDecision::Deny(
            "this machine's reverse-exec channel is off (mode: never) — turn it on to run commands here".to_string(),
        ),
        LocalExecMode::Bypass => LocalExecDecision::Allow,
        LocalExecMode::Ask => {
            if policy.deny.iter().any(|pattern| matches(pattern, command)) {
                LocalExecDecision::Deny("a deny rule matched this command".to_string())
            } else if policy.allow.iter().any(|pattern| matches(pattern, command)) {
                LocalExecDecision::Allow
            } else {
                LocalExecDecision::Ask
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(mode: LocalExecMode, allow: &[&str], deny: &[&str]) -> LocalExecPolicy {
        LocalExecPolicy {
            mode,
            allow: allow.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn default_is_closed() {
        // The default policy (no config at all) denies everything — the channel is off.
        assert!(matches!(
            decide(&LocalExecPolicy::default(), "echo hi"),
            LocalExecDecision::Deny(_)
        ));
    }

    #[test]
    fn never_denies_even_an_allowlisted_command() {
        // Mode is the baseline: Never denies regardless of the lists.
        let p = policy(LocalExecMode::Never, &["echo"], &[]);
        assert!(matches!(decide(&p, "echo hi"), LocalExecDecision::Deny(_)));
    }

    #[test]
    fn ask_with_no_rules_asks() {
        let p = policy(LocalExecMode::Ask, &[], &[]);
        assert_eq!(decide(&p, "echo hi"), LocalExecDecision::Ask);
    }

    #[test]
    fn ask_allowlist_allows_on_word_boundary_only() {
        let p = policy(LocalExecMode::Ask, &["git status"], &[]);
        assert_eq!(decide(&p, "git status"), LocalExecDecision::Allow);
        assert_eq!(decide(&p, "git status --short"), LocalExecDecision::Allow);
        // A shared prefix that is NOT a word boundary must not match — closed by default.
        assert_eq!(decide(&p, "git statusx"), LocalExecDecision::Ask);
        assert_eq!(decide(&p, "git log"), LocalExecDecision::Ask);
    }

    #[test]
    fn ask_denylist_denies() {
        let p = policy(LocalExecMode::Ask, &[], &["rm"]);
        assert!(matches!(decide(&p, "rm -rf /"), LocalExecDecision::Deny(_)));
    }

    #[test]
    fn deny_wins_over_allow() {
        // The same command both allowed and denied: deny wins.
        let p = policy(LocalExecMode::Ask, &["sudo rm"], &["sudo"]);
        assert!(matches!(
            decide(&p, "sudo rm -rf /"),
            LocalExecDecision::Deny(_)
        ));
    }

    #[test]
    fn bypass_allows_everything() {
        // Bypass is the user's deliberate "allow all" — even a command that would be denylisted.
        let p = policy(LocalExecMode::Bypass, &[], &["rm"]);
        assert_eq!(decide(&p, "rm -rf /"), LocalExecDecision::Allow);
        assert_eq!(decide(&p, "anything at all"), LocalExecDecision::Allow);
    }

    #[test]
    fn an_empty_pattern_never_matches() {
        // A blank rule must not become a wildcard that allows or denies everything.
        assert!(!matches("", "echo hi"));
        assert!(!matches("   ", "echo hi"));
        let p = policy(LocalExecMode::Ask, &[""], &[]);
        assert_eq!(decide(&p, "echo hi"), LocalExecDecision::Ask);
    }
}
