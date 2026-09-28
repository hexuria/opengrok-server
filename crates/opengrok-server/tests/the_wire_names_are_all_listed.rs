//! `opengrok_wire::agui::SENT_TYPES` and `CUSTOM_NAMES` are what the wire corpus tells NativeChat
//! this server CAN send (#255). Its ledger is two-way, so a name missing here is a frame the app
//! was never checked against. This reads the non-test source of every crate and fails on a
//! `type` or CUSTOM `name` the lists do not hold, or on a listed one nothing produces.
//!
//! A SCAN, NOT A REGISTRY, on purpose: CUSTOM frames are built three ways (`.with("name", …)`,
//! `json!({"type": "CUSTOM", "name": …})` and `Projection::custom(NAME, …)`), in four crates, and
//! a new one is most likely written the way its neighbour was, not through a registry.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use opengrok_wire::agui::{CUSTOM_NAMES, EventType, SENT_TYPES};

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .to_path_buf()
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read_dir").flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// Each crate's `src/`, cut at a file's first top-level `#[cfg(test)]`: a test builds frames
/// the server never sends.
fn source() -> Vec<(PathBuf, String)> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(crates_dir()).expect("crates").flatten() {
        let src = entry.path().join("src");
        if src.is_dir() {
            rust_files(&src, &mut files);
        }
    }
    files
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path).expect("read");
            (path, producing_code(&text))
        })
        .collect()
}

/// The code that can produce a frame: comment lines dropped (a doc may name `EventType::Raw`
/// without anything sending it), the file cut at its test module rather than at any
/// `#[cfg(test)]` (a test-only helper mid-file would hide the producers after it), and the
/// `SENT_TYPES` initializer left out, which lists types rather than sends them (review of #258:
/// scanning it let a listed type nothing sends pass).
fn producing_code(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let test_module = lines.iter().enumerate().position(|(at, line)| {
        *line == "#[cfg(test)]"
            && lines[at + 1..]
                .iter()
                .find(|next| !next.starts_with("#["))
                .is_some_and(|next| next.starts_with("mod ") || next.starts_with("pub mod "))
    });
    let mut kept = Vec::new();
    let mut in_list = false;
    for line in &lines[..test_module.unwrap_or(lines.len())] {
        let code = line.trim_start();
        if code.starts_with("//") {
            continue;
        }
        if code.starts_with("pub const SENT_TYPES") {
            in_list = true;
        }
        if !in_list {
            kept.push(*line);
        }
        if in_list && code.starts_with("];") {
            in_list = false;
        }
    }
    kept.join("\n")
}

fn screaming(camel: &str) -> String {
    let mut out = String::new();
    for (i, ch) in camel.chars().enumerate() {
        if ch.is_ascii_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(ch.to_ascii_uppercase());
    }
    out
}

fn identifier_after(text: &str, at: usize) -> &str {
    let rest = &text[at..];
    let end = rest
        .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .unwrap_or(rest.len());
    &rest[..end]
}

/// A literal (`"x"`) or a constant (`NAME`, `path::NAME`) as written at `at`, resolved.
fn argument(text: &str, at: usize, consts: &BTreeMap<String, String>) -> Option<String> {
    let rest = text[at..].trim_start();
    if let Some(literal) = rest.strip_prefix('"') {
        return literal.split('"').next().map(str::to_string);
    }
    let path: String = rest
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == ':')
        .collect();
    let last = path.rsplit("::").next().unwrap_or_default();
    consts.get(last).cloned()
}

fn sent_types(files: &[(PathBuf, String)]) -> BTreeSet<String> {
    let spec: BTreeSet<String> = all_spec_types();
    let mut found = BTreeSet::new();
    for (_, text) in files {
        for (at, _) in text.match_indices("EventType::") {
            let variant = identifier_after(text, at + "EventType::".len());
            // `EventType::{…}` in a `use`, or a path in a comment, names no variant.
            if !variant.is_empty() {
                found.insert(screaming(variant));
            }
        }
        for (at, _) in text.match_indices("\"type\": \"") {
            let value = identifier_after(text, at + "\"type\": \"".len());
            if spec.contains(value) {
                found.insert(value.to_string());
            }
        }
    }
    found
}

fn all_spec_types() -> BTreeSet<String> {
    let text =
        std::fs::read_to_string(crates_dir().join("opengrok-wire/src/agui.rs")).expect("agui.rs");
    let start = text.find("pub enum EventType {").expect("enum");
    let body = &text[start..text[start..].find('}').map(|end| start + end).expect("end")];
    body.lines()
        .map(str::trim)
        .filter(|line| !line.starts_with("//") && line.ends_with(','))
        .map(|line| screaming(line.trim_end_matches(',')))
        .collect()
}

fn custom_names(files: &[(PathBuf, String)]) -> BTreeMap<String, Vec<String>> {
    let mut consts = BTreeMap::new();
    for (_, text) in files {
        for line in text.lines() {
            let line = line.trim();
            let Some(rest) = line
                .strip_prefix("pub const ")
                .or_else(|| line.strip_prefix("const "))
            else {
                continue;
            };
            if let Some((name, value)) = rest.split_once(": &str = \"") {
                consts.insert(name.to_string(), value.trim_end_matches("\";").to_string());
            }
        }
    }
    let mut found: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (path, text) in files {
        let mut record = |name: Option<String>| {
            if let Some(name) = name {
                found
                    .entry(name)
                    .or_default()
                    .push(path.display().to_string());
            }
        };
        for (at, _) in text.match_indices(".with(\"name\",") {
            record(argument(text, at + ".with(\"name\",".len(), &consts));
        }
        for (at, _) in text.match_indices(".custom(") {
            record(argument(text, at + ".custom(".len(), &consts));
        }
        for (at, _) in text.match_indices("\"name\":") {
            // Only a `name` inside a CUSTOM frame: the lines just above it say so.
            let before = &text[text[..at].rmatch_indices('\n').nth(5).map_or(0, |(i, _)| i)..at];
            if before.contains("\"CUSTOM\"") || before.contains("EventType::Custom") {
                record(argument(text, at + "\"name\":".len(), &consts));
            }
        }
    }
    found
}

#[test]
fn every_type_the_server_sends_is_listed() {
    let found = sent_types(&source());
    let listed: BTreeSet<String> = SENT_TYPES
        .iter()
        .map(|kind| {
            serde_json::to_value(kind)
                .expect("serialise")
                .as_str()
                .expect("a string")
                .to_string()
        })
        .collect();
    assert_eq!(
        found, listed,
        "SENT_TYPES must be exactly the AG-UI types the non-test source builds"
    );
    assert!(SENT_TYPES.contains(&EventType::Custom));
}

#[test]
fn every_custom_name_the_server_sends_is_listed() {
    let found = custom_names(&source());
    let listed: BTreeSet<String> = CUSTOM_NAMES.iter().map(|name| name.to_string()).collect();
    let names: BTreeSet<String> = found.keys().cloned().collect();
    assert_eq!(
        names, listed,
        "CUSTOM_NAMES must be exactly the CUSTOM names the non-test source sends: {found:#?}"
    );
}
