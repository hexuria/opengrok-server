//! An uncapped coworker's dead key must not cost the conversation. A capped one must still hold.
//!
//! `key_for` attaches a coworker's own gateway key to EVERY turn, but `GuardedDoor` returns before
//! it ever reads that key when the coworker is uncapped — so for an uncapped coworker the key buys
//! metering and nothing else. On 8 Sep 2026 that arrangement took every real chat down twice: once
//! when a wipe of the gateway's database destroyed the keys our rows still named (401), and again
//! when re-minted keys landed on an org principal whose route reached no seats (503). Both times
//! the credential's only job was to count, and both times it stopped the talking.
//!
//! So an uncapped coworker now retries once on the deployment's key. The other half is the half
//! that must not regress: a CAPPED coworker still fails closed, because running it on the
//! deployment's key would step around the very cap its own key exists to enforce.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_harness::{
    ChatMessage, DeltaStream, GatewayKey, ModelDelta, ModelDoor, ModelError, ModelRequest,
};
use opengrok_store::{PgStore, PointsLimit, PointsScope};

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// A door that refuses any request carrying a coworker's own key, and answers anything else.
///
/// This is the gateway's behaviour on 8 Sep exactly: the key authenticated against nothing (401)
/// or landed on a route with no credential (503), while the deployment's key served the same
/// model in the same second. `saw` records every key kind it was handed, so the test can assert
/// what was RETRIED rather than only what came back.
#[derive(Default)]
struct RefusesOwnKeys {
    status: u16,
    body: String,
    saw: Mutex<Vec<Option<String>>>,
    calls: AtomicUsize,
}

impl std::fmt::Debug for RefusesOwnKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RefusesOwnKeys")
    }
}

#[async_trait::async_trait]
impl ModelDoor for RefusesOwnKeys {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let own = match &request.gateway_key {
            Some(GatewayKey::Own(key)) => Some(key.clone()),
            _ => None,
        };
        if let Ok(mut saw) = self.saw.lock() {
            saw.push(own.clone());
        }
        if own.is_some() {
            return Err(ModelError::Refused {
                status: self.status,
                body: self.body.clone(),
            });
        }
        Ok(Box::pin(futures::stream::iter(vec![Ok(ModelDelta::Text(
            "served on the deployment's key".to_string(),
        ))])))
    }
}

impl RefusesOwnKeys {
    fn refusing(status: u16, body: &str) -> Arc<Self> {
        Arc::new(Self {
            status,
            body: body.to_string(),
            saw: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        })
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
    fn keys_seen(&self) -> Vec<Option<String>> {
        self.saw.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

async fn seed_account(store: &PgStore, email: &str) -> AccountId {
    let id = AccountId::new();
    let hash = opengrok_server::auth::password::hash_password("password1").expect("hash");
    let at_ms = now_ms();
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.to_string(),
            password_hash: hash.clone(),
            first_name: "Test".to_string(),
            last_name: "User".to_string(),
            org_id: String::new(),
            plan: Plan::Ultra,
            verified: true,
            enabled: true,
            at_ms,
        })
        .expect("register");
    let view = AccountView {
        id: id.clone(),
        email: email.to_string(),
        plan: Plan::Ultra,
        trial: false,
        updated_at_ms: at_ms,
        password_hash: Some(hash),
        first_name: "Test".to_string(),
        last_name: "User".to_string(),
        org_id: None,
        verified: true,
        enabled: true,
        avatar_url: None,
    };
    store
        .append_account(&id, 0, &events, &view)
        .await
        .expect("append account");
    id
}

/// A turn as `conversation.rs` builds one: the coworker's own key attached, whatever its limits.
fn a_turn(coworker: &CoworkerId, payer: &AccountId) -> ModelRequest {
    ModelRequest {
        gateway_key: Some(GatewayKey::Own("oag_live_deadbeef000000".to_string())),
        spend_scope: Some(coworker.as_str().to_string()),
        spend_actor: Some(payer.as_str().to_string()),
        model: "xai/grok-4.6@sub".to_string(),
        system: None,
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: "ok".to_string(),
        }],
        tools: Vec::new(),
    }
}

