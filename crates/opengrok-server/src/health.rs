//! `/health`, the liveness probe. Every smoke waits on it, `scripts/gate.sh` and `serve.sh` poll
//! it, and a deployment's supervisor decides from it whether to keep using this host.
//!
//! `ok` is the whole contract. Readers treat a non-2xx and an `ok !== true` body the same way, so
//! the reply may say more but must never say `ok: true` when it cannot serve.

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::json;

use crate::agui::AgUiState;

#[derive(Clone)]
struct HealthState {
    agui: AgUiState,
    /// When this process started. A reader that sees it change knows the host restarted.
    started_at_ms: i64,
}

pub fn router(agui: AgUiState) -> Router {
    Router::new()
        .route("/health", get(health))
        .with_state(HealthState {
            agui,
            started_at_ms: chrono::Utc::now().timestamp_millis(),
        })
}

async fn health(State(state): State<HealthState>) -> Response {
    // A store that cannot answer means this host cannot serve a single request, so `/health` must
    // not report that it can. This was `.unwrap_or(false)`, which folded a TOTAL database outage
    // into "not busy" and still replied `ok: true` on the one endpoint a supervisor probes to
    // decide whether to keep using this host. Failing closed is cheap here.
    let busy = match state.agui.auth.store.running_runs().await {
        Ok(live) => live > 0,
        Err(error) => {
            tracing::error!(%error, "health: the event store is not answering");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(json!({
                    "ok": false,
                    // The store's own sentence can carry a connection string, and this endpoint
                    // is unauthenticated. The log gets the error; the reply gets a fixed sentence.
                    "reason": "the event store is not answering",
                    "pid": std::process::id(),
                    "startedAt": state.started_at_ms,
                })),
            )
                .into_response();
        }
    };
    axum::Json(json!({
        "ok": true,
        "pid": std::process::id(),
        "isBusy": busy,
        "startedAt": state.started_at_ms,
    }))
    .into_response()
}
