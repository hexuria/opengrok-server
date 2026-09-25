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

// ---- #198: the first org, admin and gateway key ------------------------------------------------

/// Every step file the setup README's table links, in order.
fn setup_steps() -> Vec<String> {
    read("docs/setup/README.md")
        .lines()
        .filter(|line| line.starts_with("| ") && line.contains("](") && line.contains(".md)"))
        .filter_map(|line| {
            let start = line.find("](")? + 2;
            let end = start + line[start..].find(')')?;
            Some(line[start..end].to_string())
        })
        .collect()
}

/// The flags `opengrok admin org create` accepts, read from the CLI itself so the doc and the
/// parser cannot drift: an unknown flag is REFUSED there, so a doc that invents one hands a new
/// operator an error as their first command.
fn org_create_flags() -> Vec<String> {
    let admin = read("crates/opengrok/src/admin.rs");
    let arm = admin
        .find(r#"Some("org") if args.get(1).map(String::as_str) == Some("create")"#)
        .expect("the org create arm");
    let list_start = arm + admin[arm..].find("&[").expect("its flag list") + 2;
    let list_end = list_start + admin[list_start..].find(']').expect("list end");
    admin[list_start..list_end]
        .split(',')
        .map(|flag| flag.trim().trim_matches('"').to_string())
        .collect()
}

#[test]
fn the_setup_chain_creates_the_first_org_and_admin() {
    let steps = setup_steps();
    let first_run = steps
        .iter()
        .find(|step| step.as_str() == "first-run.md")
        .unwrap_or_else(|| panic!("no first-run step in the chain: {steps:?}"));
    let page = read(&format!("docs/setup/{first_run}"));
    assert!(page.contains("opengrok admin org create"), "{first_run}");
    // Signup only redeems an invite and lands disabled; without this the page makes nobody.
    assert!(page.contains("--admin-email"), "{first_run}");
}

#[test]
fn every_documented_org_create_flag_is_one_the_cli_accepts() {
    let allowed = org_create_flags();
    assert!(allowed.contains(&"admin-email".to_string()), "{allowed:?}");
    let mut found_any = false;
    for entry in std::fs::read_dir(repo().join("docs/setup")).expect("docs/setup") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read");
        for line in text
            .lines()
            .filter(|line| line.contains("admin org create"))
        {
            found_any = true;
            for flag in line
                .split_whitespace()
                .filter_map(|word| word.strip_prefix("--"))
            {
                let flag = flag.trim_end_matches(|c: char| !c.is_ascii_alphanumeric());
                assert!(
                    allowed.iter().any(|known| known == flag),
                    "{}: --{flag} is not a flag `admin org create` accepts ({allowed:?})",
                    path.display()
                );
            }
        }
    }
    assert!(found_any, "no setup doc shows `opengrok admin org create`");
}

#[test]
fn the_setup_chain_mints_a_gateway_key_and_proves_a_real_turn() {
    let page = read("docs/setup/first-run.md");
    for needle in [
        "oag admin key create",
        "OG_GATEWAY_TOKEN",
        "reason=Passthrough",
        "/console",
        "/ag-ui",
    ] {
        assert!(
            page.contains(needle),
            "first-run.md never mentions {needle}"
        );
    }
    assert!(
        !page.contains("cursor_dev_session_token"),
        "a dev sign-in is not a way for an operator to log in"
    );
}

/// `OG_LOGIN_EMAIL` is read at boot and stored, and nothing reads it after that: it binds no
/// login and creates no account. Documenting it as the login path sent a first-run operator
/// looking for an account that was never made.
#[test]
fn og_login_email_is_not_documented_as_a_login_path() {
    let environment = read("docs/setup/environment.md");
    let row = line_starting(&environment, "| `OG_LOGIN_EMAIL` |");
    assert!(!row.contains("binds to"), "{row}");
    assert!(row.contains("first-run.md"), "{row}");
}
