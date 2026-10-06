//! The host-facing HTTP surface.
//!
//! One router, assembled here so the binary does not have to know which slices exist.

use axum::Router;

pub mod account_api;
pub mod agui;
pub mod artifacts;
pub mod auth;
pub mod auto_review;
pub mod autonomy;
pub mod cards;
pub mod computers;
pub mod connections;
pub mod domain_proof;
pub mod gateway_admin;
pub mod health;
pub mod hooks;
pub mod host_state;
pub mod inference;
#[cfg(feature = "jev")]
pub mod jev;
// Test tooling kept beside the tests: only this crate's own tests turn `record-wire` on.
#[cfg(feature = "record-wire")]
#[path = "../tests/support/wire_record.rs"]
mod wire_record;
#[cfg(not(feature = "jev"))]
pub mod jev {
    /// AuthState holds `Option<SharedJev>`. Uninhabited so a door cannot be built when the SDK
    /// is not in the graph.
    pub type SharedJev = std::convert::Infallible;

    pub fn from_env() -> Option<SharedJev> {
        tracing::info!(
            "this build was compiled without the cargo feature `jev`, so Jev is unavailable"
        );
        None
    }
}
pub mod local_exec;
pub mod mcp_door;
pub mod models;
pub mod pairs;
pub mod persona;
pub mod plugin_desk;
pub mod plugin_registry;
pub mod points;
pub mod recipes;
pub mod recovery;
// Test seams, kept beside the tests like `tests/unit/*`: every caller is a test.
#[path = "../tests/support/seams.rs"]
mod seams;
pub mod skills;
pub mod spend;
pub(crate) mod tape_lesson;
pub mod templates;
pub mod workflows;

pub use agui::AgUiState;
pub use auth::{AuthState, TokenMinter};
// The one clock every module stamps its events and rows with.
pub(crate) use agui::routes::now_ms;

/// Test builds only: record a router a test serves beside [`router`] (the plugin registry tests
/// serve a fixture registry at the real paths), as `router` records its own, while `OG_RECORD_WIRE`
/// is set.
#[cfg(feature = "record-wire")]
pub fn recorded(app: Router) -> Router {
    if wire_record::directory().is_some() {
        app.layer(axum::middleware::from_fn(wire_record::record))
    } else {
        app
    }
}

/// Everything the server serves today.
pub fn router(mut state: AgUiState, host: host_state::HostState) -> Router {
    if state.host_settings.is_none() {
        state.host_settings = Some(host.settings.clone());
    }
    let app = Router::new()
        .merge(health::router(host.clone()))
        .merge(hooks::router(host.clone()))
        .merge(agui::user_form::agui_router(host.clone()))
        .merge(agui::site_logins::agui_router(host.clone()))
        // `POST /ag-ui` needs `HostState` so a UserForm CUSTOM can mint the gateway
        // card and stamp `entryId` on the SSE frame. Other AG-UI routes stay on `AgUiState`.
        .merge(agui::run_router(host.clone()))
        .merge(auth::router(state.auth.clone()))
        .merge(auth::oauth_mcp::router(state.auth.clone()))
        .merge(agui::router(state.clone()))
        .merge(autonomy::routes::router(host.clone()))
        .merge(account_api::router(state.auth.clone()))
        .merge(inference::router(state.clone()))
        .merge(recipes::router(state.clone()))
        .merge(workflows::router(state.clone()))
        .merge(skills::router(state.clone()))
        .merge(artifacts::router(state.clone()))
        .merge(plugin_registry::router(state.clone()));
    #[cfg(feature = "jev")]
    let app = app.merge(jev::routes::router(state.clone()));
    let app = app
        .merge(local_exec::router(state.auth.clone()))
        .merge(inference::relay_router(host.clone()))
        .merge(auto_review::router(state.auth.clone()))
        .merge(computers::router(state.clone()))
        .nest("/mcp", mcp_door::router(host))
        .merge(connections::routes::router(state));
    // A 502, 503 OR 504 THE SERVER WRITES ITSELF IS A SENTENCE IN `{"error": …}`. NativeChat reads
    // any of the three as "the server is out of reach" unless its body says otherwise, so a
    // handler's plain-text refusal (a box down, a store that did not answer) read as the server
    // gone. One layer turns every such plain-text body into the JSON a client shows.
    let app = app.layer(axum::middleware::from_fn(gateway_errors_as_json));
    // The wire corpus (#255): test builds only, and only while `OG_RECORD_WIRE` is set. Added
    // before the console is mounted, so it sees the API's routes and not the static files.
    #[cfg(feature = "record-wire")]
    let app = if wire_record::directory().is_some() {
        app.layer(axum::middleware::from_fn(wire_record::record))
    } else {
        app
    };
    let app = mount_web_console(app);
    // Request trace, ON by default (`OG_TRACE_REQUESTS=0` turns it off): one INFO line per
    // request with method, path, status, the request id, whether an Origin header was present,
    // and the LENGTH of the presented bearer (never its value) so a 0- or wrong-length token that can never match is visible. It used to
    // be opt-in, and the dev server went silent for a day after a restart without the flag — the
    // question "was the stream up at 03:16" had no answer. Default-on is the answer.
    let app = if std::env::var("OG_TRACE_REQUESTS").as_deref() == Ok("0") {
        app
    } else {
        app.layer(axum::middleware::from_fn(trace_request))
    };
    // Request ids. `X-Request-Id` is taken from the client when it sends one, minted as a UUID
    // when it does not, and echoed on the response either way — so a client log line and a server log line for the same
    // call share one key. ORDER MATTERS: `.layer()` wraps what came before, so `Set` is added last
    // to run first, then `Propagate` copies the id onto the response, and only then does the trace
    // above (innermost) see a request that already carries its id.
    app.layer(tower_http::request_id::PropagateRequestIdLayer::x_request_id())
        .layer(tower_http::request_id::SetRequestIdLayer::x_request_id(
            tower_http::request_id::MakeRequestUuid,
        ))
        .layer(axum::middleware::from_fn(bound_request_id))
}

