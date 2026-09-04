//! A key, a pool and a cached answer per (coworker, MEMBER).
//!
//! A shared coworker is talked to by people who do not own it. Every per-coworker thing in the
//! spend path — the gateway key the request goes out on, the pool it counts against, and the
//! cached resolution of both — has to be per PAIR, or one member's turn is billed to another
//! member's month. The roster is not widened yet (that is the transcript re-key), so these are
//! driven at the store and the guard rather than over HTTP; the property is pinned before the
//! thing that makes it reachable arrives.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_store::{CoworkerKeyView, PgStore, PointsLimit, PointsScope};

macro_rules! database_or_skip {
    () => {
        match std::env::var("OG_DATABASE_URL") {
            Ok(url) if !url.is_empty() => url,
            _ => {
                eprintln!("SKIPPED: set OG_DATABASE_URL to run this test");
                return;
            }
        }
    };
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

async fn store_for(database_url: &str) -> PgStore {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    PgStore::new(pool)
}

async fn seed_account(store: &PgStore, email: &str) -> AccountId {
    let id = AccountId::new();
    let at_ms = now_ms();
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.to_string(),
            password_hash: "x".to_string(),
            first_name: "Test".to_string(),
            last_name: "User".to_string(),
            org_id: String::new(),
            plan: Plan::Ultra,
            verified: true,
            enabled: true,
            at_ms,
        })
        .expect("register");
    store
        .append_account(
            &id,
            0,
            &events,
            &AccountView {
                id: id.clone(),
                email: email.to_string(),
                plan: Plan::Ultra,
                trial: false,
                updated_at_ms: at_ms,
                password_hash: Some("x".to_string()),
                first_name: "Test".to_string(),
                last_name: "User".to_string(),
                org_id: None,
                verified: true,
                enabled: true,
                avatar_url: None,
            },
        )
        .await
        .expect("seed account");
    id
}

fn key_row(coworker: &CoworkerId, account: &AccountId, suffix: &str) -> CoworkerKeyView {
    CoworkerKeyView {
        coworker_id: coworker.as_str().to_string(),
        account_id: account.as_str().to_string(),
        key_id: format!("key_{suffix}"),
        key_prefix: format!("oag_live_{suffix}"),
        quota_usd: None,
        created_at_ms: now_ms(),
        revoked_at_ms: None,
        secret_scoped: true,
    }
}

/// One coworker, two members, two keys — and a member's pool sums THEIR keys, wherever those
/// keys are. Before this, a coworker had one key and every turn on it drew down the hirer's
/// month no matter who was talking.
#[tokio::test]
async fn a_coworker_has_a_key_per_member_and_each_pool_sums_its_own() {
    let database_url = database_or_skip!();
    let store = store_for(&database_url).await;
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let ada = seed_account(&store, &format!("ada-{tag}@og.local")).await;
    let bo = seed_account(&store, &format!("bo-{tag}@og.local")).await;
    let shared = CoworkerId::new();
    let adas_own = CoworkerId::new();

    store
        .insert_coworker_key(&key_row(&shared, &ada, &format!("shared_ada_{tag}")))
        .await
        .expect("ada's key on the shared coworker");
    store
        .insert_coworker_key(&key_row(&shared, &bo, &format!("shared_bo_{tag}")))
        .await
        .expect("bo's key on the same coworker");
    store
        .insert_coworker_key(&key_row(&adas_own, &ada, &format!("own_ada_{tag}")))
        .await
        .expect("ada's key on her own coworker");

    // The pair is the identity. Asking for one coworker's key without saying whose is no longer
    // a question this store can answer, which is the whole point.
    let hers = store
        .coworker_key(&shared, &ada)
        .await
        .expect("read")
        .expect("ada has one");
    let his = store
        .coworker_key(&shared, &bo)
        .await
        .expect("read")
        .expect("bo has one");
    assert_ne!(
        hers.key_id, his.key_id,
        "two members on one coworker do not share a credential"
    );

    // A pool sums the person's own keys across every coworker they talk to — including one they
    // do not own. Bo's month is Bo's, on somebody else's coworker.
    let mut ada_keys: Vec<String> = store
        .coworker_keys_for_account(&ada)
        .await
        .expect("ada's keys")
        .into_iter()
        .map(|row| row.key_id)
        .collect();
    ada_keys.sort();
    assert_eq!(
        ada_keys,
        vec![
            format!("key_own_ada_{tag}"),
            format!("key_shared_ada_{tag}")
        ],
        "both of Ada's, and neither of Bo's"
    );
    let bo_keys: Vec<String> = store
        .coworker_keys_for_account(&bo)
        .await
        .expect("bo's keys")
        .into_iter()
        .map(|row| row.key_id)
        .collect();
    assert_eq!(bo_keys, vec![format!("key_shared_bo_{tag}")]);

    // Retirement revokes EVERY member's key. Revoking only the hirer's would leave every other
    // member holding a live credential on a retired coworker.
    let revoked = store
        .mark_coworker_keys_revoked(&shared, now_ms())
        .await
        .expect("revoke");
    assert_eq!(revoked.len(), 2, "both, not just the owner's: {revoked:?}");
    assert!(
        store
            .mark_coworker_keys_revoked(&shared, now_ms())
            .await
            .expect("revoke again")
            .is_empty(),
        "already marked; nothing to revoke twice"
    );
    // The rows stay, marked: each member's month still counts toward their own pool.
    assert!(
        store
            .coworker_key(&shared, &bo)
            .await
            .expect("read")
            .expect("the row stays")
            .revoked_at_ms
            .is_some()
    );
}

