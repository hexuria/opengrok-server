//! The host-facing HTTP surface.
//!
//! One router, assembled here so the binary does not have to know which slices exist. Slice 1 is
//! auth; the gateway commands and the event stream join it next (`docs/GOAL.md`).

use axum::Router;

pub mod account_api;
pub mod agui;
pub mod auth;
pub mod autonomy;
pub mod computers;
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
        .merge(computers::router(state.clone()))
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
    use axum::extract::Path;
    use axum::routing::get;

    let Some(dir) = std::env::var("OG_WEB_CONSOLE_DIR")
        .ok()
        .filter(|dir| !dir.is_empty())
    else {
        return app;
    };
    let dir = std::path::PathBuf::from(dir);
    // Canonicalize once: it is both the existence check and the base every served path must stay
    // under, so a request cannot climb out of the console directory with `..`.
    let Ok(root) = dir.canonicalize() else {
        tracing::warn!(dir = %dir.display(), "OG_WEB_CONSOLE_DIR does not exist — /console is off");
        return app;
    };
    let index = match std::fs::read_to_string(root.join("index.html")) {
        Ok(html) => std::sync::Arc::new(html),
        Err(error) => {
            tracing::warn!(%error, "OG_WEB_CONSOLE_DIR has no readable index.html — /console is off");
            return app;
        }
    };

    let serve = move |rel: Option<Path<String>>| {
        let root = root.clone();
        let index = index.clone();
        async move {
            let rel = rel.map(|p| p.0).unwrap_or_default();
            serve_console_path(&root, &index, &rel)
        }
    };

    // Two routes, no wildcard-vs-static conflict: the bare prefix and everything beneath it. A real
    // built file (an asset) is served with its content type; every other path is the SPA entry with
    // a 200, so a deep-linked or hard-refreshed client route is a real page, not a 404.
    app.route("/console", get(serve.clone()))
        .route("/console/", get(serve.clone()))
        .route("/console/{*rest}", get(serve))
}

/// Resolve one `/console` sub-path: a real regular file under `root` is served with a guessed
/// content type; anything else (including a client route) is the SPA `index`, 200.
fn serve_console_path(root: &std::path::Path, index: &str, rel: &str) -> axum::response::Response {
    use axum::http::header::CONTENT_TYPE;
    use axum::response::{Html, IntoResponse};

    let spa = || Html(index.to_string()).into_response();
    let rel = rel.trim_start_matches('/');
    if rel.is_empty() {
        return spa();
    }
    // Resolve and confine to `root`; a path that escapes or is not a file falls through to the SPA.
    let Ok(candidate) = root.join(rel).canonicalize() else {
        return spa();
    };
    if !candidate.starts_with(root) || !candidate.is_file() {
        return spa();
    }
    match std::fs::read(&candidate) {
        Ok(bytes) => ([(CONTENT_TYPE, console_content_type(&candidate))], bytes).into_response(),
        Err(_) => spa(),
    }
}

/// The content type for a built console asset, by extension. Vite emits js/css/svg and the like;
/// anything unrecognized is served as bytes rather than mislabeled.
fn console_content_type(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("html") => "text/html; charset=utf-8",
        Some("json") | Some("map") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        _ => "application/octet-stream",
    }
}

pub(crate) use auth::password::hash_password as password_hash;
/// Re-exports so `account_api` can call the password helpers by a stable path.
pub(crate) use auth::password::verify_password as password_verify;
