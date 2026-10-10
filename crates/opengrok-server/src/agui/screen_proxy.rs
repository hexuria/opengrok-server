//! A Local VM's live desktop, served through this server (#191).
//!
//! A Docker box publishes noVNC on `127.0.0.1` only, and must: widening the bind would put the
//! exec, host and egress-bearer sockets on the network with it. But the person's app is on
//! another machine whenever the gateway is not loopback — which the client insists on — so a
//! loopback `vncUrl` painted nothing but the PNG fallback. The page and its websocket are proxied
//! here instead, and `vncUrl` names this server.
//!
//! THE TICKET IS IN THE PATH. A webview loading `vncUrl` sends no Authorization header, and noVNC
//! fetches its own scripts by relative path and its websocket by the `path` it is given: a token
//! in the query would be gone by the first asset. The ticket names one account, one coworker and
//! one box, and expires; the account's right to the coworker is checked again on every request,
//! and the box's place as its computer, with its port, every `PLACE_FOR` (`UPSTREAMS`), so a
//! retired coworker stops the next asset and reconnect, and a computer that moved within seconds.
//!
//! THE TICKET IS STABLE WITHIN A WINDOW. The pane polls the status, and a `vncUrl` that changed on
//! every poll would reload the desktop each time; the same claims sign to the same token, so the
//! URL changes once per window, not once per poll.
//!
//! THE BOX'S PAGES ARE HOSTILE UNTIL PROVEN OTHERWISE. The model has a shell on the box, a per-org
//! box is shared by colleagues, and their skill scripts are copied there to run — any of them can
//! replace what answers on 6080. Yet the page is served from THIS origin, which also serves
//! `/console` and honours its `og_access` cookie. See `confined` for what keeps the one from
//! acting as the other, and `DIAL` for why a redirect is never followed.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use opengrok_core::id::{AccountId, CoworkerId};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::agui::AgUiState;
use crate::auth::TokenMinter;
pub use crate::seams::screen_tickets_due;
use opengrok_box::viewer::{Dialled, loopback_port, socket_path, with_storage_shim};

/// What a ticket is for; an access token (same key) never verifies as one, nor this as that.
const PURPOSE: &str = "screen";
/// A ticket lives between one and two of these.
const WINDOW_SECONDS: i64 = 6 * 60 * 60;

/// Each ticket's box, its noVNC port, the box's `Computer::generation` it was learned under and
/// when, until the ticket expires: asking on each of a page's ~80 files and its websocket was 160
/// `docker` processes and 500 queries at once, 3.7 s a file and 5 s to paint (3 Oct 2026). A port
/// is the box's only in that generation: a stop, start, rebuild or removal frees it for anything
/// to take. Stamped once the check that found it has finished, never before, so one cut short
/// leaves the next request to make its own.
/// The last field is the websocket path on that port: a Bot's own screen is reached by its token
/// (#376), the shared screen by the bare path.
type Upstream = (String, u16, i64, Instant, u64, String);
pub(crate) static UPSTREAMS: LazyLock<Mutex<HashMap<String, Upstream>>> =
    LazyLock::new(Mutex::default);

/// How long a ticket's box is taken to still be its coworker's computer (`scoped_box_row_for`,
/// five queries) between checks; past it, the place and the port are both asked again.
const PLACE_FOR: Duration = Duration::from_secs(5);

fn remember(ticket: &str, box_id: &str, (port, path): (u16, &str), until: i64, generation: u64) {
    let now = chrono::Utc::now().timestamp();
    if let Ok(mut known) = UPSTREAMS.lock() {
        known.retain(|_, upstream| upstream.2 > now);
        let upstream = (
            box_id.into(),
            port,
            until,
            Instant::now(),
            generation,
            path.into(),
        );
        known.insert(ticket.into(), upstream);
    }
}

/// Forget the port of every ticket for a box stopped, reset or rebuilt here, or whose port went
/// dead or that is no longer its coworker's: the old ports may by then be another box's.
pub fn forget_box(box_id: &str) {
    if let Ok(mut known) = UPSTREAMS.lock() {
        known.retain(|_, (bx, ..)| bx != box_id);
    }
}

#[derive(Serialize, Deserialize)]
struct Ticket {
    sub: String,
    cw: String,
    bx: String,
    exp: i64,
    purpose: String,
}

