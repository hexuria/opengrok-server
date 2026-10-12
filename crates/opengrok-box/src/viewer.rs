//! The box's screen viewer as the server's screen proxy dials it (opengrok-server
//! `agui/screen_proxy.rs`): which loopback port and websocket path a box's noVNC page names, the
//! page as served there, and the box's handshake reply. Pure or loopback-only; the proxy keeps
//! the tickets, the confinement and the HTTP.

use std::time::Duration;

use std::sync::LazyLock;

use reqwest::header::{
    ACCESS_CONTROL_ALLOW_ORIGIN, CONTENT_SECURITY_POLICY, CONTENT_TYPE, HOST, HeaderMap,
    HeaderValue, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The most of the box's handshake reply that is read before giving up on it.
const HEAD_LIMIT: usize = 16 * 1024;

/// The loopback port a box's noVNC page is on. Anything that is not `http://127.0.0.1:<port>/…`
/// is refused: this proxy only ever dials this host's own loopback.
pub fn loopback_port(page: &str) -> Option<u16> {
    let rest = page.strip_prefix("http://127.0.0.1:")?;
    rest.split(['/', '?']).next()?.parse().ok()
}

/// The websocket path noVNC's page on the box would open: its `path` setting (a Bot's own screen
/// is `websockify?token=s<N>`, hexuria/box `box-screen`), else `websockify`. Only those two
/// shapes are taken from a box's answer; anything else is the shared screen's path.
pub fn socket_path(page: &str) -> String {
    let named = page
        .split(['?', '&'])
        .find_map(|pair| pair.strip_prefix("path="))
        .map(|raw| raw.replace("%3F", "?").replace("%3D", "="));
    match named
        .as_deref()
        .and_then(|p| p.strip_prefix("websockify?token=s"))
    {
        Some(slot) if !slot.is_empty() && slot.bytes().all(|b| b.is_ascii_digit()) => {
            format!("websockify?token=s{slot}")
        }
        _ => "websockify".to_string(),
    }
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
    let host = headers.get(HOST)?.to_str().ok()?;
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
    let host = headers.get(HOST)?.to_str().ok()?;
    let names = ["127.0.0.1", "localhost", "[::1]"];
    let name = names.into_iter().find(|n| direct && host.starts_with(n))?;
    let port = match host.strip_prefix(name)? {
        "" => return Some(format!("http://{name}")),
        rest => rest.strip_prefix(':')?.parse::<u16>().ok()?,
    };
    Some(format!("http://{name}:{port}"))
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

/// What asking the box's viewer for one of noVNC's files came to.
pub enum Fetched {
    /// Its status, `Content-Type` and body; a page with `STORAGE_SHIM` put in.
    Got {
        status: u16,
        kind: Option<HeaderValue>,
        body: Vec<u8>,
    },
    /// Nothing on the port took the request.
    NoAnswer,
    /// A path that steps out of noVNC's files (`..` or an empty segment).
    NotFound,
    /// The viewer took the request and then said nothing usable.
    Silent,
    /// The viewer answered with a redirect, which is never followed (`DIAL`).
    Redirected,
}

/// One of noVNC's files from the box's viewer on loopback `port`.
pub async fn fetch_file(port: u16, rest: &str) -> Fetched {
    if rest
        .split('/')
        .any(|segment| segment == ".." || segment.is_empty())
    {
        return Fetched::NotFound;
    }
    let Some(client) = DIAL.as_ref() else {
        return Fetched::Silent;
    };
    let fetched = match client
        .get(format!("http://127.0.0.1:{port}/{rest}"))
        .send()
        .await
    {
        Ok(fetched) => fetched,
        Err(error) if error.is_connect() => return Fetched::NoAnswer,
        Err(_) => return Fetched::Silent,
    };
    if fetched.status().is_redirection() {
        return Fetched::Redirected;
    }
    let status = fetched.status().as_u16();
    let kind = fetched.headers().get(CONTENT_TYPE).cloned();
    let Ok(bytes) = fetched.bytes().await else {
        return Fetched::Silent;
    };
    let page = kind
        .as_ref()
        .is_some_and(|kind| kind.as_bytes().starts_with(b"text/html"));
    let body = if page {
        with_storage_shim(&bytes)
    } else {
        bytes.to_vec()
    };
    Fetched::Got { status, kind, body }
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
pub fn confine(headers: &mut HeaderMap) {
    for (name, value) in [
        (
            CONTENT_SECURITY_POLICY,
            "sandbox allow-scripts allow-pointer-lock",
        ),
        (X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        (REFERRER_POLICY, "no-referrer"),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
}

/// Put first in every page the box serves, because `confined` takes its storage away. noVNC
/// before 1.5 (1.3.0 and 1.4.0 were tried) reads `localStorage` unguarded, and in an opaque origin
/// that read throws: the page died before it dialled, and the pane stayed blank. An in-memory
/// store stands in ONLY when the real one throws; settings then last as long as the page, which
/// is all the pane needs, since `vncUrl` carries them. It widens nothing: the page could define
/// the same object itself.
pub const STORAGE_SHIM: &[u8] = b"<script>try{window.localStorage}catch(_){var m=new Map;\
Object.defineProperty(window,'localStorage',{configurable:true,value:{\
getItem:function(k){k=String(k);return m.has(k)?m.get(k):null},\
setItem:function(k,v){m.set(String(k),String(v))},removeItem:function(k){m.delete(String(k))},\
clear:function(){m.clear()},key:function(i){var a=Array.from(m.keys());return i<a.length?a[i]:null},\
get length(){return m.size}}})}</script>";

/// `page` with `STORAGE_SHIM` straight after its `<head>` tag, so it runs before any of the page's
/// own scripts (noVNC's are modules, which wait for the parse anyway). No `<head>` at all: first.
pub fn with_storage_shim(page: &[u8]) -> Vec<u8> {
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

/// The box's reply head, and whatever arrived after it in the same reads.
pub async fn read_head(upstream: &mut tokio::net::TcpStream) -> Option<(String, Vec<u8>)> {
    let mut seen = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        let waited = tokio::time::timeout(Duration::from_secs(10), upstream.read(&mut chunk));
        let read = waited.await.ok()?.ok()?;
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

/// The box's viewer, opened for a websocket on `path` (its handshake made with the browser's
/// `key` and `protocol`): the connection, what the box sent behind its 101 (RFB speaks first,
/// and those bytes belong to the browser), and the answer's accept and chosen protocol.
pub struct Opened {
    pub upstream: tokio::net::TcpStream,
    pub early: Vec<u8>,
    pub accept: String,
    pub protocol: Option<String>,
}

/// How dialling the box's viewer went: nothing listening on the port (a remembered port gone
/// stale, so the caller asks again), a refusal in words, or the socket open.
pub enum Dialled {
    NoAnswer,
    Refused(&'static str),
    Open(Opened),
}

/// Dial the viewer on loopback `port` and make the websocket handshake for `path`.
pub async fn open_socket(port: u16, path: &str, key: &str, protocol: Option<&str>) -> Dialled {
    let Ok(mut upstream) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await else {
        return Dialled::NoAnswer;
    };
    let mut hello = format!(
        "GET /{path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\n\
         Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {key}\r\n"
    );
    if let Some(protocol) = protocol {
        hello.push_str(&format!("Sec-WebSocket-Protocol: {protocol}\r\n"));
    }
    hello.push_str("\r\n");
    if upstream.write_all(hello.as_bytes()).await.is_err() {
        return Dialled::Refused("the computer's screen did not answer");
    }
    let Some((head, early)) = read_head(&mut upstream).await else {
        return Dialled::Refused("the computer's screen did not answer the handshake");
    };
    let answered = |name: &str| {
        head.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_string())
        })
    };
    match (
        head.starts_with("HTTP/1.1 101"),
        answered("sec-websocket-accept"),
    ) {
        (true, Some(accept)) => Dialled::Open(Opened {
            upstream,
            early,
            accept,
            protocol: answered("sec-websocket-protocol"),
        }),
        _ => Dialled::Refused("the computer's screen refused the handshake"),
    }
}
