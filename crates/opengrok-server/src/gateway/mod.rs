//! Seam A: the gateway the desktop client actually lives on.
//!
//! This is the surface `docs/PORT-PRIORITY.md` measured — 123 JSON+SSE commands, not protobuf —
//! and this module is its slice 7: enough for the real, unmodified Grok Bot app to boot against
//! us (`/health`, `/events`, and the roster/settings commands), pointed here by nothing more than
//! `SAND_HOST_GATEWAY_URL`.
//!
//! EVERY WIRE RULE HERE IS TRANSCRIBED, NOT DESIGNED. Shapes, headers, status codes and timings
//! come from `docs/research/client-grok-bot.md` §2.0 and §9, each read from the client's own
//! source. Where this module has an opinion the client does not check, it keeps none.
//!
//! THE AUTH MODEL IS THE SHIPPED HOST'S. One shared bearer, compared in constant time; any
//! request carrying an `Origin` header is refused (a browser page must never be able to drive
//! the gateway, even with the token); and with no token configured, only loopback `Host`s are
//! served — exactly `gateway-server.ts`'s posture, so a deployment that forgets the token fails
//! closed instead of open.

pub mod conversation;
pub mod live;
pub mod routes;
pub mod summaries;

use std::sync::{Arc, Mutex};

use subtle::ConstantTimeEq;

use crate::agui::routes::AgUiState;

/// What the gateway knows beyond the shared server state.
#[derive(Clone)]
pub struct GatewayState {
    pub agui: AgUiState,
    /// The shared bearer, compared timing-safe. `None` = loopback-only, the shipped default.
    pub bearer: Option<String>,
    /// Whose coworkers this gateway serves as its roster. The desktop is a one-person surface;
    /// this names the person.
    pub email: String,
    /// Host settings, echoed back the way the resync chain expects. In memory on purpose: the
    /// client rewrites every field of interest on every `transport-connected`, so persisting
    /// them would only preserve values the next connect immediately overwrites.
    pub settings: Arc<Mutex<serde_json::Value>>,
    /// When this process started — `/health`'s `startedAt`.
    pub started_at_ms: i64,
    /// The live stream: every SSE frame goes through here, and each `/events` subscriber holds
    /// a receiver. Slow subscribers lag and are dropped by `broadcast`'s own rules — a stalled
    /// client must not be able to wedge the host.
    pub events_tx: tokio::sync::broadcast::Sender<(String, serde_json::Value)>,
    /// This process's ordering epoch. A client that sees the epoch change treats the replica as
    /// restarted and resyncs — which is exactly what a restart means.
    pub epoch: String,
    /// Monotonic sequence per replica key (`roster`, `transcript:<agentId>`), for the `ordered`
    /// stamp every roster and transcript event carries.
    pub seqs: Arc<Mutex<std::collections::HashMap<String, i64>>>,
    /// The address `EnsureSandBox` hands out — the mint. Non-loopback or the client refuses it;
    /// `None` means the mint answers failed_precondition rather than inventing an address.
    pub public_gateway_url: Option<String>,
    /// Which agent the client last opened. `getTranscript` and `sendPrompt` fall back to it.
    pub active_agent: Arc<Mutex<Option<String>>>,
    /// Coworkers with a turn in flight right now — the roster's `isRunning`.
    pub running: Arc<Mutex<std::collections::HashSet<String>>>,
}

impl GatewayState {
    pub fn new(
        agui: AgUiState,
        bearer: Option<String>,
        email: String,
        public_gateway_url: Option<String>,
    ) -> Self {
        Self {
            agui,
            bearer,
            email,
            public_gateway_url,
            settings: Arc::new(Mutex::new(default_settings())),
            started_at_ms: chrono::Utc::now().timestamp_millis(),
            events_tx: tokio::sync::broadcast::channel(256).0,
            epoch: uuid::Uuid::now_v7().to_string(),
            seqs: Arc::new(Mutex::new(std::collections::HashMap::new())),
            active_agent: Arc::new(Mutex::new(None)),
            running: Arc::new(Mutex::new(std::collections::HashSet::new())),
        }
    }
}

/// The `getHostSettings` record the client reads back — every field from
/// `client-grok-bot.md` §9, with the defaults the shipped host starts from. Fields the shape
/// marks "omitted when undefined" are omitted.
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
        "hasSeenOnboarding": true
    })
}

/// The gateway's whole access decision, in the order the shipped host applies it.
///
/// Returns the refusal to send, or `None` for "come in".
pub fn refuse(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
) -> Option<(u16, &'static str)> {
    // A browser origin is refused before anything else — with or without the token. A web page
    // that has somehow learned the bearer must still not be able to drive a desktop's gateway.
    if headers.get(axum::http::header::ORIGIN).is_some() {
        return Some((403, "browser origins are not served"));
    }

    match &state.bearer {
        Some(expected) => {
            let presented = headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "))
                .unwrap_or_default();
            // Constant-time on the bytes; the length leak is unavoidable and harmless for a
            // random token.
            let matches = presented.len() == expected.len()
                && presented.as_bytes().ct_eq(expected.as_bytes()).into();
            if matches {
                None
            } else {
                Some((401, "bad token"))
            }
        }
        None => {
            // No token pinned: loopback hosts only, exactly the shipped host's fallback. This is
            // what keeps "forgot to configure auth" from meaning "open on the LAN".
            let host = headers
                .get(axum::http::header::HOST)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();
            let name = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
            let loopback = matches!(name, "127.0.0.1" | "localhost" | "[::1]" | "::1");
            if loopback {
                None
            } else {
                Some((403, "no gateway token is configured; loopback only"))
            }
        }
    }
}