/// Where the person's app reached this server: `loopback_origin`, or else `OG_PUBLIC_GATEWAY_URL`
/// when it names a real host, whatever the `Host` (behind a proxy, a name only the proxy knows).
/// Its default, `http://{OG_BIND}`, is usually a listen address (`0.0.0.0`, `[::]`) no webview can
/// open, so then the `Host` the request came in on, by construction one the app can reach.
pub fn public_origin(configured: &str, headers: &HeaderMap) -> Option<String> {
    if let Some(origin) = loopback_origin(headers) {
        return Some(origin);
    }
    let configured = configured.trim_end_matches('/');
    let listen_address = configured.contains("://0.0.0.0") || configured.contains("://[::]");
    if configured.starts_with("http") && !listen_address {
        return Some(configured.to_string());
    }
    let host = headers.get(header::HOST)?.to_str().ok()?;
    let plain = |c: char| c.is_ascii_alphanumeric() || ".-:[]".contains(c);
    if host.is_empty() || !host.chars().all(plain) {
        return None;
    }
    let https = headers
        .get("x-forwarded-proto")
        .is_some_and(|proto| proto.as_bytes() == b"https");
    Some(format!("{}://{host}", if https { "https" } else { "http" }))
}

/// `http://` and a `Host` of `127.0.0.1`, `localhost` or `[::1]`, any port, where an app on this
/// machine just reached this plain listener; `OG_PUBLIC_GATEWAY_URL` can name a front that is down
/// (an `https://….local` with no Caddy blanked every screen). REBUILT, NEVER ECHOED: it outranks
/// that in a URL with a ticket in it. Not after a front proxy (`Forwarded`, `X-Forwarded-*`,
/// `X-Real-IP`; noticed, never read): its `Host` is the proxy's (nginx sends its upstream's).
fn loopback_origin(headers: &HeaderMap) -> Option<String> {
    let proxied = |name: &str| name.contains("forwarded") || name == "x-real-ip";
    let direct = !headers.keys().any(|name| proxied(name.as_str()));
    let host = headers.get(header::HOST)?.to_str().ok()?;
    let names = ["127.0.0.1", "localhost", "[::1]"];
    let name = names.into_iter().find(|n| direct && host.starts_with(n))?;
    let port = match host.strip_prefix(name)? {
        "" => return Some(format!("http://{name}")),
        rest => rest.strip_prefix(':')?.parse::<u16>().ok()?,
    };
    Some(format!("http://{name}:{port}"))
}

/// `local_page` (the box's own noVNC URL on this host) as a URL on `origin`, carrying the same
/// noVNC settings plus the websocket path under the ticket. `None` when it cannot be signed.
pub fn proxied_page(
    minter: &TokenMinter,
    origin: &str,
    account: &AccountId,
    coworker: &CoworkerId,
    box_id: &str,
    local_page: &str,
    now_seconds: i64,
) -> Option<String> {
    let (_, settings) = local_page.split_once('?')?;
    let ticket = minter
        .mint_claims(&Ticket {
            sub: account.as_str().to_string(),
            cw: coworker.as_str().to_string(),
            bx: box_id.to_string(),
            exp: (now_seconds.div_euclid(WINDOW_SECONDS) + 2) * WINDOW_SECONDS,
            purpose: PURPOSE.to_string(),
        })
        .ok()?;
    let base = format!("coworkers/{}/computer/vnc/{ticket}", coworker.as_str());
    Some(format!(
        "{origin}/{base}/vnc.html?{settings}&path={base}/websockify"
    ))
}

