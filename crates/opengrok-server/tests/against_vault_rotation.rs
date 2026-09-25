//! Rotating `OG_CREDENTIAL_KEK`, and noticing when it was lost.
//!
//! The key has been lost once already (docs/verification/door1/README.md): a reboot regenerated
//! it, every sealed row stopped opening, and nothing said so. These tests pin the three things
//! that make it survivable — every row records which key sealed it, a reseal moves rows to the
//! current key, and a row under a key this server does not hold is reported at boot and on
//! `/health` rather than discovered one failed reveal at a time.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL. The database is shared with every other
//! test, so each test uses keys it generated itself and ids under its own prefix, and asserts
//! membership rather than "all clear".

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use base64::Engine;
use opengrok_core::id::AccountId;
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::host_state::HostState;
use opengrok_store::{PgStore, SiteLoginWrite, Vault};
use sqlx::Row;

macro_rules! database_or_skip {
    () => {
        match std::env::var("OG_DATABASE_URL") {
            Ok(url) => opengrok_store::gate_database_or_panic(url),
            Err(_) => {
                eprintln!("skipping: OG_DATABASE_URL is not set");
                return;
            }
        }
    };
}

async fn store(url: &str) -> PgStore {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    PgStore::new(pool)
}

/// A key nobody else in the suite holds.
fn fresh_key() -> String {
    use rand::RngExt;
    let bytes: [u8; 32] = rand::rng().random();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn unique(prefix: &str) -> String {
    format!("{prefix}:{}:", uuid::Uuid::now_v7().simple())
}

async fn key_id_of(store: &PgStore, id: &str) -> Option<String> {
    sqlx::query("select key_id from secret_store where id = $1")
        .bind(id)
        .fetch_one(store.pool())
        .await
        .expect("the row")
        .try_get::<Option<String>, _>("key_id")
        .expect("key_id")
}

async fn raw_row(store: &PgStore, id: &str) -> (Vec<u8>, Vec<u8>, i64) {
    let row =
        sqlx::query("select nonce, ciphertext, updated_at_ms from secret_store where id = $1")
            .bind(id)
            .fetch_one(store.pool())
            .await
            .expect("the row");
    (
        row.try_get("nonce").unwrap(),
        row.try_get("ciphertext").unwrap(),
        row.try_get("updated_at_ms").unwrap(),
    )
}

/// A write that forgets the key id leaves the OLD id on a row with a NEW ciphertext, and that row
/// never opens again. So every writer is checked, not just one.
#[tokio::test]
async fn every_sealed_row_records_the_current_key_id() {
    let url = database_or_skip!();
    let store = store(&url).await;
    let vault = Vault::from_base64_key(&fresh_key()).expect("vault");
    let p = unique("key-id-test");

    let loose = format!("{p}loose");
    store
        .put_secret(&loose, &vault.seal(&loose, "v").unwrap(), 1)
        .await
        .expect("put");
    assert_eq!(
        key_id_of(&store, &loose).await.as_deref(),
        Some(vault.key_id())
    );

    let org = format!("org_{}", uuid::Uuid::now_v7().simple());
    store
        .set_org_computer_secret(&vault, &org, "ascii", "box_key", 1)
        .await
        .expect("org secret");
    assert_eq!(
        key_id_of(&store, &format!("org-computer:{org}:ascii"))
            .await
            .as_deref(),
        Some(vault.key_id())
    );

    let account = AccountId::new();
    let row = store
        .upsert_site_login(
            &vault,
            &account,
            &SiteLoginWrite {
                origin: "example.com",
                username: "ada",
                label: "",
                kind: "password",
                notes: "",
                password: Some("hunter2"),
                otpauth: Some("otpauth://totp/x?secret=JBSWY3DPEHPK3PXP"),
                passkey: None,
            },
            1,
        )
        .await
        .expect("site login");
    for id in [
        PgStore::site_login_secret_id(&account, &row.id),
        PgStore::site_login_code_id(&account, &row.id),
    ] {
        assert_eq!(
            key_id_of(&store, &id).await.as_deref(),
            Some(vault.key_id()),
            "{id}"
        );
    }

    // An overwrite under a new key moves the id with the ciphertext.
    let next = Vault::from_base64_key(&fresh_key()).expect("next vault");
    store
        .put_secret(&loose, &next.seal(&loose, "w").unwrap(), 2)
        .await
        .expect("overwrite");
    assert_eq!(
        key_id_of(&store, &loose).await.as_deref(),
        Some(next.key_id())
    );
    assert_eq!(
        store.open_credential(&next, &loose).await.expect("opens"),
        Some("w".to_string())
    );
}

/// Rows under the retired key move to the current one; a row no key opens is counted and left
/// byte-for-byte alone; running it again does nothing, which is what makes it resumable.
#[tokio::test]
async fn reseal_moves_retired_rows_to_the_current_key_and_can_be_resumed() {
    let url = database_or_skip!();
    let store = store(&url).await;
    let (old_kek, new_kek, stranger_kek) = (fresh_key(), fresh_key(), fresh_key());
    let old = Vault::from_base64_key(&old_kek).unwrap();
    let new = Vault::from_base64_key(&new_kek).unwrap();
    let stranger = Vault::from_base64_key(&stranger_kek).unwrap();
    let p = unique("reseal-test");

    let mut ids = Vec::new();
    for n in 0..3 {
        let id = format!("{p}old-{n}");
        store
            .put_secret(&id, &old.seal(&id, &format!("value {n}")).unwrap(), 1)
            .await
            .unwrap();
        ids.push(id);
    }
    // One row from before key ids, under the old key: a reseal is also how those get an id.
    let legacy = format!("{p}legacy");
    let mut sealed = old.seal(&legacy, "legacy value").unwrap();
    sealed.key_id = None;
    store.put_secret(&legacy, &sealed, 1).await.unwrap();
    ids.push(legacy.clone());
    let current = format!("{p}current");
    store
        .put_secret(&current, &new.seal(&current, "current").unwrap(), 1)
        .await
        .unwrap();
    let lost = format!("{p}lost");
    store
        .put_secret(&lost, &stranger.seal(&lost, "gone").unwrap(), 1)
        .await
        .unwrap();
    let lost_before = raw_row(&store, &lost).await;

    let ring = Vault::from_base64_keys(&new_kek, &[old_kek.as_str()]).unwrap();
    let report = store.reseal_secrets(&ring, &p).await.expect("reseal");
    assert_eq!(
        (report.resealed, report.already_current, report.raced),
        (4, 1, 0),
        "{report:?}"
    );
    assert_eq!(report.unopenable, vec![lost.clone()], "{report:?}");

    // The retired key can go now: the current key alone opens every moved row.
    for id in ids.iter().chain([&current]) {
        assert!(
            store.open_credential(&new, id).await.expect(id).is_some(),
            "{id} does not open under the current key alone"
        );
        assert_eq!(key_id_of(&store, id).await.as_deref(), Some(new.key_id()));
    }
    assert_eq!(
        store
            .open_credential(&new, &legacy)
            .await
            .unwrap()
            .as_deref(),
        Some("legacy value")
    );
    assert_eq!(
        raw_row(&store, &lost).await,
        lost_before,
        "a row nothing opens must never be rewritten or dropped"
    );
    assert_eq!(
        key_id_of(&store, &lost).await.as_deref(),
        Some(stranger.key_id())
    );

    let before: Vec<_> = futures_rows(&store, &ids).await;
    let again = store.reseal_secrets(&ring, &p).await.expect("reseal again");
    assert_eq!(
        (again.resealed, again.already_current, again.raced),
        (0, 5, 0),
        "{again:?}"
    );
    assert_eq!(again.unopenable, vec![lost]);
    assert_eq!(
        futures_rows(&store, &ids).await,
        before,
        "a second run must not rewrite rows that are already current"
    );
}

async fn futures_rows(store: &PgStore, ids: &[String]) -> Vec<(Vec<u8>, Vec<u8>, i64)> {
    let mut rows = Vec::new();
    for id in ids {
        rows.push(raw_row(store, id).await);
    }
    rows
}

/// The boot canary: a row under a key this server does not hold is named by its key id, and the
/// verdict says what to do. Rows opening under the current key are not reported.
#[tokio::test]
async fn the_check_names_a_key_this_server_does_not_have() {
    let url = database_or_skip!();
    let store = store(&url).await;
    let new = Vault::from_base64_key(&fresh_key()).unwrap();
    let stranger = Vault::from_base64_key(&fresh_key()).unwrap();
    let p = unique("check-test");
    let lost = format!("{p}lost");
    store
        .put_secret(&lost, &stranger.seal(&lost, "gone").unwrap(), 1)
        .await
        .unwrap();
    let mine = format!("{p}mine");
    store
        .put_secret(&mine, &new.seal(&mine, "here").unwrap(), 1)
        .await
        .unwrap();

    let check = store.vault_check(Some(&new)).await.expect("check");
    assert!(check.lost.contains_key(stranger.key_id()), "{check:?}");
    assert!(!check.lost.contains_key(new.key_id()), "{check:?}");
    assert!(check.current >= 1, "{check:?}");
    assert!(
        !check.failed.iter().any(|k| k == new.key_id()),
        "the current key's newest row opens: {check:?}"
    );
    let problem = check.problem().expect("a lost key is a problem");
    assert!(problem.contains("no longer has"), "{problem}");
    assert!(problem.contains("OG_CREDENTIAL_KEK_OLD"), "{problem}");

    // No vault at all while sealed rows exist: every one of them is unreadable, and that is a
    // problem too, not a quiet "connectors unavailable".
    let none = store
        .vault_check(None)
        .await
        .expect("check without a vault");
    assert!(!none.configured);
    assert!(
        none.problem()
            .is_some_and(|p| p.contains("OG_CREDENTIAL_KEK")),
        "{none:?}"
    );
}

fn state_over(store: PgStore, vault: Option<Vault>) -> AgUiState {
    AgUiState {
        auth: AuthState::new(
            store,
            Arc::new(TokenMinter::new(b"vault-rotation-secret")),
            "vault@og.local".to_string(),
        ),
        door: Arc::new(MockDoor::echoing()),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer: None,
        vault: vault.map(Arc::new),
        connectors: Connectors {
            providers: Arc::new(BTreeMap::new()),
            redirect_uri: "http://127.0.0.1/callback".to_string(),
        },
        plugins: Arc::new(BTreeMap::new()),
        host_settings: None,
    }
}

/// `/health` reports a lost key WITHOUT flipping `ok`: every supervisor and smoke reads only `ok`,
/// and a server whose saved logins will not open still runs coworkers. The reply is
/// unauthenticated, so it names the fix but no key id and no row id.
#[tokio::test]
async fn health_reports_a_lost_key_without_flipping_ok() {
    let url = database_or_skip!();
    let store = store(&url).await;
    let stranger = Vault::from_base64_key(&fresh_key()).unwrap();
    let lost = format!("{}lost", unique("health-test"));
    store
        .put_secret(&lost, &stranger.seal(&lost, "gone").unwrap(), 1)
        .await
        .unwrap();

    let vault = Vault::from_base64_key(&fresh_key()).unwrap();
    let agui = state_over(store, Some(vault));
    let gateway = HostState::new(agui.clone(), None);
    let app = opengrok_server::router(agui, gateway);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    let response = reqwest::get(format!("{base}/health")).await.expect("probe");
    assert_eq!(response.status().as_u16(), 200);
    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(body["ok"], serde_json::json!(true), "{body}");
    assert_eq!(body["vault"]["ok"], serde_json::json!(false), "{body}");
    assert_eq!(
        body["vault"]["configured"],
        serde_json::json!(true),
        "{body}"
    );
    let reason = body["vault"]["reason"].as_str().unwrap_or_default();
    assert!(reason.contains("no longer has"), "{body}");
    assert!(reason.contains("OG_CREDENTIAL_KEK_OLD"), "{body}");
    let whole = body.to_string();
    assert!(
        !whole.contains(stranger.key_id()),
        "no key id on an open door: {body}"
    );
    assert!(!whole.contains(&lost), "no row id on an open door: {body}");
}
