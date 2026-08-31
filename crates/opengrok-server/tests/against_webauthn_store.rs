//! The WebAuthn device registry (passkey step-up, slice 7): register a credential, list it, record
//! an assertion, revoke it, and the "any registered device?" gate. Needs Postgres; skips loudly
//! without it.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use opengrok_store::PgStore;

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
    opengrok_store::migrations::run(&pool).await.expect("migrations");
    PgStore::new(pool)
}

#[tokio::test]
async fn a_device_registers_lists_touches_and_revokes() {
    let database_url = database_or_skip!();
    let store = store(&database_url).await;
    let account = format!("acct_{}", uuid::Uuid::now_v7().simple());
    let cred = format!("cred_{}", uuid::Uuid::now_v7().simple());

    // Nothing registered ⇒ the gate is closed: no device, no remote control.
    assert!(!store.has_registered_device(&account).await.expect("gate"));

    store
        .register_webauthn_credential(&account, &cred, "PUBKEY-JSON-v1", "Uriah's MacBook", 100)
        .await
        .expect("register");
    assert!(store.has_registered_device(&account).await.expect("gate"));

    let devices = store.webauthn_credentials(&account).await.expect("list");
    assert_eq!(devices.len(), 1);
    let (cid, pubkey, sign_count, label, created, last_used, revoked) = &devices[0];
    assert_eq!(cid, &cred);
    assert_eq!(pubkey, "PUBKEY-JSON-v1");
    assert_eq!(*sign_count, 0);
    assert_eq!(label, "Uriah's MacBook");
    assert_eq!(*created, 100);
    assert!(last_used.is_none());
    assert!(!revoked);

    // A successful assertion bumps the sign count and stamps last-used.
    store
        .touch_webauthn_credential(&account, &cred, 7, 200)
        .await
        .expect("touch");
    let devices = store.webauthn_credentials(&account).await.expect("list");
    assert_eq!(devices[0].2, 7);
    assert_eq!(devices[0].5, Some(200));

    // Revoke ⇒ the gate closes again and the row reads revoked.
    store.revoke_webauthn_credential(&account, &cred).await.expect("revoke");
    assert!(!store.has_registered_device(&account).await.expect("gate"));
    let devices = store.webauthn_credentials(&account).await.expect("list");
    assert!(devices[0].6, "the row should read revoked");

    // Re-registering the same authenticator un-revokes it (registering IS re-authorising).
    store
        .register_webauthn_credential(&account, &cred, "PUBKEY-JSON-v2", "Uriah's MacBook", 300)
        .await
        .expect("re-register");
    assert!(store.has_registered_device(&account).await.expect("gate"));
    let devices = store.webauthn_credentials(&account).await.expect("list");
    assert_eq!(devices.len(), 1, "re-register upserts, not duplicates");
    assert_eq!(devices[0].1, "PUBKEY-JSON-v2");
    assert!(!devices[0].6);
}