/// The box behind a ticket, and its noVNC port — re-authorised now, not at mint time. The port
/// is the remembered one while the box is in the generation it was learned in and its place was
/// checked within `PLACE_FOR`; otherwise the place and the port are both asked again, the
/// generation read first, so a stop or removal during the asking is seen by the next request.
async fn upstream_for(state: &AgUiState, cw: &str, ticket: &str) -> Option<(u16, String, String)> {
    let claims: Ticket = state.auth.minter.verify_claims(ticket).ok()?;
    if claims.purpose != PURPOSE || claims.cw != cw {
        return None;
    }
    let account = AccountId::from_stored(claims.sub);
    let coworker = CoworkerId::from_stored(claims.cw);
    let Ok(true) = crate::agui::routes::owned_coworker(state, &account, &coworker).await else {
        return None;
    };
    let docker = crate::agui::provision::provider_for(state, None, "local-docker").await?;
    let generation = docker.generation(&claims.bx);
    if let Ok(known) = UPSTREAMS.lock()
        && let Some((_, port, _, checked, of, path)) = known.get(ticket)
        && *of == generation
        && checked.elapsed() < PLACE_FOR
    {
        return Some((*port, claims.bx, path.clone()));
    }
    let row = crate::agui::provision::scoped_box_row_for(state, &account, &coworker).await;
    let Some(row) = row.filter(|row| row.box_id == claims.bx && row.kind == "local-docker") else {
        forget_box(&claims.bx);
        return None;
    };
    // The Bot's own screen when it was told to use one (#376), else the shared one.
    let which = crate::agui::provision::screen_for(state, &account, coworker.as_str()).await;
    let page = docker.screen_url(&row.box_id, &which).await.ok()??;
    let port = loopback_port(&page)?;
    let path = socket_path(&page);
    remember(ticket, &row.box_id, (port, &path), claims.exp, generation);
    Some((port, row.box_id, path))
}

/// `GET /coworkers/{id}/computer/vnc/{ticket}/{*rest}` — noVNC's files, and its websocket.
/// Every refusal is the same 404: a guessed ticket learns nothing about which part was wrong.
/// A remembered port nothing answers on is asked for once more, fresh, before giving up.
pub async fn serve(
    State(state): State<AgUiState>,
    Path((coworker_id, ticket, rest)): Path<(String, String, String)>,
    mut request: Request,
) -> Response {
    let upgrade = request
        .headers()
        .get(header::UPGRADE)
        .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"));
    for _ in 0..2 {
        let Some((port, box_id, path)) = upstream_for(&state, &coworker_id, &ticket).await else {
            return confined((StatusCode::NOT_FOUND, "no such screen").into_response());
        };
        if upgrade {
            match tunnel(port, &path, request).await {
                Ok(response) => return response,
                Err(unsent) => request = *unsent,
            }
        } else if let Some(fetched) = fetch(port, &rest).await {
            return confined(fetched);
        }
        forget_box(&box_id);
    }
    let silent = (StatusCode::BAD_GATEWAY, SILENT).into_response();
    if upgrade { silent } else { confined(silent) }
}

const SILENT: &str = "the computer's screen did not answer";
const REDIRECTED: &str =
    "the computer's screen answered with a redirect, which this server does not follow";

/// The one client the proxy dials a box with. NO REDIRECTS: `loopback_port` pins only the first
/// hop, and reqwest follows ten by default — a box answering `302 Location:
/// http://169.254.169.254/…` had this host fetch its cloud credentials (or the gateway's loopback
/// admin, or any internal service) and hand the body to the ticket holder, and to the box's own
/// script. NO PROXY: it only ever dials loopback, and must not carry a box's port through an
/// `HTTP_PROXY` in the server's environment.
static DIAL: LazyLock<Option<reqwest::Client>> = LazyLock::new(|| {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(Duration::from_secs(20))
        .build()
        .ok()
});

