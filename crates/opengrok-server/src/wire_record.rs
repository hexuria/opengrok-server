//! Record the wire NativeChat reads (#255): every AG-UI frame a route streams and every REST
//! body it answers, as the server's own tests drive it, for `examples/wire_corpus.rs` to turn
//! into the corpus NativeChat vendors (`tests/fixtures/wire/`).
//!
//! NOT IN THE SHIPPED SERVER. Compiled only with the `record-wire` feature, which only this
//! crate's own tests turn on (its dev-dependency on itself), and active only while
//! `OG_RECORD_WIRE` names a directory. It writes what it sees raw, one JSON line per frame or
//! body; placeholders, redaction and choosing one file per shape are the corpus builder's job,
//! so a secret this layer sees never leaves the scratch directory the script throws away.

use std::io::Write;
use std::path::PathBuf;

use axum::body::Body;
use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use futures::StreamExt;
use serde_json::{Value, json};

/// The directory records go under, when recording is on. Empty is off: the gate sets it empty
/// when it has no database to record against.
pub(crate) fn directory() -> Option<PathBuf> {
    std::env::var_os("OG_RECORD_WIRE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// A REST body larger than this is not recorded: nothing NativeChat reads is near it.
const BODY_LIMIT: usize = 16 * 1024 * 1024;

pub(crate) async fn record(request: Request, next: Next) -> Response {
    let Some(route) = request
        .extensions()
        .get::<MatchedPath>()
        .map(|matched| matched.as_str().to_string())
    else {
        return next.run(request).await;
    };
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let response = next.run(request).await;
    let status = response.status().as_u16();
    let streams = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));
    let (parts, body) = response.into_parts();
    if streams {
        // Teed frame by frame, so the stream still reaches the test as it is written.
        let mut pending = String::new();
        let stream = body.into_data_stream().map(move |chunk| {
            if let Ok(bytes) = &chunk {
                pending.push_str(&String::from_utf8_lossy(bytes));
                while let Some(end) = pending.find("\n\n") {
                    let event: String = pending.drain(..end + 2).collect();
                    for data in event.lines().filter_map(|line| line.strip_prefix("data:")) {
                        if let Ok(frame) = serde_json::from_str::<Value>(data.trim()) {
                            write(json!({ "kind": "agui", "route": route, "frame": frame }));
                        }
                    }
                }
            }
            chunk
        });
        return Response::from_parts(parts, Body::from_stream(stream));
    }
    let Ok(bytes) = axum::body::to_bytes(body, BODY_LIMIT).await else {
        return Response::from_parts(parts, Body::empty());
    };
    let body = serde_json::from_slice::<Value>(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    write(json!({
        "kind": "rest",
        "method": method,
        "route": route,
        "path": path,
        "status": status,
        "body": body,
    }));
    Response::from_parts(parts, Body::from(bytes))
}

/// The test running now: nextest runs one test per process and names it after `--exact`; a
/// plain `cargo test` names each test's thread.
fn test_name() -> String {
    let args: Vec<String> = std::env::args().collect();
    if let Some(at) = args.iter().position(|arg| arg == "--exact")
        && let Some(name) = args.get(at + 1)
    {
        return name.clone();
    }
    std::thread::current()
        .name()
        .filter(|name| *name != "main" && !name.starts_with("tokio-"))
        .map_or_else(|| "unknown".to_string(), str::to_string)
}

/// The test binary, without cargo's hash: `against_routines-3f2a…` → `against_routines`.
fn test_binary() -> String {
    let stem = std::env::args()
        .next()
        .and_then(|arg| {
            PathBuf::from(arg)
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    match stem.rsplit_once('-') {
        Some((name, hash)) if hash.chars().all(|ch| ch.is_ascii_hexdigit()) => name.to_string(),
        _ => stem,
    }
}

fn write(mut record: Value) {
    let Some(directory) = directory() else {
        return;
    };
    if let Some(object) = record.as_object_mut() {
        object.insert("test".to_string(), Value::String(test_name()));
        object.insert("binary".to_string(), Value::String(test_binary()));
    }
    let raw = directory.join("raw");
    if std::fs::create_dir_all(&raw).is_err() {
        return;
    }
    let file = raw.join(format!("{}.jsonl", std::process::id()));
    if let Ok(mut out) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
    {
        let _ = writeln!(out, "{record}");
    }
}
