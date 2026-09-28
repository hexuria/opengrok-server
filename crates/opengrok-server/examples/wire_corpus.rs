//! Build the wire corpus NativeChat vendors (#255) from what `src/wire_record.rs` recorded, or
//! check a committed corpus against a fresh build. `scripts/record-wire.sh` drives both.
//!
//! ```text
//! wire_corpus build <record-dir> <out-dir> <server-sha>
//! wire_corpus check <built-dir> <committed-dir>
//! ```
//!
//! THE LAYOUT IS NATIVECHAT'S, not ours to tidy (non-negotiable #1): `agui/<TYPE>/<slug>.json`,
//! `agui/custom/<name>/<slug>.json`, `rest/<METHOD>_<route with / { } as _>/<status>-<slug>.json`
//! holding `{method, path, status, body}`, and `MANIFEST.json` with `server_sha`, `recorded_by`,
//! `emits`, `entries` and `unrecorded` (nativechat `src/opengrok/conformance.rs`).
//!
//! One file per distinct SHAPE (keys and value types), named after the lexicographically first
//! test that produced it, so a re-recording names the same shape the same way. Ids and clocks
//! become placeholders in the server's own formats, one per distinct value in a file, so equal
//! stays equal; secrets become `«redacted»`, never a placeholder that looks like a real key.
//! `check` compares shapes, not bytes: a value no test pins may differ between recordings, and
//! only a shape the committed copy does not have is a stale corpus.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use opengrok_core::run::SuspendReason;
use opengrok_tools::FormResolution;
use opengrok_wire::agui::{CUSTOM_NAMES, SENT_TYPES};
use serde_json::{Map, Value, json};

/// The REST routes NativeChat reads (#255's list). Any other route is recorded but not kept.
const REST_PREFIXES: &[&str] = &[
    "/auth",
    "/account",
    "/coworkers",
    "/ag-ui",
    "/local-exec",
    "/recipes",
    "/schedules",
    "/skills",
    // A person's attachments (#229): the upload and the thread's list a replay draws them from.
    "/artifacts",
];

/// Routes under those prefixes that are not JSON NativeChat reads: the screen proxy serves noVNC's
/// own pages and a websocket, and an artifact's `/bytes` is the file itself.
const REST_LEFT_OUT: &[&str] = &["/computer/vnc/", "/bytes"];

const REDACTED: &str = "«redacted»";

/// Keys whose value is a secret, whole.
const SECRET_KEYS: &[&str] = &[
    "key",
    "apikey",
    "api_key",
    "token",
    "accesstoken",
    "access_token",
    "refreshtoken",
    "refresh_token",
    "idtoken",
    "id_token",
    "password",
    "secret",
    "clientsecret",
    "client_secret",
    "cookie",
    "authorization",
    "gatewaykey",
    "bearer",
];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [verb, record, out, sha] if verb == "build" => {
            build(Path::new(record), Path::new(out), sha)
        }
        [verb, built, committed] if verb == "check" => {
            if !check(Path::new(built), Path::new(committed)) {
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!(
                "usage: wire_corpus build <record-dir> <out-dir> <sha> | check <built> <committed>"
            );
            std::process::exit(2);
        }
    }
}

struct Record {
    dir: String,
    status: Option<u16>,
    test: String,
    binary: String,
    route: String,
    method: String,
    content: Value,
}

