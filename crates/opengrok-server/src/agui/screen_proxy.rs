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
//! one box, and expires; the account's right to the coworker and the box's place as its computer
//! are checked again on every request, so a revoked share stops the next asset and reconnect.
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

use std::sync::LazyLock;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use opengrok_core::id::{AccountId, CoworkerId};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::agui::AgUiState;
use crate::auth::TokenMinter;

/// What a ticket is for; an access token (same key) never verifies as one, nor this as that.
const PURPOSE: &str = "screen";
/// A ticket lives between one and two of these.
const WINDOW_SECONDS: i64 = 6 * 60 * 60;
/// The most of the box's handshake reply that is read before giving up on it.
const HEAD_LIMIT: usize = 16 * 1024;

#[derive(Serialize, Deserialize)]
struct Ticket {
    sub: String,
    cw: String,
    bx: String,
    exp: i64,
    purpose: String,
}

/// Where the person's app reached this server. `OG_PUBLIC_GATEWAY_URL` when it names a real host;
/// its default, `http://{OG_BIND}`, is usually a listen address (`0.0.0.0`, `[::]`) no webview can open,
/// so then the `Host` the request came in on, which is by construction one the app can reach.
pub fn public_origin(configured: &str, headers: &HeaderMap) -> Option<String> {
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

/// The loopback port a box's noVNC page is on. Anything that is not `http://127.0.0.1:<port>/…`
/// is refused: this proxy only ever dials this host's own loopback.
fn loopback_port(page: &str) -> Option<u16> {
    let rest = page.strip_prefix("http://127.0.0.1:")?;
    rest.split(['/', '?']).next()?.parse().ok()
}

/// The box behind a ticket, as its noVNC port — re-authorised now, not at mint time.
async fn upstream_for(state: &AgUiState, coworker_id: &str, ticket: &str) -> Option<u16> {
    let claims: Ticket = state.auth.minter.verify_claims(ticket).ok()?;
    if claims.purpose != PURPOSE || claims.cw != coworker_id {
        return None;
    }
    let account = AccountId::from_stored(claims.sub);
    let coworker = CoworkerId::from_stored(claims.cw);
    if !matches!(
        crate::agui::routes::owned_coworker(state, &account, &coworker).await,
        Ok(true)
    ) {
        return None;
    }
    let scoped = crate::agui::provision::scoped_box_for(state, &account, &coworker).await?;
    if scoped.box_id != claims.bx || scoped.kind != "local-docker" {
        return None;
    }
    let page = scoped.computer.screen_url(&scoped.box_id).await.ok()??;
    loopback_port(&page)
}

/// `GET /coworkers/{id}/computer/vnc/{ticket}/{*rest}` — noVNC's files, and its websocket.
/// Every refusal is the same 404: a guessed ticket learns nothing about which part was wrong.
pub async fn serve(
    State(state): State<AgUiState>,
    Path((coworker_id, ticket, rest)): Path<(String, String, String)>,
    request: Request,
) -> Response {
    let Some(port) = upstream_for(&state, &coworker_id, &ticket).await else {
        return confined((StatusCode::NOT_FOUND, "no such screen").into_response());
    };
    let upgrade = request
        .headers()
        .get(header::UPGRADE)
        .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"));
    if upgrade {
        return tunnel(port, request).await;
    }
    confined(fetch(port, &rest).await)
}

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
async fn fetch(port: u16, rest: &str) -> Response {
    let silent = || {
        (
            StatusCode::BAD_GATEWAY,
            "the computer's screen did not answer",
        )
            .into_response()
    };
    if rest
        .split('/')
        .any(|segment| segment == ".." || segment.is_empty())
    {
        return (StatusCode::NOT_FOUND, "no such file").into_response();
    }
    let Some(client) = DIAL.as_ref() else {
        return silent();
    };
    let Ok(fetched) = client
        .get(format!("http://127.0.0.1:{port}/{rest}"))
        .send()
        .await
    else {
        return silent();
    };
    if fetched.status().is_redirection() {
        return (
            StatusCode::BAD_GATEWAY,
            "the computer's screen answered with a redirect, which this server does not follow",
        )
            .into_response();
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
    response
}

/// Put first in every page the box serves, because `confined` takes its storage away. noVNC
/// before 1.5 (1.3.0 and 1.4.0 were tried) reads `localStorage` unguarded, and in an opaque origin
/// that read throws: the page died before it dialled, and the pane stayed blank. An in-memory
/// store stands in ONLY when the real one throws; settings then last as long as the page, which
/// is all the pane needs, since `vncUrl` carries them. It widens nothing: the page could define
/// the same object itself.
const STORAGE_SHIM: &[u8] = b"<script>try{window.localStorage}catch(_){var m=new Map;\
Object.defineProperty(window,'localStorage',{configurable:true,value:{\
getItem:function(k){k=String(k);return m.has(k)?m.get(k):null},\
setItem:function(k,v){m.set(String(k),String(v))},removeItem:function(k){m.delete(String(k))},\
clear:function(){m.clear()},key:function(i){var a=Array.from(m.keys());return i<a.length?a[i]:null},\
get length(){return m.size}}})}</script>";

/// `page` with `STORAGE_SHIM` straight after its `<head>` tag, so it runs before any of the page's
/// own scripts (noVNC's are modules, which wait for the parse anyway). No `<head>` at all: first.
fn with_storage_shim(page: &[u8]) -> Vec<u8> {
    let lower = page.to_ascii_lowercase();
    let after_head = lower
        .windows(6)
        .position(|window| {
            window.starts_with(b"<head")
                && window
                    .get(5)
                    .is_some_and(|&byte| byte == b'>' || byte.is_ascii_whitespace())
        })
        .and_then(|start| {
            let close = lower.get(start..)?.iter().position(|&byte| byte == b'>')?;
            Some(start + close + 1)
        })
        .unwrap_or(0);
    let (before, after) = page.split_at(after_head.min(page.len()));
    [before, STORAGE_SHIM, after].concat()
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
/// browser checks, and the frames pass through untouched in both directions.
async fn tunnel(port: u16, mut request: Request) -> Response {
    let refused = |why: &'static str| (StatusCode::BAD_GATEWAY, why).into_response();
    let [key, protocol] = ["sec-websocket-key", "sec-websocket-protocol"].map(|name| {
        let value = request.headers().get(name)?.to_str().ok()?;
        Some(value.to_string())
    });
    let Some(key) = key else {
        return (StatusCode::BAD_REQUEST, "not a websocket handshake").into_response();
    };
    let Ok(mut upstream) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await else {
        return refused("the computer's screen did not answer");
    };
    let mut hello = format!(
        "GET /websockify HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\n\
         Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {key}\r\n"
    );
    if let Some(protocol) = &protocol {
        hello.push_str(&format!("Sec-WebSocket-Protocol: {protocol}\r\n"));
    }
    hello.push_str("\r\n");
    if upstream.write_all(hello.as_bytes()).await.is_err() {
        return refused("the computer's screen did not answer");
    }
    let Some((head, early)) = read_head(&mut upstream).await else {
        return refused("the computer's screen did not answer the handshake");
    };
    if !head.starts_with("HTTP/1.1 101") {
        return refused("the computer's screen refused the handshake");
    }
    let answered = |name: &str| {
        head.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_string())
        })
    };
    let Some(accept) = answered("sec-websocket-accept") else {
        return refused("the computer's screen refused the handshake");
    };
    let chosen = answered("sec-websocket-protocol");
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
    response
        .body(Body::empty())
        .unwrap_or_else(|_| refused("the computer's screen refused the handshake"))
}

/// The box's reply head, and whatever arrived after it in the same reads.
async fn read_head(upstream: &mut tokio::net::TcpStream) -> Option<(String, Vec<u8>)> {
    let mut seen = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            upstream.read(&mut chunk),
        )
        .await
        .ok()?
        .ok()?;
        if read == 0 {
            return None;
        }
        seen.extend_from_slice(chunk.get(..read)?);
        if let Some(end) = seen.windows(4).position(|window| window == b"\r\n\r\n") {
            let early = seen.split_off(end + 4);
            return Some((String::from_utf8_lossy(&seen).into_owned(), early));
        }
        if seen.len() > HEAD_LIMIT {
            return None;
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
#[path = "../../tests/unit/screen_proxy.rs"]
mod tests;