async fn connect() -> Option<PgStore> {
    let database_url = std::env::var("OG_DATABASE_URL").ok()?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    Some(PgStore::new(pool))
}

/// Both halves, in one test, because they are one decision seen from two sides.
#[tokio::test]
async fn a_dead_key_costs_an_uncapped_turn_nothing_and_a_capped_turn_everything() {
    let Some(store) = connect().await else {
        eprintln!("skipping: OG_DATABASE_URL is not set");
        return;
    };
    let stamp = now_ms();
    let payer = seed_account(&store, &format!("deadkey-{stamp}@og.local")).await;

    // ---- 1. UNCAPPED: the turn survives on the deployment's key ----
    //
    // 401 is the shape from the wipe: the gateway had never heard of the key our row named.
    let uncapped = CoworkerId::new();
    let door = RefusesOwnKeys::refusing(401, "authentication failed");
    let guard = opengrok_server::spend::GuardedDoor::new(door.clone(), store.clone(), None);

    let served = guard.stream(a_turn(&uncapped, &payer)).await;
    assert!(
        served.is_ok(),
        "an uncapped coworker must not lose its turn to a credential that only meters it: {:?}",
        served.err()
    );
    assert_eq!(
        door.calls(),
        2,
        "it should have tried the coworker's key, then fallen back"
    );
    assert_eq!(
        door.keys_seen(),
        vec![Some("oag_live_deadbeef000000".to_string()), None],
        "the retry must drop the key rather than send the same dead one twice"
    );

    // ---- 2. the 503 shape too ----
    //
    // A key that authenticates but lands on a route with no credential. Same remedy, and it is
    // matched on its sentence rather than its status, so this pins the sentence.
    let also_uncapped = CoworkerId::new();
    let door503 = RefusesOwnKeys::refusing(
        503,
        r#"{"error":{"message":"no credential available for provider openai on this route"}}"#,
    );
    let guard503 = opengrok_server::spend::GuardedDoor::new(door503.clone(), store.clone(), None);
    assert!(
        guard503
            .stream(a_turn(&also_uncapped, &payer))
            .await
            .is_ok(),
        "a 503 naming a credential is the same problem wearing a different status"
    );
    assert_eq!(door503.calls(), 2);

    // ---- 3. a refusal that is NOT about a credential must not be retried ----
    //
    // The narrow match earns its keep here: retrying a bad request on a second key spends a
    // request to get the same answer, and would mask a real fault as a flaky one.
    let bad_request = CoworkerId::new();
    let door400 = RefusesOwnKeys::refusing(400, "the model id is malformed");
    let guard400 = opengrok_server::spend::GuardedDoor::new(door400.clone(), store.clone(), None);
    assert!(
        guard400.stream(a_turn(&bad_request, &payer)).await.is_err(),
        "a malformed request is not a credential problem"
    );
    assert_eq!(door400.calls(), 1, "and must not be tried a second time");

    // ---- 4. CAPPED: still fails closed, and this is the half that must never regress ----
    //
    // Falling back here would run a capped coworker on the deployment's key, which is precisely
    // the cap being stepped around. It is held instead, with its own sentence.
    let capped = CoworkerId::new();
    store
        .put_points_limit(
            PointsScope::Coworker,
            capped.as_str(),
            PointsLimit {
                month_points: Some(5_000),
                day_points: None,
            },
            payer.as_str(),
            now_ms(),
        )
        .await
        .expect("store the cap");

    let door_capped = RefusesOwnKeys::refusing(401, "authentication failed");
    let guard_capped =
        opengrok_server::spend::GuardedDoor::new(door_capped.clone(), store.clone(), None);
    let held = guard_capped.stream(a_turn(&capped, &payer)).await;

    assert!(
        held.is_err(),
        "a capped coworker must be held, not quietly run uncounted"
    );
    assert_eq!(
        door_capped.calls(),
        0,
        "and held BEFORE the door — a capped turn never reaches the model at all when its own \
         key cannot be counted on"
    );
}
