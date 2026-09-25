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

// ---- #200: a database a demo can live in ---------------------------------------------------------

/// The gateway's dev container keeps PGDATA in its writable layer, so a Docker restart wiped
/// every database on it. The documented home for data that matters has to survive that — and the
/// mount has to match the image's major version: from 18 the image's VOLUME is
/// `/var/lib/postgresql` (PGDATA is `/var/lib/postgresql/18/docker`), and a mount at the old
/// `…/data` path does not hold the cluster.
#[test]
fn the_documented_postgres_keeps_its_data_across_a_restart() {
    let postgres = read("docs/setup/postgres.md");
    assert!(
        !postgres.contains("The fix worth making\nonce")
            && !postgres.contains("fix worth making once"),
        "the trap is still only described, never fixed"
    );
    assert!(
        postgres.contains("postgres:18"),
        "name the image version the mount is for"
    );
    assert!(
        postgres.contains("-v opengrok-pgdata:/var/lib/postgresql "),
        "a named volume at the 18+ mount point"
    );
    assert!(
        !postgres.contains("opengrok-pgdata:/var/lib/postgresql/data"),
        "the pre-18 path does not hold an 18 cluster"
    );
    assert!(postgres.contains("--restart unless-stopped"));
}

#[test]
fn there_is_a_backup_and_restore_runbook() {
    let postgres = read("docs/setup/postgres.md");
    for needle in [
        "pg_dump",
        "pg_restore",
        "OG_CREDENTIAL_KEK",
        "OG_TOKEN_SECRET",
    ] {
        assert!(
            postgres.contains(needle),
            "postgres.md never mentions {needle}"
        );
    }
}

#[test]
fn data_transforming_migrations_have_a_documented_approach() {
    let postgres = read("docs/setup/postgres.md");
    assert!(postgres.contains("\n## Data-transforming migrations\n"));
    let migrations = read("crates/opengrok-store/src/migrations.rs");
    let module_doc: String = migrations
        .lines()
        .take_while(|line| line.starts_with("//!"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        module_doc.contains("Data-transforming migrations"),
        "the schema's own doc points at the approach before anyone writes one"
    );
}

/// The side databases the smokes are handed, as the setup doc creates them and as CI does. A
/// fourth one (slice 21) was added to CI and the gate and never to the doc, so a local gate run
/// that followed the doc failed at slice 21 with a word about Postgres, not about the doc.
#[test]
fn the_gate_databases_the_doc_creates_are_the_ones_ci_creates() {
    let for_db = |text: &str| -> Vec<String> {
        let line = text
            .lines()
            .find(|line| line.trim_start().starts_with("for db in "))
            .expect("a `for db in` loop");
        line.split_whitespace()
            .filter(|word| {
                word.starts_with("opengrok_s") && word.trim_end_matches(';').ends_with("_gate")
            })
            .map(|word| word.trim_end_matches(';').to_string())
            .collect()
    };
    let ci = for_db(&read(".github/workflows/ci.yml"));
    let doc = for_db(&read("docs/setup/postgres.md"));
    assert!(!ci.is_empty());
    assert_eq!(doc, ci, "docs/setup/postgres.md and ci.yml disagree");
}

// ---- #202: onboarding onto the client we actually serve --------------------------------------

/// The banner `6714eb5` gave the removed client's own reference, which PORT-PRIORITY — a plan made
/// entirely of that client's commands — never got.
#[test]
fn port_priority_carries_the_removal_banner() {
    let banner_line = read("docs/research/client-grok-bot.md")
        .lines()
        .find(|line| line.starts_with("> **"))
        .expect("client-grok-bot.md's banner")
        .to_string();
    // The bold lead sentence; what follows it names each document's own doors.
    let close = banner_line[4..].find("**").expect("the lead's closing **") + 6;
    let banner = &banner_line[..close];
    let head: String = read("docs/PORT-PRIORITY.md")
        .lines()
        .take(10)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        head.contains(banner),
        "PORT-PRIORITY.md's head lacks: {banner}"
    );
}

#[test]
fn handover_is_current() {
    let handover = read("docs/HANDOVER.md");
    for stale in [
        "Open Grok.app",
        "Rewritten 2 Sep 2026",
        "**#61",
        "desktop-client",
    ] {
        assert!(
            !handover.contains(stale),
            "HANDOVER.md still says {stale:?}"
        );
    }
    assert!(handover.contains("NativeChat"));
    assert!(handover.contains("setup/nativechat.md"));
}

