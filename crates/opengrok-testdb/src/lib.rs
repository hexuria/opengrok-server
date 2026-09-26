//! The database a Postgres integration test may use, and the one it does use.
//!
//! Test support that every crate's integration tests reach for (through `opengrok-store`'s
//! re-export), kept out of the store so it is not counted against the store's size ceiling and
//! does not grow it.

/// Whether a database URL names a database the integration tests may use: one whose name ends
/// in `_gate`. The dev database once collected 2,521 fixture accounts because a shell had the
/// live URL exported when the suite ran; a test that refuses anything else cannot do that again.
pub fn is_test_database_url(url: &str) -> bool {
    let path = url.split('?').next().unwrap_or(url);
    let name = path.rsplit('/').next().unwrap_or("");
    !name.is_empty() && name.ends_with("_gate")
}

/// The database this test binary uses, or a panic that says why the tests will not run against
/// `url`. For the top of every Postgres integration test: the one place a live database could
/// sneak in.
///
/// EACH TEST BINARY GETS ITS OWN DATABASE beside `url`: `opengrok_gate` becomes
/// `opengrok_against_monitors_gate`, created on first use. Several tests act on the whole
/// database, not on rows they made: a purge deletes everyone off its allowlist, and the monitor
/// sweep walks one cursor over every event. `cargo test` runs binaries one after another, which
/// hid it. nextest runs them at once, and two such tests failed about one run in two. Tests
/// inside one binary still share a database, as they always have. The role in `url` needs
/// CREATEDB; the gate's `oag` and CI's service user have it.
pub fn gate_database_or_panic(url: String) -> String {
    assert!(
        is_test_database_url(&url),
        "refusing to run tests against {url}: OG_DATABASE_URL must name a database whose name ends in _gate (the dev database is not a test fixture)",
    );
    static OWN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    OWN.get_or_init(|| {
        let binary = std::env::current_exe()
            .ok()
            .and_then(|exe| {
                exe.file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
            })
            .unwrap_or_default();
        let own = binary_database_url(&url, &binary);
        if own != url {
            create_database_blocking(&url, &own);
        }
        own
    })
    .clone()
}

/// `url` with its database renamed for one test binary. Cargo names a test binary
/// `<target>-<16 hex>`; the hash is dropped so every build of the binary reuses its database.
/// Postgres truncates names at 63 bytes, so a long one keeps its tail, which ends in `_gate`.
fn binary_database_url(url: &str, binary: &str) -> String {
    let (path, query) = match url.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (url, None),
    };
    let Some((server, name)) = path.rsplit_once('/') else {
        return url.to_string();
    };
    let target = match binary.rsplit_once('-') {
        Some((target, hash)) if hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()) => {
            target
        }
        _ => binary,
    };
    let target: String = target
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    if target.is_empty() {
        return url.to_string();
    }
    let base = name.strip_suffix("_gate").unwrap_or(name);
    let mut own = format!("{base}_{target}_gate");
    if own.len() > 63 {
        own = own[own.len() - 63..].trim_start_matches('_').to_string();
    }
    match query {
        Some(query) => format!("{server}/{own}?{query}"),
        None => format!("{server}/{own}"),
    }
}

/// `create database` for `own`, through `shared`, unless it exists. Called from inside a test's
/// async runtime, so it runs on a thread of its own with its own runtime. An advisory lock
/// serialises the creates: Postgres refuses a CREATE DATABASE while another one is copying
/// template1, and every binary starts at once.
fn create_database_blocking(shared: &str, own: &str) {
    let shared = shared.to_string();
    let name = own
        .split('?')
        .next()
        .and_then(|path| path.rsplit('/').next())
        .unwrap_or_default()
        .to_string();
    let created = std::thread::spawn(move || -> Result<(), String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        runtime.block_on(async {
            use sqlx::Connection;
            let mut admin = sqlx::PgConnection::connect(&shared)
                .await
                .map_err(|e| format!("connect to {shared}: {e}"))?;
            sqlx::query("select pg_advisory_lock(7401)")
                .execute(&mut admin)
                .await
                .map_err(|e| e.to_string())?;
            let exists: bool =
                sqlx::query_scalar("select exists (select 1 from pg_database where datname = $1)")
                    .bind(&name)
                    .fetch_one(&mut admin)
                    .await
                    .map_err(|e| e.to_string())?;
            if !exists {
                // CREATE DATABASE takes no bind parameter; the name is built from a URL that
                // has passed is_test_database_url and from the binary's own file name.
                sqlx::query(sqlx::AssertSqlSafe(format!("create database \"{name}\"")))
                    .execute(&mut admin)
                    .await
                    .map_err(|e| format!("create database {name}: {e}"))?;
            }
            sqlx::query("select pg_advisory_unlock(7401)")
                .execute(&mut admin)
                .await
                .map_err(|e| e.to_string())?;
            Ok(())
        })
    })
    .join();
    let failure = match created {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(error),
        Err(_) => Some(format!("creating the test database {own} panicked")),
    };
    assert!(failure.is_none(), "{}", failure.unwrap_or_default());
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    #[test]
    fn only_a_gate_database_is_a_test_database() {
        use super::is_test_database_url;
        assert!(is_test_database_url(
            "postgres://oag:oag@127.0.0.1:5452/opengrok_gate"
        ));
        assert!(is_test_database_url(
            "postgres://oag:oag@127.0.0.1:5452/opengrok_gate?sslmode=disable"
        ));
        assert!(!is_test_database_url(
            "postgres://oag:oag@127.0.0.1:5455/opengrok_web_verify"
        ));
        assert!(!is_test_database_url(
            "postgres://oag:oag@127.0.0.1:5455/opengrok_gate_backup"
        ));
        assert!(!is_test_database_url("postgres://oag:oag@127.0.0.1:5455/"));
    }

    #[test]
    #[should_panic(expected = "ends in _gate")]
    fn a_live_database_url_panics_before_any_test_runs() {
        super::gate_database_or_panic(
            "postgres://oag:oag@127.0.0.1:5455/opengrok_web_verify".to_string(),
        );
    }

    #[test]
    fn each_test_binary_gets_a_gate_database_of_its_own() {
        use super::{binary_database_url, is_test_database_url};
        let url = "postgres://oag:oag@127.0.0.1:5432/opengrok_gate";
        assert_eq!(
            binary_database_url(url, "against_monitors-0123456789abcdef"),
            "postgres://oag:oag@127.0.0.1:5432/opengrok_against_monitors_gate"
        );
        // A unit-test binary, the hash kept off, the query string kept on.
        assert_eq!(
            binary_database_url(
                &format!("{url}?sslmode=disable"),
                "opengrok_store-fedcba9876543210"
            ),
            "postgres://oag:oag@127.0.0.1:5432/opengrok_opengrok_store_gate?sslmode=disable"
        );
        // Not cargo's hash shape: the whole stem is the name.
        assert_eq!(
            binary_database_url(url, "my-tests"),
            "postgres://oag:oag@127.0.0.1:5432/opengrok_my_tests_gate"
        );
        // Too long for Postgres: the tail is kept, and it is still a gate database.
        let long = binary_database_url(url, &"x".repeat(80));
        let name = long.rsplit('/').next().unwrap();
        assert!(name.len() <= 63 && is_test_database_url(&long), "{long}");
        // No binary name: the URL as given.
        assert_eq!(binary_database_url(url, ""), url);
    }
}