/// Wrap a plain-text 502/503/504 body as `{"error": text}` (see `router`). A JSON body is left as
/// the handler wrote it, and so is a streamed or empty one.
/// The longest plain-text 502/503/504 body `gateway_errors_as_json` reads before it gives up.
const GATEWAY_ERROR_MAX: usize = 256 * 1024;

async fn gateway_errors_as_json(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::{StatusCode, header};
    let response = next.run(request).await;
    let gateway_ish = matches!(
        response.status(),
        StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT
    );
    let plain = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/plain"));
    if !(gateway_ish && plain) {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    // A body past the cap, or one whose stream broke, still gets a sentence: an empty 503 is the
    // "out of reach" reading this layer exists to replace (review of #262). The cap clears the
    // longest sentence written today, a from-tape refusal of 64 KiB plus its promise.
    let sentence = match axum::body::to_bytes(body, GATEWAY_ERROR_MAX).await {
        Ok(bytes) => String::from_utf8_lossy(&bytes).trim().to_string(),
        Err(_) => "the server could not say why; it is still running".to_string(),
    };
    let json = serde_json::json!({ "error": sentence }).to_string();
    parts.headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    parts.headers.remove(header::CONTENT_LENGTH);
    axum::response::Response::from_parts(parts, axum::body::Body::from(json))
}

/// The longest client-supplied request id we keep. A UUID is 36. Past
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
    // A screen-proxy path carries its ticket, and the page's query the box's VNC password.
    let uri = match req.uri().path().split_once("/computer/vnc/") {
        Some((head, _)) => format!("{head}/computer/vnc/<ticket>"),
        None => req.uri().to_string(),
    };
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
/// `/console/account` finds no such file, so the handler hands off to the SPA's entry document and
/// the router inside the page takes over.
///
/// THE ENTRY DOCUMENT IS READ PER REQUEST, NOT SNAPSHOTTED AT BOOT. It used to be loaded into an
/// `Arc<String>` here while the hashed bundles beside it were read from disk on every request —
/// so a console rebuilt against a running server left the two halves disagreeing forever: the new
/// bundles were on disk and served, and the boot-time index still named the deleted old ones.
/// Every reload painted a white page, and neither the server log nor the browser console said why,
/// because the missing bundle was answered with the index at 200. One small file re-read per
/// request costs nothing next to the filesystem work this handler already does, and it means
/// `bun run build` lands without a restart.
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
    // Read once here only to refuse the route when the build is absent — the value is not kept.
    if let Err(error) = std::fs::read_to_string(root.join("index.html")) {
        tracing::warn!(%error, "OG_WEB_CONSOLE_DIR has no readable index.html — /console is off");
        return app;
    }

    let serve = move |rel: Option<Path<String>>| {
        let root = root.clone();
        async move {
            let rel = rel.map(|p| p.0).unwrap_or_default();
            serve_console_path(&root, &rel)
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
/// content type; a client route is the SPA `index`, 200; a missing *asset* is a 404.
///
/// Two rules here are the difference between a console deploy that lands and one that paints a
/// white page for everyone who had the old one open.
///
/// 1. THE ENTRY DOCUMENT MUST NOT BE CACHED. Vite names every bundle by its content hash, so
///    `index.html` is the only file that says which hashes are current. Served with no directive
///    a browser caches it heuristically, and after the next build that stale copy asks for a
///    bundle that no longer exists on disk. The assets are the mirror image: their name *is* their
///    version, so they can be cached forever and never revalidated.
/// 2. A MISSING ASSET MUST 404, NOT FALL THROUGH TO THE SPA. The fallback used to answer every
///    unresolved path with `index.html` at 200 — including `/console/assets/index-OLDHASH.js`.
///    The browser then parsed an HTML document as a module, and what reached the console was a
///    syntax error about an unexpected `<`, with nothing naming the real cause. Only paths that
///    could be client routes fall through; anything under the build's asset directory, or carrying
///    a file extension, answers for itself.
fn serve_console_path(root: &std::path::Path, rel: &str) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
    use axum::response::IntoResponse;

    // The entry document is read only when it is about to be served — an asset request never
    // touches it. It is a blocking read on a runtime thread, the same as the asset read below; on
    // the file sizes a Vite build emits that is well under the noise floor, but it is not free,
    // so it is not paid for a request that does not need it.
    //
    // Losing the document after boot — a `dist` wiped mid-rebuild, a mount that went away — means
    // the console is gone, not that a blank client route should render. An empty 200 is the worst
    // of the three answers: it looks like the page loaded and simply had nothing to say.
    //
    // `no-store`, not `no-cache`: the latter still lets a browser hold the copy and revalidate,
    // and a 304 on a document naming dead hashes is exactly the failure being closed.
    let spa = || match std::fs::read_to_string(root.join("index.html")) {
        Ok(index) => (
            [
                (CONTENT_TYPE, "text/html; charset=utf-8"),
                (CACHE_CONTROL, "no-store"),
            ],
            index,
        )
            .into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "the web console is not built",
        )
            .into_response(),
    };
    let rel = rel.trim_start_matches('/');
    // The entry document BY ITS OWN NAME is still the entry document. Without this it carried an
    // extension, took the file branch, and left with a five-minute `max-age` — a five-minute
    // window in which the one URL a bookmark would use hands back a page naming hashes the next
    // build deletes, which is the exact hole the `no-store` above exists to close.
    if rel.is_empty() || rel == "index.html" {
        return spa();
    }
    // A request that names a file is a request for that file. Answering it with the SPA turns a
    // missing asset into an unreadable parse error three layers away from the cause. A path
    // carrying a `..` segment is in the same class: whatever it is, it is not a client route, and
    // the confinement check below already refuses to serve through it — so say 404 rather than
    // hand a probe a 200 and let it wonder.
    //
    // THE EXTENSION TEST IS A CONSTRAINT ON THE ROUTER. A client route containing a `.` in any
    // segment would be taken for a file and answered 404 instead of deep-linking. None does today;
    // the day one is added, this heuristic has to learn about it first.
    let names_a_file = rel.starts_with("assets/")
        || std::path::Path::new(rel).extension().is_some()
        || rel.split('/').any(|segment| segment == "..");
    let missing = || {
        if names_a_file {
            (StatusCode::NOT_FOUND, "not found").into_response()
        } else {
            spa()
        }
    };

    // Resolve and confine to `root`; a path that escapes or is not a file cannot be served.
    let Ok(candidate) = root.join(rel).canonicalize() else {
        return missing();
    };
    if !candidate.starts_with(root) || !candidate.is_file() {
        return missing();
    }
    match std::fs::read(&candidate) {
        // A content-hashed bundle never changes under its own name, so it is safe to pin. Anything
        // else the build emits keeps a short life instead of an indefinite one.
        Ok(bytes) => {
            let cache = if rel.starts_with("assets/") {
                "public, max-age=31536000, immutable"
            } else {
                "public, max-age=300"
            };
            (
                [
                    (CONTENT_TYPE, console_content_type(&candidate)),
                    (CACHE_CONTROL, cache),
                ],
                bytes,
            )
                .into_response()
        }
        Err(_) => missing(),
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

#[cfg(test)]
#[path = "../tests/unit/gateway_errors.rs"]
mod gateway_errors_tests;

#[cfg(test)]
#[path = "../tests/unit/console.rs"]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod console_tests;