/// P0-E removed prost, tonic and the `opengrok-proto` crate with seam B; a stack table that still
/// names them sends a newcomer looking for a gRPC service that is not there.
#[test]
fn goal_stack_matches_the_code() {
    let goal = read("docs/GOAL.md");
    let rpc = line_starting(&goal, "| RPC |");
    assert!(
        !rpc.contains("**tonic + prost**"),
        "the decision cell: {rpc}"
    );
    assert!(rpc.contains("AG-UI"), "{rpc}");
    for entry in std::fs::read_dir(repo().join("crates")).expect("crates") {
        let manifest = entry.expect("entry").path().join("Cargo.toml");
        let text = std::fs::read_to_string(&manifest).unwrap_or_default();
        for dependency in ["tonic", "prost"] {
            assert!(
                !text
                    .lines()
                    .any(|line| line.trim_start().starts_with(dependency)),
                "{} depends on {dependency} again; GOAL.md's stack row needs to say so",
                manifest.display()
            );
        }
    }
}

#[test]
fn roadmap_reflects_p0e_and_shipped_artifacts() {
    let roadmap = read("docs/ROADMAP.md");
    assert!(
        !roadmap
            .lines()
            .any(|line| line.starts_with("- [ ] **16.later**")),
        "16.later says nothing else is pending and is still unticked"
    );
    assert!(
        !roadmap
            .lines()
            .any(|line| line.contains("Artifacts/uploads") && line.contains("parked")),
        "the artifact store shipped in cf0a512"
    );
    for heading in ["## Slice 7 ", "## Slice 8 ", "## Slice 9 "] {
        let line = line_starting(&roadmap, heading);
        assert!(line.contains("removed in P0-E"), "{line}");
    }
    for tier in ["- [x] P5 ", "- [x] P6 ", "- [x] P7 ", "- [x] P8 "] {
        let line = line_starting(&roadmap, tier);
        assert!(line.contains("removed in P0-E"), "{line}");
    }
}

#[test]
fn nativechat_has_a_setup_page_in_the_chain() {
    assert!(setup_steps().iter().any(|step| step == "nativechat.md"));
    let page = read("docs/setup/nativechat.md");
    for needle in [
        "tls.md",
        "first-run.md",
        "POST /ag-ui",
        "/ag-ui/approvals",
        "/coworkers/{id}/computer",
        "POST /schedules",
        "client-nativechat.md",
    ] {
        assert!(
            page.contains(needle),
            "nativechat.md never mentions {needle}"
        );
    }
    let readme = read("README.md");
    assert!(
        !readme.contains("Grok Bot desktop app connects"),
        "README.md still onboards onto the removed client"
    );
}

/// Every path the server mounts, read from the source: `.route(` then the first string literal,
/// which also catches the multi-line calls (`/ag-ui/host-settings`, `/.well-known/*`, …).
fn mounted_paths() -> Vec<String> {
    fn walk(dir: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).expect("dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                walk(&path, out);
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read");
            let text = text.split("#[cfg(test)]").next().unwrap_or_default();
            for (at, _) in text.match_indices(".route(") {
                let rest = text[at + ".route(".len()..].trim_start();
                if let Some(literal) = rest.strip_prefix('"')
                    && let Some(end) = literal.find('"')
                {
                    out.push(literal[..end].to_string());
                }
            }
        }
    }
    let mut paths = Vec::new();
    walk(&repo().join("crates/opengrok-server/src"), &mut paths);
    paths.push("/mcp".to_string());
    paths.sort();
    paths.dedup();
    paths
}

/// The route map is held to the router: a route added without a row, or a row whose NativeChat
/// cell is blank, fails here. A blank cell is the dangerous one — it reads as "nobody calls this"
/// when the truth is "nobody has looked".
#[test]
fn every_route_the_server_mounts_is_in_the_route_map() {
    let map = read("docs/research/client-nativechat.md");
    let paths = mounted_paths();
    assert!(
        paths.len() > 90,
        "the route walk found only {}",
        paths.len()
    );
    let missing: Vec<&String> = paths
        .iter()
        .filter(|path| !map.contains(&format!("`{path}`")))
        .collect();
    assert!(missing.is_empty(), "routes with no row: {missing:?}");
    let rows: Vec<&str> = map
        .lines()
        .filter(|line| line.starts_with("| `/"))
        .collect();
    assert!(!rows.is_empty());
    for row in rows {
        let cells: Vec<&str> = row.split(" | ").collect();
        assert!(
            cells.len() >= 5,
            "a row needs route, methods, mounted at, reply, NativeChat: {row}"
        );
        let nativechat = cells[cells.len() - 1].trim_end_matches('|').trim();
        assert!(!nativechat.is_empty(), "blank NativeChat cell: {row}");
    }
}