/// The guard's resolved-limits cache is keyed on the PAIR. With the cache OFF this passes for
/// the wrong reason, so it runs at the production freshness: a coworker-keyed cache would serve
/// Bo Ada's pool for a whole freshness window, and he would be refused or allowed on somebody
/// else's month.
#[tokio::test]
async fn the_limits_cache_does_not_serve_one_member_another_members_pool() {
    let database_url = database_or_skip!();
    let store = store_for(&database_url).await;
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let ada = seed_account(&store, &format!("ada-cache-{tag}@og.local")).await;
    let bo = seed_account(&store, &format!("bo-cache-{tag}@og.local")).await;
    let shared = CoworkerId::new();

    for (who, pool) in [(&ada, 1_000_000_i64), (&bo, 7_i64)] {
        store
            .put_points_limit(
                PointsScope::Member,
                who.as_str(),
                PointsLimit {
                    month_points: Some(pool),
                    day_points: None,
                },
                "test",
                now_ms(),
            )
            .await
            .expect("pool");
    }
    store
        .put_points_limit(
            PointsScope::Coworker,
            shared.as_str(),
            PointsLimit {
                month_points: Some(500),
                day_points: None,
            },
            "test",
            now_ms(),
        )
        .await
        .expect("cap");

    // Default freshness, NOT `with_fresh_ms(0)`: at zero every read goes to the database and the
    // cache key is never exercised, which is exactly the bug this is here to catch.
    let door = opengrok_server::spend::GuardedDoor::new(
        std::sync::Arc::new(opengrok_harness::MockDoor::echoing()),
        store.clone(),
        None,
    );

    // Ada first, so hers is the entry a coworker-keyed cache would hand to Bo.
    let hers = door.limits_for(&shared, &ada).await.expect("ada's limits");
    let his = door.limits_for(&shared, &bo).await.expect("bo's limits");
    assert_eq!(hers.pool, Some(1_000_000), "her own pool");
    assert_eq!(
        his.pool,
        Some(7),
        "his own pool, not the entry Ada's read just cached"
    );
    assert_eq!(hers.payer, ada, "and each says whose it is");
    assert_eq!(his.payer, bo);
    // The coworker's own cap is the coworker's, and is the same for both.
    assert_eq!(hers.cap, Some(500));
    assert_eq!(his.cap, Some(500));

    // Cached, and still each their own on the second read.
    assert_eq!(
        door.limits_for(&shared, &bo).await.expect("again").pool,
        Some(7)
    );
    assert_eq!(
        door.limits_for(&shared, &ada).await.expect("again").pool,
        Some(1_000_000)
    );
}
