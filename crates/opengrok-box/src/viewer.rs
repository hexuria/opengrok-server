//! The box's screen viewer as the server's screen proxy dials it (opengrok-server
//! `agui/screen_proxy.rs`): which loopback port and websocket path a box's noVNC page names, the
//! page as served there, and the box's handshake reply. Pure or loopback-only; the proxy keeps
//! the tickets, the confinement and the HTTP.

use std::time::Duration;

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
