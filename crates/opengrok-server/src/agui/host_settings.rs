//! The host's settings record: defaults, the egress-tunnel intent flags, and (on this door)
//! GET/PUT `/ag-ui/host-settings`.
//!
//! These lived under `gateway/` because the desktop verbs `getHostSettings` / `setHostSettings`
//! / `isEgressTunnelAvailable` minted the record. NativeChat speaks AG-UI, so the defaults and
//! the intent flags belong next to that door. Seam A still re-exports them for the one commit
//! it has left to live.

use serde_json::json;

/// The `getHostSettings` record the client reads back — every field from
/// `client-grok-bot.md` §9, with the defaults the shipped host starts from. Fields the shape
/// marks "omitted when undefined" are omitted.
pub fn default_settings() -> serde_json::Value {
    json!({
        "notifications": {
            "isEnabled": false, "allowedApps": [], "minIntervalMs": 5000,
            "maxPerWindow": 10, "windowMs": 300_000
        },
        "mcpCustomInstructions": {},
        "mcpCustomInstructionsByServerId": {},
        "mcpDisabledToolsByServerId": {},
        "mcpBoxServers": [],
        "autoReviewInstructions": null,
        "localToolPermission": null,
        "webauthnProxyEnabled": false,
        // Ours, not Cursor's — but the field must exist, the resync chain reads the record whole.
        "inferenceProvider": "cursor",
        "inferenceRouterUsage": null,
        "sidebarSections": [],
        "hasSeenOnboarding": true,
        // User-network tunnel. Default off: docker host-network is not the prod path.
        // Host intent only. `isEgressTunnelAvailable` is this flag (or env) AND
        // `/v1/info` `capabilities.egress_tunnel.ready`. No box / failed info → false
        // so NativeChat does not paint the toggle live until a client is attached.
        "egressTunnelEnabled": false
    })
}

/// Grok host: `process.env.SAND_EGRESS_TUNNEL_ENABLED === "1"`. OpenGrok also honors
/// `OG_EGRESS_TUNNEL_ENABLED`. The in-app toggle is `egressTunnelEnabled`. Any one is
/// host intent — not yet "the laptop client is attached".
#[must_use]
pub fn egress_tunnel_from(
    settings: &serde_json::Value,
    og_env: Option<&str>,
    sand_env: Option<&str>,
) -> bool {
    og_env == Some("1")
        || sand_env == Some("1")
        || settings
            .get("egressTunnelEnabled")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
}

#[must_use]
pub fn egress_tunnel_available(settings: &serde_json::Value) -> bool {
    egress_tunnel_from(
        settings,
        std::env::var("OG_EGRESS_TUNNEL_ENABLED").ok().as_deref(),
        std::env::var("SAND_EGRESS_TUNNEL_ENABLED").ok().as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn egress_tunnel_is_off_by_default() {
        let settings = default_settings();
        assert_eq!(settings["egressTunnelEnabled"], false);
        assert!(!egress_tunnel_from(&settings, None, None));
    }

    #[test]
    fn egress_tunnel_env_or_setting_turns_it_on() {
        let settings = default_settings();
        assert!(egress_tunnel_from(&settings, Some("1"), None));
        assert!(egress_tunnel_from(&settings, None, Some("1")));
        assert!(
            !egress_tunnel_from(&settings, Some("true"), None),
            "Grok host parity is strictly === \"1\""
        );
        assert!(egress_tunnel_from(
            &json!({ "egressTunnelEnabled": true }),
            None,
            None
        ));
    }

    #[test]
    fn advertised_needs_the_box_ready_when_info_is_present() {
        let ready = opengrok_box::EgressTunnel {
            enabled: true,
            ready: true,
        };
        let waiting = opengrok_box::EgressTunnel {
            enabled: true,
            ready: false,
        };
        assert!(!opengrok_box::EgressTunnel::advertised(true, None));
        assert!(opengrok_box::EgressTunnel::advertised(true, Some(ready)));
        assert!(!opengrok_box::EgressTunnel::advertised(true, Some(waiting)));
        assert!(!opengrok_box::EgressTunnel::advertised(false, Some(ready)));
    }
}
