//! A routine that an external app wakes, rather than a clock.
//!
//! The cron half of `/schedules` has had a smoke since slice 10; the webhook half had nothing at
//! all, because for two PRs nothing on this server could create one. The wake lived in the
//! aggregate, the door lived in `hooks.rs`, and the only thing that ever minted the pair — the
//! desktop's `createAgentAutomation` — was deleted with seam A. This file is the proof that the
//! three pieces meet: `POST /schedules` mints, `GET /schedules` shows what was minted, and
//! `POST /hooks/{id}` with that key starts a real run.
//!
//! WHAT IS ACTUALLY AT RISK HERE IS THE KEY. A minted bearer that the door would not accept is a
//! routine nobody can fire; a key the door accepts after it was rotated is a key that leaked and
//! could not be taken back. Both are checked against the running server rather than against the
//! hashing helper, because the mint, the hash and the comparison are three functions that have to
//! agree and only the HTTP path exercises all three.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::AccountId;
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::password::hash_password;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::host_state::HostState;
use opengrok_store::PgStore;
use serde_json::{Value, json};

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

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

async fn seed_account(store: &PgStore, email: &str) -> AccountId {
    let id = AccountId::new();
    let hash = hash_password("password1").expect("hash");
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

struct Harness {
    base: String,
    client: reqwest::Client,
    store: PgStore,
    agui: AgUiState,
    account: AccountId,
    email: String,
}

/// A server, with the two addresses `hook_url` chooses between spelled out: what this host
/// advertises for itself (`OG_PUBLIC_GATEWAY_URL` in production), and the base the rest of auth
/// already hands out. Either may be empty, and the third case — both empty — is a deployment that
/// has told us no address at all.
async fn harness(
    database_url: &str,
    email: &str,
    public_gateway_url: Option<&str>,
    public_url: &str,
) -> Harness {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let account = seed_account(&store, email).await;
    let minter = Arc::new(TokenMinter::new(b"a-routine-woken-by-a-webhook-secret"));
    let mut auth = AuthState::new(store.clone(), minter, email.to_string());
    auth.public_url = public_url.to_string();
    let agui = AgUiState {
        auth,
        door: Arc::new(MockDoor::echoing()),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer: None,
        vault: None,
        connectors: Connectors {
            providers: Arc::new(BTreeMap::new()),
            redirect_uri: "http://127.0.0.1/callback".to_string(),
        },
        plugins: Arc::new(BTreeMap::new()),
        host_settings: None,
    };
    let host = HostState::new(agui.clone(), public_gateway_url.map(str::to_string));
    let app = opengrok_server::router(agui.clone(), host);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    Harness {
        base: format!("http://127.0.0.1:{}", addr.port()),
        client: reqwest::Client::new(),
        store,
        agui,
        account,
        email: email.to_string(),
    }
}

/// The ordinary harness: a host that advertises an address, which is what a deployment reachable
/// by the app POSTing to it looks like.
async fn advertising(database_url: &str, email: &str) -> Harness {
    harness(
        database_url,
        email,
        Some("http://opengrok.lan:1447"),
        "http://127.0.0.1:9/auth",
    )
    .await
}

impl Harness {
    fn token(&self) -> String {
        self.agui
            .auth
            .minter
            .mint_access(
                self.account.as_str(),
                "sess-routines",
                &self.email,
                "ultra",
                chrono::Utc::now().timestamp(),
                3600,
            )
            .expect("mint access")
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        let res = self
            .client
            .post(format!("{}{path}", self.base))
            .header("authorization", format!("Bearer {}", self.token()))
            .json(&body)
            .send()
            .await
            .expect("post");
        let status = res.status().as_u16();
        let text = res.text().await.expect("body");
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        let res = self
            .client
            .get(format!("{}{path}", self.base))
            .header("authorization", format!("Bearer {}", self.token()))
            .send()
            .await
            .expect("get");
        let status = res.status().as_u16();
        let text = res.text().await.expect("body");
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn hire(&self) -> String {
        let (status, hired) = self
            .post("/coworkers", json!({ "name": "Nightshift" }))
            .await;
        assert_eq!(status, 201, "{hired}");
        hired["id"].as_str().expect("coworker id").to_string()
    }

    /// A webhook routine, and what the reply says about how to fire it.
    async fn webhook_routine(&self, coworker: &str) -> (String, Value) {
        let (status, body) = self
            .post(
                "/schedules",
                json!({
                    "coworkerId": coworker,
                    "kind": "webhook",
                    "name": "Inbox",
                    "prompt": "a webhook arrived; report in",
                }),
            )
            .await;
        assert_eq!(status, 201, "{body}");
        (body["id"].as_str().expect("id").to_string(), body)
    }

    /// The outside world's call: NO account token, only the hook's own key.
    async fn fire_hook(&self, hook_id: &str, key: &str) -> u16 {
        self.client
            .post(format!("{}/hooks/{hook_id}", self.base))
            .header("authorization", format!("Bearer {key}"))
            .header("content-type", "application/json")
            .json(&json!({ "item": "milk" }))
            .send()
            .await
            .expect("post a hook")
            .status()
            .as_u16()
    }
}

/// The last path segment of the minted URL — what `POST /hooks/{id}` is addressed to.
fn hook_id_of(url: &str) -> &str {
    url.rsplit('/').next().expect("a path segment")
}

/// Wait for a run journaled under this routine's own thread. A firing is spawned, so the 202 is
/// the server accepting the wake and not the run existing yet.
async fn run_appears(store: &PgStore, thread: &str) -> bool {
    for _ in 0..60 {
        if let Ok(runs) = store.runs_for_thread(thread, 10).await
            && !runs.is_empty()
        {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    false
}

async fn runs_in_thread(store: &PgStore, thread: &str) -> usize {
    store
        .runs_for_thread(thread, 50)
        .await
        .expect("read a thread")
        .len()
}

/// THE ONE THE SLICE IS FOR. A routine is created with a webhook wake, the server mints both
/// halves, and the reply carries everything a person needs to wire it into something else.
#[tokio::test]
async fn a_webhook_routine_is_minted_with_a_url_and_a_key() {
    let database_url = database_or_skip!();
    let email = format!("routine-mint-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = advertising(&database_url, &email).await;
    let coworker = h.hire().await;

    let (id, created) = h.webhook_routine(&coworker).await;
    assert_eq!(created["kind"], json!("webhook"), "{created}");
    assert_eq!(
        created["cron"],
        Value::Null,
        "a hook has no clock, and `null` says so where an empty string would read as an \
         expression somebody forgot to fill in: {created}"
    );
    assert_eq!(
        created["nextDueMs"],
        Value::Null,
        "nothing is due: the sweep never claims a webhook routine: {created}"
    );

    let url = created["webhook"]["url"].as_str().expect("a url");
    let key = created["webhook"]["key"].as_str().expect("a key");
    let hook = hook_id_of(url);
    assert!(
        hook.starts_with("hook_"),
        "the hook id is minted by us, not chosen by the caller: {created}"
    );
    assert_eq!(
        url,
        format!("http://opengrok.lan:1447/hooks/{hook}"),
        "the URL is the address this host advertises, not the socket it happens to be bound to: \
         {created}"
    );
    assert!(key.starts_with("og_"), "{created}");
    assert_eq!(
        created["webhook"]["header"],
        json!(format!("Authorization: Bearer {key}")),
        "the header is spelled out so it can be pasted rather than assembled: {created}"
    );

    // And a listing says exactly the same thing, key included — the aggregate keeps the plaintext
    // so a person who closed this reply is not forced to rotate to get their key back.
    let (status, rows) = h.get("/schedules").await;
    assert_eq!(status, 200, "{rows}");
    let mine = rows
        .as_array()
        .expect("an array")
        .iter()
        .find(|row| row["id"] == json!(id))
        .expect("the routine that was just created");
    assert_eq!(mine["kind"], json!("webhook"), "{mine}");
    assert_eq!(mine["cron"], Value::Null, "{mine}");
    assert_eq!(mine["webhook"]["url"], json!(url), "{mine}");
    assert_eq!(
        mine["webhook"]["key"],
        json!(key),
        "the same key, not a second one: minting again on every read would break whatever the \
         first one was pasted into: {mine}"
    );
}

/// An external app POSTs, and a coworker takes a turn. Nobody signed in for this.
#[tokio::test]
async fn an_inbound_post_with_the_key_fires_a_run() {
    let database_url = database_or_skip!();
    let email = format!("routine-fire-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = advertising(&database_url, &email).await;
    let coworker = h.hire().await;
    let (id, created) = h.webhook_routine(&coworker).await;
    let key = created["webhook"]["key"].as_str().expect("key").to_string();
    let hook = hook_id_of(created["webhook"]["url"].as_str().expect("url")).to_string();

    // The wrong key first, so a run appearing later cannot be credited to it.
    assert_eq!(
        h.fire_hook(&hook, "og_not_the_minted_key").await,
        401,
        "a wrong bearer must not fire somebody's coworker"
    );
    assert_eq!(
        runs_in_thread(&h.store, &id).await,
        0,
        "and it must not have started anything"
    );

    assert_eq!(h.fire_hook(&hook, &key).await, 202);
    assert!(
        run_appears(&h.store, &id).await,
        "a run must be journaled under the routine's own id as its thread — that is how the \
         pane shows a routine's history"
    );
}

/// A key that leaked has to be takeable back, and the URL it was pasted into has to keep working
/// with the new one. Rotating replaces the bearer and nothing else.
#[tokio::test]
async fn a_rotated_key_works_and_the_old_one_stops() {
    let database_url = database_or_skip!();
    let email = format!("routine-rotate-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = advertising(&database_url, &email).await;
    let coworker = h.hire().await;
    let (id, created) = h.webhook_routine(&coworker).await;
    let old = created["webhook"]["key"].as_str().expect("key").to_string();
    let url = created["webhook"]["url"].as_str().expect("url").to_string();
    let hook = hook_id_of(&url).to_string();

    let (status, rotated) = h
        .post(&format!("/schedules/{id}/rotate-key"), json!({}))
        .await;
    assert_eq!(status, 200, "{rotated}");
    let new = rotated["webhook"]["key"].as_str().expect("a new key");
    assert!(new.starts_with("og_"), "{rotated}");
    assert_ne!(new, old, "a rotation that returns the same key is not one");
    assert_eq!(
        rotated["webhook"]["url"],
        json!(url),
        "the hook id and its URL survive a rotation — only the key moves, or every rotation \
         would be a reconfiguration: {rotated}"
    );

    assert_eq!(
        h.fire_hook(&hook, &old).await,
        401,
        "the old key stops at once: a grace period is a window in which the reason for \
         rotating still holds"
    );
    assert_eq!(h.fire_hook(&hook, new).await, 202);
    assert!(run_appears(&h.store, &id).await, "the new key really fires");

    // And the listing shows the key that works now, not the one that was minted at create.
    let (_, rows) = h.get("/schedules").await;
    let mine = rows
        .as_array()
        .expect("an array")
        .iter()
        .find(|row| row["id"] == json!(id))
        .expect("the routine")
        .clone();
    assert_eq!(mine["webhook"]["key"], json!(new), "{mine}");
}

/// A cron routine has no key, so there is nothing to rotate — and the aggregate is what says so,
/// not a second check at the door.
#[tokio::test]
async fn rotating_a_cron_routine_is_a_conflict() {
    let database_url = database_or_skip!();
    let email = format!("routine-cronkey-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = advertising(&database_url, &email).await;
    let coworker = h.hire().await;
    let (status, created) = h
        .post(
            "/schedules",
            json!({ "coworkerId": coworker, "cron": "0 9 * * 1", "prompt": "Monday report" }),
        )
        .await;
    assert_eq!(status, 201, "{created}");
    let id = created["id"].as_str().expect("id").to_string();

    let (status, refused) = h
        .post(&format!("/schedules/{id}/rotate-key"), json!({}))
        .await;
    assert_eq!(status, 409, "{refused}");
    assert_eq!(
        refused["error"],
        json!("that schedule is not a webhook"),
        "{refused}"
    );
}

/// THE OLD BODY STILL MEANS WHAT IT MEANT. Every caller written before this slice sends
/// `{coworkerId, cron, prompt}` with no `kind` — and a default that changed would turn their
/// routines into hooks nobody POSTs to.
#[tokio::test]
async fn a_body_without_a_kind_is_still_a_cron_routine() {
    let database_url = database_or_skip!();
    let email = format!("routine-cron-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = advertising(&database_url, &email).await;
    let coworker = h.hire().await;

    let (status, created) = h
        .post(
            "/schedules",
            json!({ "coworkerId": coworker, "cron": "0 9 * * 1", "prompt": "Monday report" }),
        )
        .await;
    assert_eq!(status, 201, "{created}");
    assert_eq!(created["kind"], json!("cron"), "{created}");
    assert_eq!(created["cron"], json!("0 0 9 * * 1"), "{created}");
    assert!(
        created["nextDueMs"].is_i64(),
        "a clock routine still says when it is next due: {created}"
    );
    assert_eq!(
        created["webhook"],
        Value::Null,
        "and it carries no webhook block at all: {created}"
    );

    // A cron routine with no expression is refused rather than stored as a row that never fires.
    let (status, refused) = h
        .post(
            "/schedules",
            json!({ "coworkerId": coworker, "prompt": "when, exactly?" }),
        )
        .await;
    assert_eq!(status, 422, "{refused}");

    // And a `kind` we do not know is refused too, rather than quietly read as the default.
    let (status, refused) = h
        .post(
            "/schedules",
            json!({ "coworkerId": coworker, "kind": "webhok", "prompt": "a typo" }),
        )
        .await;
    assert_eq!(
        status, 422,
        "a misspelled kind must not install a clock routine with no clock: {refused}"
    );
}

/// WHICH ADDRESS THE HOOK URL CARRIES. A URL is handed to a phone or a SaaS app, so the host's
/// own advertised address wins over anything auth hands out for itself — and a deployment that
/// has told us no address at all says so with a path rather than inventing a host.
#[tokio::test]
async fn the_hook_url_prefers_the_address_this_host_advertises() {
    let database_url = database_or_skip!();

    let advertised = format!("routine-url-a-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(
        &database_url,
        &advertised,
        Some("http://opengrok.lan:1447/"),
        "http://auth.example:8080",
    )
    .await;
    let coworker = h.hire().await;
    let (_, created) = h.webhook_routine(&coworker).await;
    let url = created["webhook"]["url"].as_str().expect("url");
    assert_eq!(
        url,
        format!("http://opengrok.lan:1447/hooks/{}", hook_id_of(url)),
        "the advertised address wins, and its trailing slash is not doubled: {created}"
    );

    let fallback = format!("routine-url-b-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &fallback, None, "http://auth.example:8080").await;
    let coworker = h.hire().await;
    let (_, created) = h.webhook_routine(&coworker).await;
    let url = created["webhook"]["url"].as_str().expect("url");
    assert_eq!(
        url,
        format!("http://auth.example:8080/hooks/{}", hook_id_of(url)),
        "with nothing advertised, the base the rest of auth already uses is the honest answer: \
         {created}"
    );

    let silent = format!("routine-url-c-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &silent, None, "").await;
    let coworker = h.hire().await;
    let (_, created) = h.webhook_routine(&coworker).await;
    let url = created["webhook"]["url"].as_str().expect("url");
    assert_eq!(
        url,
        format!("/hooks/{}", hook_id_of(url)),
        "told no address, we give the path and let the reader supply the host — inventing one \
         would hand somebody a URL that resolves to the wrong machine: {created}"
    );
}
