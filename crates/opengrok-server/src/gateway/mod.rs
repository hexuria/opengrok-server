//! What is left of the gateway: the state the surviving doors share.
//!
//! This module was seam A — the JSON+SSE door the discontinued Electron client lived on. The door
//! is gone (`POST /api/{method}`, `GET /events`, `/avatars/{id}`, the live bus, the rooms). What
//! stays is the machinery AG-UI, the MCP door, the hooks and the autonomy loops reach for through
//! it: the conversation's suspension/resume path, user forms, credential prompts, the cards, and
//! the host settings record.
//!
//! `GatewayState` KEEPS ITS NAME for now. Renaming it would touch every test that constructs one
//! and say nothing new; it is a follow-up, not this deletion.

pub mod cards;
pub mod conversation;
pub mod credential;
pub mod hooks;
pub mod lifecycle;
pub mod user_form;

use std::sync::{Arc, Mutex};

use crate::agui::routes::AgUiState;

/// What the gateway knows beyond the shared server state.
#[derive(Clone)]
pub struct GatewayState {
    pub agui: AgUiState,
    /// The host settings record — the same `Arc` `AgUiState.host_settings` holds, so
    /// `GET/PUT /ag-ui/host-settings` and everything that reads a setting read one record.
    pub settings: Arc<Mutex<serde_json::Value>>,
    /// When this process started — `/health`'s `startedAt`.
    pub started_at_ms: i64,
    /// The address a client is handed for this host. `None` means we do not invent one.
    pub public_gateway_url: Option<String>,
}

impl GatewayState {
    pub fn new(mut agui: AgUiState, public_gateway_url: Option<String>) -> Self {
        let settings = agui
            .host_settings
            .clone()
            .unwrap_or_else(|| Arc::new(Mutex::new(default_settings())));
        agui.host_settings = Some(settings.clone());
        Self {
            agui,
            public_gateway_url,
            settings,
            started_at_ms: chrono::Utc::now().timestamp_millis(),
        }
    }
}

/// The host settings record, with the defaults this server starts from. Its shape is the one
/// `GET/PUT /ag-ui/host-settings` answers, and it is what every reader of a setting merges into.
pub fn default_settings() -> serde_json::Value {
    serde_json::json!({
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