fn records(record_dir: &Path) -> Vec<Record> {
    let spec_types: BTreeSet<String> = SENT_TYPES.iter().map(type_word).collect();
    let mut out = Vec::new();
    let raw = record_dir.join("raw");
    let Ok(entries) = std::fs::read_dir(&raw) else {
        return out;
    };
    for entry in entries.flatten() {
        let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
        for line in text.lines() {
            let Ok(raw) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let word = |key: &str| raw[key].as_str().unwrap_or_default().to_string();
            let test = word("test")
                .rsplit("::")
                .next()
                .unwrap_or_default()
                .to_string();
            let (binary, route, method) = (word("binary"), word("route"), word("method"));
            match raw["kind"].as_str() {
                Some("agui") => {
                    if let Some(record) = frame(&raw["frame"], &test, &binary, &route) {
                        out.push(record);
                    }
                }
                Some("rest") => {
                    if !REST_PREFIXES.iter().any(|prefix| route.starts_with(prefix))
                        || REST_LEFT_OUT.iter().any(|part| route.contains(part))
                    {
                        continue;
                    }
                    // A replay body carries the run's frames: each is a frame NativeChat reads too.
                    let mut frames = Vec::new();
                    collect_frames(&raw["body"], &spec_types, &mut frames);
                    for found in frames {
                        if let Some(record) = frame(&found, &test, &binary, &route) {
                            out.push(record);
                        }
                    }
                    let status = raw["status"].as_u64().unwrap_or_default() as u16;
                    let dir = format!("rest/{method}_{}", route.replace(['/', '{', '}', '*'], "_"));
                    out.push(Record {
                        dir,
                        status: Some(status),
                        test,
                        binary,
                        route,
                        method: method.clone(),
                        content: json!({
                            "method": method,
                            "path": raw["path"],
                            "status": status,
                            "body": raw["body"],
                        }),
                    });
                }
                _ => {}
            }
        }
    }
    out
}

fn frame(frame: &Value, test: &str, binary: &str, route: &str) -> Option<Record> {
    let kind = frame.get("type")?.as_str()?;
    let dir = if kind == "CUSTOM" {
        format!("agui/custom/{}", frame.get("name")?.as_str()?)
    } else {
        format!("agui/{kind}")
    };
    Some(Record {
        dir,
        status: None,
        test: test.to_string(),
        binary: binary.to_string(),
        route: route.to_string(),
        method: String::new(),
        content: frame.clone(),
    })
}

fn collect_frames(value: &Value, types: &BTreeSet<String>, out: &mut Vec<Value>) {
    match value {
        Value::Object(object) => {
            if object
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| types.contains(kind))
            {
                out.push(value.clone());
                return;
            }
            object
                .values()
                .for_each(|child| collect_frames(child, types, out));
        }
        Value::Array(items) => items
            .iter()
            .for_each(|child| collect_frames(child, types, out)),
        _ => {}
    }
}

fn type_word(kind: &opengrok_wire::agui::EventType) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Keys and value types, not values: what NativeChat's parsers depend on.
fn shape(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(_) => "bool".to_string(),
        Value::Number(_) => "number".to_string(),
        Value::String(_) => "string".to_string(),
        Value::Array(items) => {
            let kinds: BTreeSet<String> = items.iter().map(shape).collect();
            format!("[{}]", kinds.into_iter().collect::<Vec<_>>().join("|"))
        }
        Value::Object(object) => {
            let fields: BTreeMap<&String, String> = object
                .iter()
                .map(|(key, child)| (key, shape(child)))
                .collect();
            let inner: Vec<String> = fields
                .into_iter()
                .map(|(key, child)| format!("{key}:{child}"))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
    }
}

