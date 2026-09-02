//! The host-facing HTTP surface.
//!
//! One router, assembled here so the binary does not have to know which slices exist. Slice 1 is
//! auth; the gateway commands and the event stream join it next (`docs/GOAL.md`).

use axum::Router;

pub mod account_api;
pub mod agui;
pub mod auth;
pub mod auto_review;
pub mod autonomy;
pub mod computers;
pub mod connections;
pub mod domain_proof;
pub mod gateway;
pub mod gateway_admin;
pub mod grpc;
pub mod local_exec;
pub mod mcp_door;
pub mod models;
pub mod recovery;
pub mod seamb;
pub mod seamb_send;
pub mod spend;

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
        .merge(seamb::router(gateway.clone()))
        .merge(auth::router(state.auth.clone()))
        .merge(auth::oauth_mcp::router(state.auth.clone()))
        .merge(agui::router(state.clone()))
        .merge(autonomy::routes::router(state.clone()))
        .merge(account_api::router(state.auth.clone()))
        .merge(local_exec::router(state.auth.clone()))
        .merge(auto_review::router(state.auth.clone()))
        .merge(computers::router(state.clone()))
        .nest("/mcp", mcp_door::router(gateway))
        .merge(connections::routes::router(state));
    let app = mount_web_console(app);
    // Request trace, ON by default (`OG_TRACE_REQUESTS=0` turns it off): one INFO line per
    // request with method, path, status, the request id, whether an Origin header was present
    // (the gateway refuses those 403 before the token), and the LENGTH of the presented bearer
    // (never its value) so a 0- or wrong-length token that can never match is visible. It used to
    // be opt-in, and the dev server went silent for a day after a restart without the flag — the
    // question "was the stream up at 03:16" had no answer. Default-on is the answer.
    let app = if std::env::var("OG_TRACE_REQUESTS").as_deref() == Ok("0") {
        app
    } else {
        app.layer(axum::middleware::from_fn(trace_request))
    };
    // Request ids. `X-Request-Id` is taken from the client when it sends one (the desktop client
    // stamps every gateway call and every SSE connect), minted as a UUID when it does not, and
    // echoed on the response either way — so a client log line and a server log line for the same
    // call share one key. ORDER MATTERS: `.layer()` wraps what came before, so `Set` is added last
    // to run first, then `Propagate` copies the id onto the response, and only then does the trace
    // above (innermost) see a request that already carries its id.
    app.layer(tower_http::request_id::PropagateRequestIdLayer::x_request_id())
        .layer(tower_http::request_id::SetRequestIdLayer::x_request_id(
            tower_http::request_id::MakeRequestUuid,
        ))
        .layer(axum::middleware::from_fn(bound_request_id))
}

/// The longest client-supplied request id we keep. A UUID is 36; the desktop's are shorter. Past
/// this the header is dropped before `SetRequestId` sees it, so a fresh id is minted instead —
/// the id lands on every log line the request touches, and an 8 KB value there is a nuisance
/// even though `HeaderValue` already rules out control characters.
const REQUEST_ID_MAX: usize = 128;

/// Runs OUTSIDE `SetRequestId` (added last): strips an `X-Request-Id` that is too long or not
/// visible ASCII, so the layer below mints one.
async fn bound_request_id(
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let keep = req
        .headers()
        .get("x-request-id")
        .map(|value| {
            let bytes = value.as_bytes();
            !bytes.is_empty()
                && bytes.len() <= REQUEST_ID_MAX
                && bytes.iter().all(|b| b.is_ascii_graphic())
        })
        .unwrap_or(true);
    if !keep {
        req.headers_mut().remove("x-request-id");
    }
    next.run(req).await
}

/// The request id the layer above put on the request — or `-` when the layer is not mounted
/// (tests that build a bare router).
pub fn request_id(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("-")
        .to_string()
}

/// See `OG_TRACE_REQUESTS` above. Logs presence/length of sensitive headers, never their contents.
/// The handler runs inside a span carrying the request id, so every line it logs — a policy
/// refusal, a box wake, a domain proof — is greppable by the same id as the request line.
async fn trace_request(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use tracing::Instrument as _;

    let method = req.method().clone();
    let uri = req.uri().clone();
    let id = request_id(req.headers());
    let has_origin = req.headers().contains_key(axum::http::header::ORIGIN);
    let auth_len = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.len())
        .unwrap_or(0);
    let started = std::time::Instant::now();
    let span = tracing::info_span!("http", id = %id);
    let response = next.run(req).instrument(span).await;
    tracing::info!(
        %id,
        %method,
        %uri,
        status = response.status().as_u16(),
        origin = has_origin,
        auth_len,
        ms = started.elapsed().as_millis() as u64,
        "request"
    );
    response
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
