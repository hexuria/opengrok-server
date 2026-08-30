//! The host-facing HTTP surface.
//!
//! One router, assembled here so the binary does not have to know which slices exist. Slice 1 is
//! auth; the gateway commands and the event stream join it next (`docs/GOAL.md`).

use axum::Router;

pub mod agui;
pub mod auth;
pub mod autonomy;
pub mod connections;
pub mod gateway;
pub mod grpc;
pub mod recovery;
pub mod seamb;
pub mod seamb_send;

pub use agui::AgUiState;
pub use auth::{AuthState, TokenMinter};

/// Everything the server serves today.
///
/// `/health` belongs to the gateway now: the desktop client's supervisor is its most demanding
/// reader (1500 ms deadline, `ok === true`), and its reply shape is a superset of what every
/// smoke script was already checking.
pub fn router(state: AgUiState, gateway: gateway::GatewayState) -> Router {
    Router::new()
        .merge(gateway::routes::router(gateway.clone()))
        .merge(seamb::router(gateway))
        .merge(auth::router(state.auth.clone()))
        .merge(agui::router(state.clone()))
        .merge(autonomy::routes::router(state.clone()))
        .merge(connections::routes::router(state))
}
