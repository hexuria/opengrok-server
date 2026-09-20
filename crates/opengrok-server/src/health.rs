//! `GET /health` — is this server up, and is it doing anything.
//!
//! It lived in the desktop client's JSON door because that client's supervisor was its most
//! demanding reader. The client is gone and the door with it; the probe is not — `scripts/gate.sh`,
//! `scripts/serve.sh`, every smoke script and any process supervisor in front of this binary wait
//! on it before they do anything else, and all of them read exactly one field: `ok === true`.
//! THE REPLY SHAPE IS THEREFORE FROZEN, not because a client compiles against it, but because a
//! dozen scripts parse it and none of them would say why they had stopped waiting.

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};

use crate::gateway::GatewayState;

pub fn router(state: GatewayState) -> Router {
    Router::new()
        .route("/health", get(health))
        .with_state(state)
}

/// A JSON reply, stamped the way the client checks: `x-sand-mint-dedupe: 1` on every one.
pub(crate) fn reply(status: StatusCode, body: Value) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::HeaderName::from_static("x-sand-mint-dedupe"), "1"),
        ],
        body.to_string(),
    )
        .into_response()
}

pub(crate) fn refusal(code: u16, message: &str) -> Response {
    reply(
        StatusCode::from_u16(code).unwrap_or(StatusCode::FORBIDDEN),
        json!({ "error": message }),
    )
}

/// `GET /health`. The busy flag is real: it reports whether any run is live right now, and so is
/// `ok` — a host whose event store is unreachable answers 503, not a cheerful 200.
///
/// UNAUTHENTICATED, on purpose. A liveness endpoint whose whole job is to answer "I am up" must be
/// probeable without a token, or a deployment that pins one looks permanently unreachable to its
/// own supervisor. It reveals only that the server is up (plus pid/busy/startedAt — nothing
/// secret).
///
/// Token-free is not origin-free. A probe is a process, not a page; a browser page that learned
/// this host still gets nothing — not even "up" — which is the rule the gateway smoke asserted for
/// every path on this server and the rule this endpoint keeps now that it is the last one here.
async fn health(State(state): State<GatewayState>, headers: HeaderMap) -> Response {
    if headers.get(axum::http::header::ORIGIN).is_some() {
        return refusal(403, "browser origins are not served");
    }
    // A store that cannot answer means this host cannot serve a single request, so `/health` must
    // not report that it can. This was `.unwrap_or(false)`, which folded a TOTAL database outage
    // into "not busy" and still replied `ok: true` — on the one endpoint a supervisor probes to
    // decide whether this host is still worth using.
    let busy = match state.agui.auth.store.running_runs().await {
        Ok(live) => live > 0,
        Err(error) => {
            tracing::error!(%error, "health: the event store is not answering");
            return reply(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({
                    "ok": false,
                    // The store's own sentence can carry a connection string, and this endpoint is
                    // unauthenticated. The log gets the error; the reply gets a fixed sentence.
                    "reason": "the event store is not answering",
                    "pid": std::process::id(),
                    "startedAt": state.started_at_ms,
                }),
            );
        }
    };
    reply(
        StatusCode::OK,
        json!({
            "ok": true,
            "pid": std::process::id(),
            "isBusy": busy,
            "activeAgentId": null,
            "startedAt": state.started_at_ms,
            "lastBusyAtMs": state.started_at_ms,
        }),
    )
}
