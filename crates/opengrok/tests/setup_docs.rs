//! The setup docs and `.env.example`, held to the code they describe.
//!
//! These are the files a person follows with nobody watching, which is exactly when a stale line
//! costs the most: the default model that cannot call tools, the setup chain that never makes an
//! admin, the database that forgets everything on a restart. Each test pins one such promise to
//! the thing that keeps it. Plain `std`; no Postgres, no network.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::path::{Path, PathBuf};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(relative: &str) -> String {
    std::fs::read_to_string(repo().join(relative))
        .unwrap_or_else(|error| panic!("{relative}: {error}"))
}

fn line_starting<'a>(text: &'a str, prefix: &str) -> &'a str {
    text.lines()
        .find(|line| line.starts_with(prefix))
        .unwrap_or_else(|| panic!("no line starts with {prefix:?}"))
}

// ---- #197: the default route ------------------------------------------------------------------

/// `scripts/serve.sh` sources `.env`, which starts life as a copy of `.env.example` — so this line,
/// not `DEFAULT_MODEL`, is the route a fresh install actually hires on.
#[test]
fn env_example_pins_a_provider_qualified_tool_calling_model() {
    let env = read(".env.example");
    let pin = line_starting(&env, "OG_MODEL=")
        .trim_start_matches("OG_MODEL=")
        .trim();
    assert!(
        pin.contains('/'),
        "a bare name never matches an advertised id: {pin}"
    );
    assert!(
        !pin.contains("luna"),
        "luna makes zero tool calls through the gateway (ROADMAP): {pin}"
    );
}

#[test]
fn environment_md_names_the_verified_default_and_its_evidence() {
    let environment = read("docs/setup/environment.md");
    let row = line_starting(&environment, "| `OG_MODEL` |");
    let default = row.split('|').nth(2).unwrap_or_default().trim();
    assert_eq!(default, "`xai/grok-4.6`", "the Default column: {row}");
    assert!(
        row.contains("../verification/"),
        "a verified default links its evidence: {row}"
    );
}
