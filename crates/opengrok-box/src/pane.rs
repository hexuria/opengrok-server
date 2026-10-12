//! Pieces of the Computer pane's `GET /coworkers/{id}/computer` body and of the server's computer
//! work that need no server: which scope a Bot's computer lives under and the names the app places
//! it by, the egress fields it paints from, and waiting for a screen. Moved here from
//! opengrok-server `agui/provision.rs`, which still re-exports `scope_for` and `share_scope_of`.

use opengrok_core::coworker::BoxMode;
use serde_json::{Value, json};

use crate::{Computer, EgressTunnel};

/// The (scope, scope_id, box mode) a mode maps to: per-org shares one org box, per-account one box
/// per member, per-bot a dedicated box each. An account with no org falls back to account scope.
pub fn scope_for(
    mode: &str,
    account_id: &str,
    org_id: Option<&str>,
    coworker_id: &str,
    group: bool,
) -> (&'static str, String, BoxMode) {
    // A group has a computer of its own whatever the sharing mode: the room's shared desk, which
    // every member can reach next to its own box.
    if group {
        return ("group", coworker_id.to_string(), BoxMode::Shared);
    }
    match mode {
        "per-org" => match org_id {
            Some(org) => ("org", org.to_string(), BoxMode::Shared),
            None => ("account", account_id.to_string(), BoxMode::Shared),
        },
        "per-bot" => ("bot", coworker_id.to_string(), BoxMode::Dedicated),
        _ => ("account", account_id.to_string(), BoxMode::Shared),
    }
}

/// `shareScope`, and `groupId` for a group's desk, on a `GET …/computer` body.
pub fn stamp_share_scope(screen: &mut Value, scope: &str, scope_id: &str) {
    screen["shareScope"] = json!(share_scope_of(scope));
    if scope == "group" {
        screen["groupId"] = json!(scope_id);
    }
}

/// NativeChat (tip `729bdd9`) gates Route traffic from **only** this JSON —
/// `isEgressTunnelAvailable` or nested `egress_tunnel.ready`. It does not call Box `/v1/info`.
/// Probe the scoped live `boxId` already in the payload (never a frozen coworker-row id) on every
/// status read, bar a guest just silent (`shown_egress`). Do not invent `ready`.
pub fn stamp_egress_fields(screen: &mut Value, host_wants: bool, cap: Option<EgressTunnel>) {
    screen["isEgressTunnelAvailable"] = json!(EgressTunnel::advertised(host_wants, cap));
    if let Some(cap) = cap {
        screen["egress_tunnel"] = json!({
            "enabled": cap.enabled,
            "ready": cap.ready,
        });
    }
}

/// NativeChat places Route traffic chrome by computer **share**, not on every
/// bot pane. Internal `scope_for` stays `bot|account|group|org`; sharing-mode
/// policy stays `per-bot|per-account|per-org`. This is the client-facing
/// placement name (`dedicated` = bot sidebar, `user` = Settings→Computer,
/// `group` = group sidebar, `org` = admin console).
pub fn share_scope_of(scope: &str) -> &'static str {
    match scope {
        "bot" => "dedicated",
        "account" => "user",
        "group" => "group",
        "org" => "org",
        // Unknown internal scope must not paint dedicated bot-pane chrome.
        _ => "user",
    }
}

/// Poll until the box's screen is reachable or patience runs out; the caller reports either way.
pub async fn wait_for_screen(provider: &dyn Computer, box_id: &str, patience: std::time::Duration) {
    let started = std::time::Instant::now();
    loop {
        if let Ok(Some(_)) = provider.screen_url(box_id, &crate::Screen::Shared).await {
            return;
        }
        if started.elapsed() >= patience {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}
