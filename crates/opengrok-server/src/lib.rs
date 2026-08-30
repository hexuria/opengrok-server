//! The host-facing HTTP surface.
//!
//! One router, assembled here so the binary does not have to know which slices exist. Slice 1 is
//! auth; the gateway commands and the event stream join it next (`docs/GOAL.md`).

use axum::Router;

pub mod account_api;
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
    let app = Router::new()
        .merge(gateway::routes::router(gateway.clone()))
        .merge(seamb::router(gateway))
        .merge(auth::router(state.auth.clone()))
        .merge(agui::router(state.clone()))
        .merge(autonomy::routes::router(state.clone()))
        .merge(account_api::router(state.auth.clone()))
        .merge(connections::routes::router(state));
    mount_web_console(app)
}

/// Serve the built web console (the Bun/Vite SPA) at `/console`, if `OG_WEB_CONSOLE_DIR` names a
/// directory that exists. Absent or missing ⇒ no console route, which is the right default for a
/// server that has not built the SPA (the smokes and tests run this way) rather than a boot error.
///
/// The fallback to `index.html` is what makes client-side routes deep-linkable: a GET for
/// `/console/account` finds no such file, so `ServeDir` hands off to the SPA's entry document and
/// the router inside the page takes over.
fn mount_web_console(app: Router) -> Router {
    use tower_http::services::{ServeDir, ServeFile};
    let Some(dir) = std::env::var("OG_WEB_CONSOLE_DIR")
        .ok()
        .filter(|dir| !dir.is_empty())
    else {
        return app;
    };
    if !std::path::Path::new(&dir).is_dir() {
        tracing::warn!(%dir, "OG_WEB_CONSOLE_DIR is set but not a directory — /console is off");
        return app;
    }
    let index = std::path::Path::new(&dir).join("index.html");
    let serve = ServeDir::new(&dir).not_found_service(ServeFile::new(index));
    app.nest_service("/console", serve)
}

pub(crate) use auth::password::hash_password as password_hash;
/// Re-exports so `account_api` can call the password helpers by a stable path.
pub(crate) use auth::password::verify_password as password_verify;
