//! Whether a coworker's leave-box tools may use the person's network, and how that answer is
//! stored. Lives here because it is a decision policy, not a tool: opengrok-tools consumes it,
//! the server stores and reads it.

/// Whether leave-box tools (`computer`, `open_url`, `run_recipe`) raise the Review-an-action card
/// for the egress tunnel before they run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EgressTunnelMode {
    /// No tunnel: nothing to ask about.
    Off,
    /// The box's guest said the tunnel is up with a client attached: ask before the first leave-box
    /// tool.
    On,
    /// The host wants the tunnel but the box was asleep when the turn started, so the guest could
    /// not be asked. Ask it right after the first leave-box tool wakes the box, and raise the card
    /// only if it says the tunnel is really there — a card that asserts a tunnel that is not
    /// attached would be a lie.
    AskTheBoxAfterWake,
}

/// The person's standing answer, per computer, to the tunnel's Review-an-action card. Stored as
/// the reverse-exec channel's words (`bypass` | `ask` | `never`) so one vocabulary serves both.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum EgressPolicy {
    /// Never raise the tunnel card for this computer: every leave-box action may use the
    /// person's network.
    Always,
    /// One card per run, as before. What an unset policy means.
    #[default]
    Ask,
    /// This computer may not use the person's network: while the tunnel is on for it, the
    /// leave-box tools are not offered at all, and one that arrives anyway is refused.
    Never,
}

impl EgressPolicy {
    /// The stored word; anything unknown reads as `Ask`, which is what no row means too.
    pub fn from_stored(mode: &str) -> Self {
        match mode {
            "bypass" => Self::Always,
            "never" => Self::Never,
            _ => Self::Ask,
        }
    }

    pub fn as_stored(self) -> &'static str {
        match self {
            Self::Always => "bypass",
            Self::Ask => "ask",
            Self::Never => "never",
        }
    }

    pub fn is_valid(mode: &str) -> bool {
        matches!(mode, "bypass" | "ask" | "never")
    }
}

/// What a leave-box tool answers when the computer's network use is switched off.
pub const NETWORK_OFF: &str =
    "this computer's use of the person's network is switched off, so it cannot reach the web";