fn slug(test: &str) -> String {
    test.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn build(record_dir: &Path, out: &Path, sha: &str) {
    if !record_dir.join("raw").is_dir() {
        eprintln!(
            "nothing was recorded under {}: run the tests with OG_RECORD_WIRE set, against a database",
            record_dir.display()
        );
        std::process::exit(1);
    }
    let mut chosen: BTreeMap<(String, Option<u16>, String), Record> = BTreeMap::new();
    for record in records(record_dir) {
        let mut content = record.content.clone();
        normalise(&mut content);
        let key = (record.dir.clone(), record.status, shape(&content));
        let keep = chosen
            .get(&key)
            .is_none_or(|kept| (&record.test, &record.binary) < (&kept.test, &kept.binary));
        if keep {
            chosen.insert(key, Record { content, ..record });
        }
    }
    let _ = std::fs::remove_dir_all(out);
    std::fs::create_dir_all(out).expect("the corpus directory");
    let mut names: BTreeMap<String, usize> = BTreeMap::new();
    let mut entries = Vec::new();
    let mut words = String::new();
    for ((dir, status, _), record) in &chosen {
        let base = match status {
            Some(status) => format!("{status}-{}", slug(&record.test)),
            None => slug(&record.test),
        };
        let seen = names.entry(format!("{dir}/{base}")).or_insert(0);
        *seen += 1;
        let name = if *seen == 1 {
            base
        } else {
            format!("{base}-{seen}")
        };
        let file = format!("{dir}/{name}.json");
        let path = out.join(&file);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        let text = serde_json::to_string_pretty(&record.content).expect("json") + "\n";
        words.push_str(&text);
        std::fs::write(&path, text).expect("write");
        let origin = if record.method.is_empty() {
            format!("streamed or replayed by {}", record.route)
        } else {
            format!("{} {}", record.method, record.route)
        };
        entries.push(json!({
            "file": file,
            "source": format!(
                "{origin}, recorded from the test {} (crates/opengrok-server/tests/{}.rs); ids and times are placeholders",
                record.test, record.binary
            ),
        }));
    }
    let emits = emits();
    let unrecorded = unrecorded(&emits, &chosen, &words);
    let manifest = json!({
        "server_sha": sha,
        "recorded_by": "opengrok-server recorder",
        "emits": emits,
        "entries": entries,
        "unrecorded": unrecorded,
    });
    std::fs::write(
        out.join("MANIFEST.json"),
        serde_json::to_string_pretty(&manifest).expect("json") + "\n",
    )
    .expect("manifest");
    println!("{} fixtures, {} unrecorded", chosen.len(), unrecorded.len());
    // A route NativeChat reads that no test drives over HTTP is not in `emits` (routes are not
    // wire words), so it would be silently missing: say so, as the review of #258 found for
    // `/local-exec`, whose behaviour was tested beside its handlers.
    for prefix in REST_PREFIXES {
        let covered = chosen
            .values()
            .any(|record| record.status.is_some() && record.route.starts_with(prefix));
        if !covered {
            println!("note: no fixture for any {prefix} route");
        }
    }
}

fn emits() -> Value {
    // Exhaustive on purpose: a new reason or resolution fails to compile here until it is listed.
    let reasons = [
        SuspendReason::ExecConsent,
        SuspendReason::PolicyApproval,
        SuspendReason::AutoReview,
        SuspendReason::UserForm,
    ];
    for reason in reasons {
        match reason {
            SuspendReason::ExecConsent
            | SuspendReason::PolicyApproval
            | SuspendReason::AutoReview
            | SuspendReason::UserForm => {}
        }
    }
    let resolutions = [
        FormResolution::Submitted,
        FormResolution::FillFailed,
        FormResolution::Dismissed,
        FormResolution::Escalated,
    ];
    for resolution in resolutions {
        match resolution {
            FormResolution::Submitted
            | FormResolution::FillFailed
            | FormResolution::Dismissed
            | FormResolution::Escalated => {}
        }
    }
    json!({
        "agui_types": SENT_TYPES.iter().map(type_word).collect::<Vec<_>>(),
        "custom_names": CUSTOM_NAMES,
        "approval_reasons": reasons.iter().map(SuspendReason::as_str).collect::<Vec<_>>(),
        "form_resolutions": resolutions.iter().map(|resolution| resolution.as_str()).collect::<Vec<_>>(),
    })
}

/// Every word in `emits` that no fixture carries.
fn unrecorded(
    emits: &Value,
    chosen: &BTreeMap<(String, Option<u16>, String), Record>,
    written: &str,
) -> Vec<String> {
    let dirs: BTreeSet<&String> = chosen.keys().map(|(dir, _, _)| dir).collect();
    let mut missing = Vec::new();
    let words = |key: &str| -> Vec<String> {
        emits[key]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    for kind in words("agui_types") {
        if !dirs.contains(&format!("agui/{kind}")) && kind != "CUSTOM" {
            missing.push(kind);
        }
    }
    for name in words("custom_names") {
        if !dirs.contains(&format!("agui/custom/{name}")) {
            missing.push(name);
        }
    }
    for word in words("approval_reasons")
        .into_iter()
        .chain(words("form_resolutions"))
    {
        if !written.contains(&format!("\"{word}\"")) {
            missing.push(word);
        }
    }
    missing
}

fn normalise(value: &mut Value) {
    let mut ids: BTreeMap<String, String> = BTreeMap::new();
    let mut clocks: BTreeMap<String, i64> = BTreeMap::new();
    walk(value, None, &mut ids, &mut clocks);
}

fn walk(
    value: &mut Value,
    key: Option<&str>,
    ids: &mut BTreeMap<String, String>,
    clocks: &mut BTreeMap<String, i64>,
) {
    let lower = key.map(str::to_ascii_lowercase);
    match value {
        Value::Object(object) => {
            let keys: Vec<String> = object.keys().cloned().collect();
            for child_key in keys {
                if let Some(child) = object.get_mut(&child_key) {
                    walk(child, Some(&child_key), ids, clocks);
                }
            }
        }
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| walk(item, key, ids, clocks)),
        Value::String(text) => {
            if lower
                .as_deref()
                .is_some_and(|key| SECRET_KEYS.contains(&key))
            {
                *text = REDACTED.to_string();
            } else if lower.as_deref() == Some("base64") {
                *text = "AAAA".to_string();
            } else if is_iso_instant(text) {
                *text = "2026-09-01T00:00:00Z".to_string();
            } else if lower.as_deref().is_some_and(|key| key.ends_with("prefix")) {
                // A key's first characters, shown so a person can tell keys apart: not the key.
                *text = placeholder_ids(text, ids);
            } else {
                *text = stable(&redact_query(&redact_inline(&placeholder_ids(text, ids))));
            }
        }
        Value::Number(number) => {
            let Some(key) = key else { return };
            if let Some(fixed) = measured(key) {
                *value = json!(fixed);
            } else if key == "timestamp" {
                *value = json!(100);
            } else if is_instant(key) {
                let next = 1_790_000_000_000 + clocks.len() as i64 * 1000;
                let fixed = *clocks.entry(number.to_string()).or_insert(next);
                *value = json!(fixed);
            }
        }
        _ => {}
    }
}

/// A duration a test measures rather than sets, fixed to a small one in its own unit so that
/// recording the same code twice writes the same corpus: NativeChat pins it by `server_sha`, and
/// a sha that names two corpora pins nothing. A limit (`max_wall_ms`) is configuration, and kept.
fn measured(key: &str) -> Option<i64> {
    match key {
        "total_ms" | "model_ms" | "tool_wait_ms" | "auto_review_ms" | "ms" => Some(3),
        "retryAfterSecs" => Some(60),
        _ => None,
    }
}

/// `2026-09-27T04:52:06Z` and the like: an instant written as a date.
fn is_iso_instant(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() >= 20
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[..4].iter().all(u8::is_ascii_digit)
}

/// A point in time, not a duration, count or limit. Only instants get a clock placeholder:
/// `model_ms`, `total_ms` and a budget's `max_wall_ms` are durations, and an epoch-sized one is a
/// 57-year turn that NativeChat's `TurnTiming` rightly refuses to parse.
fn is_instant(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    lower == "timestampms"
        || lower.ends_with("atms")
        || lower.ends_with("at_ms")
        || lower.ends_with("at")
        || lower.ends_with("untilms")
        || lower.ends_with("until_ms")
        || lower.ends_with("duems")
        || lower.ends_with("due_ms")
}

/// `Bearer <secret>`, gateway keys, and a JWT anywhere in a string: a screen ticket rides
/// inside a URL path, not as a word of its own.
fn redact_inline(text: &str) -> String {
    let mut out = Vec::new();
    let mut after_bearer = false;
    for word in text.split(' ') {
        let secret = after_bearer || word.starts_with("oag_live_") || word.starts_with("oag_test_");
        after_bearer = word == "Bearer";
        out.push(if secret {
            REDACTED.to_string()
        } else {
            redact_jwts(word)
        });
    }
    out.join(" ")
}

/// What a test mints at random and nothing reads for meaning: a loopback server's port and a
/// taught skill's generated name. Fixed, so recording the same code twice differs only where a
/// test's timing does.
fn stable(text: &str) -> String {
    let mut out = text.to_string();
    for host in ["127.0.0.1:", "localhost:"] {
        let mut from = 0;
        while let Some(found) = out[from..].find(host) {
            let port = from + found + host.len();
            let end = out[port..]
                .find(|ch: char| !ch.is_ascii_digit())
                .map_or(out.len(), |offset| port + offset);
            if end > port {
                out.replace_range(port..end, "1447");
            }
            from = port;
        }
    }
    if let Some(rest) = out.strip_prefix("taught-")
        && !rest.is_empty()
        && rest.chars().all(|ch| ch.is_ascii_hexdigit())
    {
        out = "taught-0199bb4e".to_string();
    }
    out
}

/// A secret-named query parameter's value in a URL: a screen URL carries the box's VNC password.
fn redact_query(text: &str) -> String {
    let mut out = text.to_string();
    for name in [
        "password",
        "token",
        "key",
        "secret",
        "ticket",
        "access_token",
    ] {
        let mut from = 0;
        let pattern = format!("{name}=");
        while let Some(found) = out[from..].find(&pattern) {
            let start = from + found;
            let preceded = start == 0 || matches!(out.as_bytes()[start - 1], b'?' | b'&' | b'#');
            let value = start + pattern.len();
            if !preceded {
                from = value;
                continue;
            }
            let end = out[value..]
                .find(['&', '#', '"', ' '])
                .map_or(out.len(), |offset| value + offset);
            out.replace_range(value..end, REDACTED);
            from = value + REDACTED.len();
        }
    }
    out
}

/// Every `eyJ…` run of base64url with two dots in it becomes `«redacted»`.
fn redact_jwts(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("eyJ") {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let end = tail
            .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.'))
            .unwrap_or(tail.len());
        let token = &tail[..end];
        out.push_str(if token.matches('.').count() >= 2 {
            REDACTED
        } else {
            token
        });
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// UUIDs, hyphenated or simple, become `0199bb4e-0000-7000-8000-…` in the order they appear, so
/// a prefix (`cw_`, `acct_`) and an id that recurs keep their meaning.
fn placeholder_ids(text: &str, ids: &mut BTreeMap<String, String>) -> String {
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut at = 0;
    while at < bytes.len() {
        let (hyphenated, simple) = (uuid_at(bytes, at, true), uuid_at(bytes, at, false));
        let length = if hyphenated {
            36
        } else if simple {
            32
        } else {
            0
        };
        if length == 0 {
            let ch = text[at..].chars().next().expect("char");
            out.push(ch);
            at += ch.len_utf8();
            continue;
        }
        let original = text[at..at + length].to_ascii_lowercase();
        let index = ids.len() + 1;
        let fixed = ids
            .entry(original)
            .or_insert_with(|| format!("0199bb4e-0000-7000-8000-{index:012}"))
            .clone();
        out.push_str(&if hyphenated {
            fixed
        } else {
            fixed.replace('-', "")
        });
        at += length;
    }
    out
}

fn uuid_at(bytes: &[u8], at: usize, hyphenated: bool) -> bool {
    let groups: &[usize] = if hyphenated { &[8, 4, 4, 4, 12] } else { &[32] };
    let length = if hyphenated { 36 } else { 32 };
    if at + length > bytes.len() {
        return false;
    }
    // Not the tail of a longer hex run.
    if at > 0 && bytes[at - 1].is_ascii_hexdigit() {
        return false;
    }
    let mut position = at;
    for (index, group) in groups.iter().enumerate() {
        if index > 0 {
            if bytes[position] != b'-' {
                return false;
            }
            position += 1;
        }
        if !bytes[position..position + group]
            .iter()
            .all(u8::is_ascii_hexdigit)
        {
            return false;
        }
        position += group;
    }
    bytes
        .get(position)
        .is_none_or(|next| !next.is_ascii_hexdigit())
}

/// Every field path and its type in `value`, arrays merged: `body[].lastRun.status:string`.
/// A `null` adds no path, so a field seen as an object in one recording and `null` in another
/// is the same corpus.
fn paths(value: &Value, at: &str, out: &mut BTreeSet<String>) {
    match value {
        Value::Null => {}
        Value::Object(object) => {
            out.insert(format!("{at}:object"));
            for (key, child) in object {
                paths(child, &format!("{at}.{key}"), out);
            }
        }
        Value::Array(items) => {
            out.insert(format!("{at}:array"));
            items
                .iter()
                .for_each(|item| paths(item, &format!("{at}[]"), out));
        }
        Value::Bool(_) => {
            out.insert(format!("{at}:bool"));
        }
        Value::Number(_) => {
            out.insert(format!("{at}:number"));
        }
        Value::String(_) => {
            out.insert(format!("{at}:string"));
        }
    }
}

/// The corpus as NativeChat depends on it: for each frame type (`agui/<TYPE>`, a CUSTOM by
/// name) and each route and status (`rest/<dir> <status>`), every field path its fixtures show.
fn groups(dir: &Path) -> BTreeMap<String, BTreeSet<String>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in std::fs::read_dir(&next).into_iter().flatten().flatten() {
            let path: PathBuf = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.file_name().is_some_and(|name| name == "MANIFEST.json") {
                continue;
            }
            let value: Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_default())
                    .unwrap_or(Value::Null);
            let parent = path
                .parent()
                .and_then(|parent| parent.strip_prefix(dir).ok())
                .map(|parent| parent.display().to_string())
                .unwrap_or_default();
            let group = if parent.starts_with("rest") {
                let status = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.split('-').next())
                    .unwrap_or_default()
                    .to_string();
                format!("{parent} {status}")
            } else {
                parent
            };
            paths(&value, "", out.entry(group).or_default());
        }
    }
    out
}

