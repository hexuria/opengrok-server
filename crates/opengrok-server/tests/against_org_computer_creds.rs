//! The per-org computer-credential store: seal a box.ascii.dev key, see it listed as configured,
//! open it back to the plaintext, and clear it. Reuses the generic sealed `secret_store`, so this
//! proves the org-scoped key layer round-trips through the vault. Needs Postgres; skips loudly
//! without it.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use opengrok_store::{PgStore, Vault};

macro_rules! database_or_skip {
    () => {
        match std::env::var("OG_DATABASE_URL") {
            Ok(url) => url,
            Err(_) => {
                eprintln!("skipping: OG_DATABASE_URL is not set");
                return;
            }
        }
    };
}

async fn store(database_url: &str) -> PgStore {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(database_url)
        .await
        .expect("connect");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    PgStore::new(pool)
}

fn vault() -> Vault {
    // A throwaway KEK for the test — 32 bytes base64.
    Vault::from_base64_key("rIeYsJHlXEYIoRjZQfL73u7UuVMYxIrdlDT5tndh/kY=").expect("vault")
}

#[tokio::test]
async fn an_org_box_key_seals_lists_opens_and_clears() {
    let database_url = database_or_skip!();
    let store = store(&database_url).await;
    let vault = vault();
    // A unique org so parallel runs don't collide on the shared secret_store.
    let org = format!("org_{}", uuid::Uuid::now_v7().simple());

    // Nothing configured to start.
    assert!(
        store
            .org_computer_kinds(&org)
            .await
            .expect("kinds")
            .is_empty()
    );
    assert!(
        store
            .org_computer_secret(&vault, &org, "ascii")
            .await
            .expect("open")
            .is_none()
    );

    // Set the box key.
    store
        .set_org_computer_secret(&vault, &org, "ascii", "box_live_secret_key", 1)
        .await
        .expect("set");

    // It lists as configured, and opens back to the plaintext.
    let kinds = store.org_computer_kinds(&org).await.expect("kinds");
    assert_eq!(kinds, vec!["ascii".to_string()]);
    assert_eq!(
        store
            .org_computer_secret(&vault, &org, "ascii")
            .await
            .expect("open"),
        Some("box_live_secret_key".to_string())
    );

    // The wrong org cannot open it (the seal is bound to the row id).
    let other = format!("org_{}", uuid::Uuid::now_v7().simple());
    assert!(
        store
            .org_computer_secret(&vault, &other, "ascii")
            .await
            .expect("open other")
            .is_none()
    );

    // Clear it.
    store
        .clear_org_computer_secret(&org, "ascii")
        .await
        .expect("clear");
    assert!(
        store
            .org_computer_kinds(&org)
            .await
            .expect("kinds")
            .is_empty()
    );
}

/// A ciphertext sealed under one KEK must not count as configured under another — that is how a
/// rotated OG_CREDENTIAL_KEK made ascii look "configured" while every open failed.
#[tokio::test]
async fn a_key_sealed_under_another_kek_is_not_openable() {
    let database_url = database_or_skip!();
    let store = store(&database_url).await;
    let vault = vault();
    let org = format!("org_{}", uuid::Uuid::now_v7().simple());
    store
        .set_org_computer_secret(&vault, &org, "ascii", "box_live_secret_key", 1)
        .await
        .expect("set");

    let other = Vault::from_base64_key("ZmVkY2JhOTg3NjU0MzIxMGZlZGNiYTk4NzY1NDMyMTA=")
        .expect("other vault");
    assert!(
        store
            .org_computer_secret(&other, &org, "ascii")
            .await
            .is_err(),
        "opening with the wrong KEK must fail, not return None"
    );
    assert!(
        store
            .org_computer_kinds_openable(&other, &org)
            .await
            .expect("openable")
            .is_empty(),
        "an unreadable secret is not configured"
    );
    assert_eq!(
        store.org_computer_kinds(&org).await.expect("kinds"),
        vec!["ascii".to_string()],
        "the row is still there — only opening it fails"
    );
}
