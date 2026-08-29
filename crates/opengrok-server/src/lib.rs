//! The host-facing HTTP surface.
//!
//! One router, assembled here so the binary does not have to know which slices exist. Slice 1 is
//! auth; the gateway commands and the event stream join it next (`docs/GOAL.md`).

use axum::Router;
use axum::routing::get;

pub mod agui;
pub mod auth;
pub mod connections;
pub mod recovery;

pub use agui::AgUiState;
pub use auth::{AuthState, TokenMinter};

/// Everything the server serves today.
pub fn router(state: AgUiState) -> Router {
    Router::new()
        .route("/health", get(health))
        .merge(auth::router(state.auth.clone()))
        .merge(agui::router(state))
}

/// The client's supervisor polls this with a 1500 ms deadline and discards the connection if it
/// misses (`docs/RUNBOOK.md` §4). The full body it wants arrives with the gateway slice; for now
/// it answers the shape's mandatory core so the endpoint is never the reason a boot fails.
async fn health() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({ "ok": true, "pid": std::process::id() }))
}