fn emits_of(dir: &Path) -> Value {
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("MANIFEST.json")).unwrap_or_default(),
    )
    .unwrap_or(Value::Null);
    let mut kept = Map::new();
    kept.insert("emits".to_string(), manifest["emits"].clone());
    kept.insert("unrecorded".to_string(), manifest["unrecorded"].clone());
    Value::Object(kept)
}

/// True when `committed` still shows NativeChat everything a fresh build does: every frame type,
/// route and status, and every field path in them. A path or group the fresh build lacks is only
/// reported: a test that reads a routine while its run is in flight sees `lastRun` as `null` or
/// an object by timing, and failing on that would fail the gate at random.
fn check(built: &Path, committed: &Path) -> bool {
    let (fresh, kept) = (groups(built), groups(committed));
    // AN EMPTY RECORDING PROVES NOTHING. With every group missing only noted, a run that recorded
    // nothing (no database, no OG_RECORD_WIRE) would call any corpus current (review of #258).
    if fresh.is_empty() {
        eprintln!(
            "the fresh recording has no fixtures, so it cannot vouch for the committed corpus"
        );
        return false;
    }
    let mut fine = true;
    for (group, fields) in &fresh {
        let Some(known) = kept.get(group) else {
            eprintln!("not in the committed corpus: {group}");
            fine = false;
            continue;
        };
        for field in fields.difference(known) {
            eprintln!("new in {group}: {field}");
            fine = false;
        }
        for field in known.difference(fields) {
            eprintln!("note: not seen this time in {group}: {field}");
        }
    }
    for group in kept.keys().filter(|group| !fresh.contains_key(*group)) {
        eprintln!("note: not recorded this time: {group}");
    }
    if emits_of(built)["emits"] != emits_of(committed)["emits"] {
        eprintln!("MANIFEST emits changed");
        fine = false;
    }
    if !fine {
        eprintln!(
            "the wire corpus is stale: run scripts/record-wire.sh and commit tests/fixtures/wire/"
        );
    }
    fine
}