/// One of noVNC's files from the box on `port`, with only its status, body and `Content-Type`.
/// `None` when nothing on `port` took the request.
async fn fetch(port: u16, rest: &str) -> Option<Response> {
    let silent = || Some((StatusCode::BAD_GATEWAY, SILENT).into_response());
    if rest
        .split('/')
        .any(|segment| segment == ".." || segment.is_empty())
    {
        return Some((StatusCode::NOT_FOUND, "no such file").into_response());
    }
    let Some(client) = DIAL.as_ref() else {
        return silent();
    };
    let fetched = match client
        .get(format!("http://127.0.0.1:{port}/{rest}"))
        .send()
        .await
    {
        Ok(fetched) => fetched,
        Err(error) if error.is_connect() => return None,
        Err(_) => return silent(),
    };
    if fetched.status().is_redirection() {
        return Some((StatusCode::BAD_GATEWAY, REDIRECTED).into_response());
    }
    let status = StatusCode::from_u16(fetched.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let kind = fetched.headers().get(header::CONTENT_TYPE).cloned();
    let Ok(bytes) = fetched.bytes().await else {
        return silent();
    };
    let page = kind
        .as_ref()
        .is_some_and(|kind| kind.as_bytes().starts_with(b"text/html"));
    let bytes = if page {
        with_storage_shim(&bytes).into()
    } else {
        bytes
    };
    let mut response = (status, bytes).into_response();
    if let Some(kind) = kind {
        response.headers_mut().insert(header::CONTENT_TYPE, kind);
    }
    Some(response)
}

/// Every answer on the proxy's path, the box's files included, as a page that cannot act as the
/// person. Without this a box that replaced its noVNC (a shell command, a colleague's skill
/// script, a prompt injection) ran script on THIS origin: `fetch('/coworkers', {credentials:
/// 'include'})` with the `og_access` cookie of whoever opened `vncUrl` in a browser signed in to
/// `/console` — or framed `/console` and read it. Before the proxy the page was on its own
/// loopback origin, which carried no cookies.
///
/// - `sandbox` without `allow-same-origin` gives the page an OPAQUE origin. Its requests to this
///   server are cross-site, so the `SameSite=Lax` cookies stay home, and without CORS on the API
///   it could not read an answer anyway. noVNC needs only its scripts and pointer lock; forms,
///   popups and top navigation stay off. DO NOT add `allow-same-origin` to "fix" a noVNC error:
///   together with `allow-scripts` it removes the sandbox.
/// - `Access-Control-Allow-Origin: *` is what makes that workable: noVNC loads its modules with
///   `crossorigin="anonymous"` and fetches its locale and `package.json`, and from an opaque
///   origin each of those is a CORS request. `*` never carries credentials.
/// - `nosniff`, so a file is only ever what its `Content-Type` says.
/// - `no-referrer`, because the ticket is in the path and would ride along to anything else the
///   page loads.
fn confined(mut response: Response) -> Response {
    let headers = response.headers_mut();
    for (name, value) in [
        (
            header::CONTENT_SECURITY_POLICY,
            "sandbox allow-scripts allow-pointer-lock",
        ),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        (header::REFERRER_POLICY, "no-referrer"),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    response
}

/// Replay the browser's websocket handshake to the box and splice the two sockets. REPLAYED, NOT
/// REMADE: the box answers the key the browser sent, so its `Sec-WebSocket-Accept` is the one the
/// browser checks, and the frames pass through untouched in both directions. `Err` hands the
/// request back when nothing on `port` took the connection.
async fn tunnel(port: u16, path: &str, mut request: Request) -> Result<Response, Box<Request>> {
    let refused = |why: &'static str| Ok((StatusCode::BAD_GATEWAY, why).into_response());
    let [key, protocol] = ["sec-websocket-key", "sec-websocket-protocol"].map(|name| {
        let value = request.headers().get(name)?.to_str().ok()?;
        Some(value.to_string())
    });
    let Some(key) = key else {
        return Ok((StatusCode::BAD_REQUEST, "not a websocket handshake").into_response());
    };
    let opened = opengrok_box::viewer::open_socket(port, path, &key, protocol.as_deref()).await;
    let (mut upstream, early, accept, chosen) = match opened {
        Dialled::NoAnswer => return Err(Box::new(request)),
        Dialled::Refused(why) => return refused(why),
        Dialled::Open(open) => (open.upstream, open.early, open.accept, open.protocol),
    };
    let upgraded = hyper::upgrade::on(&mut request);
    tokio::spawn(async move {
        let Ok(upgraded) = upgraded.await else {
            return;
        };
        let mut client = hyper_util::rt::TokioIo::new(upgraded);
        // Bytes the box sent right behind its 101 (RFB speaks first) belong to the browser.
        if !early.is_empty() && client.write_all(&early).await.is_err() {
            return;
        }
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    });
    let mut response = Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, "websocket")
        .header("sec-websocket-accept", accept);
    if let Some(chosen) = chosen {
        response = response.header("sec-websocket-protocol", chosen);
    }
    response.body(Body::empty()).map_or_else(
        |_| refused("the computer's screen refused the handshake"),
        Ok,
    )
}

#[cfg(test)]
#[path = "../../tests/unit/screen_proxy.rs"]
mod tests;
