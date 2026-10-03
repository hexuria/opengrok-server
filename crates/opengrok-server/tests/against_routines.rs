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
use opengrok_core::id::{AccountId, CoworkerId, RunId};
use opengrok_core::run::{Run, RunCommand, RunView};
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
            Ok(url) => opengrok_store::gate_database_or_panic(url),
            Err(_) => {
                eprintln!("skipping: OG_DATABASE_URL is not set");
                return;
            }
        }
    };
}

/// THE SWEEP'S CLOCK IS SHARED BY EVERY TEST IN THIS FILE. A test that claims with a moment in the
/// future, or runs a real tick, claims every due routine in the database — other tests' too — and
/// a routine claimed by somebody else's test never fires in its own. Those tests hold this lock
/// exclusively; every other test holds it shared, so they still run side by side.
///
/// A POSTGRES ADVISORY LOCK, NOT A MUTEX: nextest runs each test in its own process, and the
/// database is the one thing they all share. It lives on a connection of its own, so the lock
/// goes when the test's guard is dropped and the connection closes.
async fn clock(database_url: &str, exclusive: bool) -> sqlx::PgConnection {
    use sqlx::Connection;
    const CLOCK_KEY: i64 = 0x5eed_c10c;
    let mut connection = sqlx::PgConnection::connect(database_url)
        .await
        .expect("connect for the clock lock");
    let lock = if exclusive {
        "select pg_advisory_lock($1)"
    } else {
        "select pg_advisory_lock_shared($1)"
    };
    sqlx::query(lock)
        .bind(CLOCK_KEY)
        .execute(&mut connection)
        .await
        .expect("take the clock lock");
    connection
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

/// A reply, with the one header this door has a rule about.
struct Response {
    status: u16,
    cache_control: String,
    body: Value,
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
    harness_in(database_url, email, (public_gateway_url, public_url), false).await
}

/// An advertising server whose routines may wake in seconds (`OG_ROUTINE_SECOND_CRON`), for a
/// test that needs a tick's worth of due routines now rather than in a minute.
async fn in_seconds(database_url: &str, email: &str) -> Harness {
    let addresses = (Some("http://opengrok.lan:1447"), "http://127.0.0.1:9/auth");
    harness_in(database_url, email, addresses, true).await
}

async fn harness_in(
    database_url: &str,
    email: &str,
    (public_gateway_url, public_url): (Option<&str>, &str),
    second_cron: bool,
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
    auth.second_cron = second_cron;
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

    /// A second signed-in account on the same server, for the checks that one account's routines
    /// are invisible to another.
    async fn stranger(&self) -> String {
        let email = format!(
            "routine-stranger-{}@og.local",
            uuid::Uuid::now_v7().simple()
        );
        let account = seed_account(&self.store, &email).await;
        self.agui
            .auth
            .minter
            .mint_access(
                account.as_str(),
                "sess-stranger",
                &email,
                "ultra",
                chrono::Utc::now().timestamp(),
                3600,
            )
            .expect("mint access")
    }

    /// `None` is a request with no `Authorization` header at all, which is its own case.
    async fn call(&self, method: &str, path: &str, token: Option<&str>, body: Value) -> Response {
        let url = format!("{}{path}", self.base);
        let mut request = match method {
            "GET" => self.client.get(url),
            "PATCH" => self.client.patch(url).json(&body),
            "DELETE" => self.client.delete(url),
            _ => self.client.post(url).json(&body),
        };
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let res = request.send().await.expect("request");
        let status = res.status().as_u16();
        let cache_control = res
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let text = res.text().await.expect("body");
        Response {
            status,
            cache_control,
            body: serde_json::from_str(&text).unwrap_or(Value::String(text)),
        }
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        let res = self.call("POST", path, Some(&self.token()), body).await;
        (res.status, res.body)
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        let res = self
            .call("GET", path, Some(&self.token()), Value::Null)
            .await;
        (res.status, res.body)
    }

    async fn patch(&self, path: &str, body: Value) -> (u16, Value) {
        let res = self.call("PATCH", path, Some(&self.token()), body).await;
        (res.status, res.body)
    }

    /// A cron routine that will not fire on its own while a test runs: 09:00 on Mondays.
    async fn cron_routine(&self, coworker: &str) -> String {
        let (status, body) = self
            .post(
                "/schedules",
                json!({
                    "coworkerId": coworker,
                    "name": "Weekly",
                    "cron": "0 9 * * 1",
                    "prompt": "write the weekly report",
                }),
            )
            .await;
        assert_eq!(status, 201, "{body}");
        body["id"].as_str().expect("id").to_string()
    }

    async fn row(&self, id: &str) -> Value {
        let (status, rows) = self.get("/schedules").await;
        assert_eq!(status, 200, "{rows}");
        rows.as_array()
            .expect("an array")
            .iter()
            .find(|row| row["id"] == json!(id))
            .cloned()
            .expect("the routine")
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

    /// The outside world's call: NO account token, only the hook's own key. `None` sends no
    /// `Authorization` header at all.
    async fn fire_hook(&self, hook_id: &str, key: Option<&str>) -> u16 {
        let mut request = self
            .client
            .post(format!("{}/hooks/{hook_id}", self.base))
            .header("content-type", "application/json")
            .json(&json!({ "item": "milk" }));
        if let Some(key) = key {
            request = request.header("authorization", format!("Bearer {key}"));
        }
        request.send().await.expect("post a hook").status().as_u16()
    }
}

/// A run in flight on this thread, written straight to the store.
///
/// The cap reads the projection, and racing three real firings to prove a cap is a test about
/// timing rather than about the cap.
async fn seed_running_run(store: &PgStore, account: &AccountId, thread: &str) -> RunId {
    let id = RunId::new();
    let mut run = Run::default();
    let at_ms = now_ms();
    let events = run
        .decide(RunCommand::Start {
            thread_id: thread.to_string(),
            coworker_id: Some(CoworkerId::from_stored("cw_in_flight")),
            model: Some("oag/cheap".to_string()),
            effort: Default::default(),
            inference_source: Default::default(),
            system: None,
            skill_id: None,
            offered_skills: Vec::new(),
            prompt: None,
            limits: Default::default(),
            at_ms,
        })
        .expect("start");
    for event in &events {
        run.apply(event);
    }
    let view = RunView {
        id: id.clone(),
        thread_id: thread.to_string(),
        status: run.status,
        event_count: 0,
        updated_at_ms: at_ms,
    };
    store
        .append_run(&id, 0, &events, &view, Some(account))
        .await
        .expect("append run");
    id
}

/// A run in flight that the routine's own log says its clock fired — what the history lists.
async fn seed_clock_run(h: &Harness, routine: &str) -> RunId {
    use opengrok_core::id::ScheduleId;
    use opengrok_core::schedule::{FireCause, ScheduleCommand};
    let run_id = seed_running_run(&h.store, &h.account, routine).await;
    let id = ScheduleId::from_stored(routine);
    let (loaded, seq) = h.store.load_schedule(&id).await.expect("load");
    let events = loaded
        .decide(ScheduleCommand::Fire {
            run_id: run_id.clone(),
            cause: FireCause::Clock,
            by: None,
            at_ms: now_ms(),
        })
        .expect("fire");
    let mut after = loaded;
    for event in &events {
        after.apply(event);
    }
    h.store
        .append_schedule(&id, &h.account, seq, &events, &after, now_ms())
        .await
        .expect("append the firing");
    run_id
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
    let _clock = clock(&database_url, false).await;
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
    let _clock = clock(&database_url, false).await;
    let email = format!("routine-fire-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = advertising(&database_url, &email).await;
    let coworker = h.hire().await;
    let (id, created) = h.webhook_routine(&coworker).await;
    let key = created["webhook"]["key"].as_str().expect("key").to_string();
    let hook = hook_id_of(created["webhook"]["url"].as_str().expect("url")).to_string();

    // Every refusal first. A count taken here could only ever prove that nothing had fired YET,
    // which a slow spawn would satisfy too; the honest check is the count AFTER a fire that is
    // known to have landed — a refusal that secretly fired would make it two.
    assert_eq!(
        h.fire_hook(&hook, Some("og_not_the_minted_key")).await,
        401,
        "a wrong bearer must not fire somebody's coworker"
    );
    assert_eq!(
        h.fire_hook(&hook, None).await,
        401,
        "and neither must no bearer at all"
    );
    assert_eq!(
        h.fire_hook("hook_01a00000-0000-7000-8000-000000000000", Some(&key))
            .await,
        401,
        "an unknown hook id answers exactly what a wrong key does, or the id space can be walked"
    );

    assert_eq!(h.fire_hook(&hook, Some(&key)).await, 202);
    assert!(
        run_appears(&h.store, &id).await,
        "a run must be journaled under the routine's own id as its thread — that is how the \
         pane shows a routine's history"
    );
    assert_eq!(
        runs_in_thread(&h.store, &id).await,
        1,
        "exactly one run: the three refusals above started nothing"
    );
}

/// A ROUTINE MAY NOT BE PRESSED INTO UNBOUNDED WORK. Whoever holds the key can POST as fast as
/// they like — a retry loop, a SaaS app redelivering — and every press would otherwise open a run
/// that is billed and holds a recovery lease.
#[tokio::test]
async fn a_hook_with_too_much_already_running_is_refused() {
    let database_url = database_or_skip!();
    let _clock = clock(&database_url, false).await;
    let email = format!("routine-cap-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = advertising(&database_url, &email).await;
    let coworker = h.hire().await;
    let (id, created) = h.webhook_routine(&coworker).await;
    let key = created["webhook"]["key"].as_str().expect("key").to_string();
    let hook = hook_id_of(created["webhook"]["url"].as_str().expect("url")).to_string();

    for _ in 0..3 {
        seed_running_run(&h.store, &h.account, &id).await;
    }
    assert_eq!(
        h.fire_hook(&hook, Some(&key)).await,
        429,
        "three in flight is the cap, and the key being right does not raise it"
    );
    assert_eq!(
        runs_in_thread(&h.store, &id).await,
        3,
        "and the refusal really refused: no fourth run"
    );

    // A wrong key is still a 401 and not a 429 — how much work a routine has in flight is not
    // something an unauthenticated caller may learn by watching which refusal they get.
    assert_eq!(h.fire_hook(&hook, Some("og_wrong")).await, 401);
}

/// A paused routine does not run because a todo app POSTed. Pause is the person's word, and an
/// inbound webhook is not their "run now".
#[tokio::test]
async fn a_paused_routine_refuses_an_inbound_post() {
    let database_url = database_or_skip!();
    let _clock = clock(&database_url, false).await;
    let email = format!("routine-paused-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = advertising(&database_url, &email).await;
    let coworker = h.hire().await;
    let (id, created) = h.webhook_routine(&coworker).await;
    let key = created["webhook"]["key"].as_str().expect("key").to_string();
    let hook = hook_id_of(created["webhook"]["url"].as_str().expect("url")).to_string();

    let (status, body) = h.post(&format!("/schedules/{id}/pause"), json!({})).await;
    assert_eq!(status, 204, "{body}");

    assert_eq!(
        h.fire_hook(&hook, Some(&key)).await,
        409,
        "the key is right and the routine is stopped: that is a conflict, not a bad token"
    );
    assert_eq!(runs_in_thread(&h.store, &id).await, 0);

    // Resumed, the same POST works — so the 409 was the pause and not a broken key.
    let (status, body) = h.post(&format!("/schedules/{id}/resume"), json!({})).await;
    assert_eq!(status, 204, "{body}");
    assert_eq!(h.fire_hook(&hook, Some(&key)).await, 202);
    assert!(run_appears(&h.store, &id).await);
}

/// SOMEBODY ELSE'S ROUTINE IS SOMEBODY ELSE'S KEY. A listing that leaked one would hand over the
/// bearer for a coworker the reader may not use at all.
#[tokio::test]
async fn another_account_sees_neither_the_routine_nor_its_key() {
    let database_url = database_or_skip!();
    let _clock = clock(&database_url, false).await;
    let email = format!("routine-owner-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = advertising(&database_url, &email).await;
    let coworker = h.hire().await;
    let (id, created) = h.webhook_routine(&coworker).await;
    let key = created["webhook"]["key"].as_str().expect("key").to_string();

    let stranger = h.stranger().await;
    let theirs = h
        .call("GET", "/schedules", Some(&stranger), Value::Null)
        .await;
    assert_eq!(theirs.status, 200, "{}", theirs.body);
    let rows = theirs.body.as_array().expect("an array").clone();
    assert!(
        !rows.iter().any(|row| row["id"] == json!(id)),
        "a stranger must not see the routine at all: {rows:?}"
    );
    assert!(
        !theirs.body.to_string().contains(&key),
        "and the key must not appear anywhere in what they are shown"
    );

    let refused = h
        .call(
            "POST",
            &format!("/schedules/{id}/rotate-key"),
            Some(&stranger),
            json!({}),
        )
        .await;
    assert_eq!(
        refused.status, 404,
        "not-yours is no-such, or the id space can be probed: {}",
        refused.body
    );

    // No token at all is 401 on both, and the key still works afterwards — nothing above moved it.
    assert_eq!(
        h.call("GET", "/schedules", None, Value::Null).await.status,
        401
    );
    assert_eq!(
        h.call(
            "POST",
            &format!("/schedules/{id}/rotate-key"),
            None,
            json!({})
        )
        .await
        .status,
        401
    );
    let hook = hook_id_of(created["webhook"]["url"].as_str().expect("url")).to_string();
    assert_eq!(h.fire_hook(&hook, Some(&key)).await, 202);
}

/// A BEARER MUST NOT SIT IN A CACHE. Every reply on this door that carries a key says so, the way
/// the OAuth door does for the tokens it mints.
#[tokio::test]
async fn every_reply_that_carries_a_key_forbids_caching() {
    let database_url = database_or_skip!();
    let _clock = clock(&database_url, false).await;
    let email = format!("routine-cache-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = advertising(&database_url, &email).await;
    let coworker = h.hire().await;

    let created = h
        .call(
            "POST",
            "/schedules",
            Some(&h.token()),
            json!({
                "coworkerId": coworker,
                "kind": "webhook",
                "name": "Inbox",
                "prompt": "a webhook arrived; report in",
            }),
        )
        .await;
    assert_eq!(created.status, 201, "{}", created.body);
    assert_eq!(created.cache_control, "no-store", "on create");
    let id = created.body["id"].as_str().expect("id").to_string();

    let listed = h
        .call("GET", "/schedules", Some(&h.token()), Value::Null)
        .await;
    assert_eq!(listed.cache_control, "no-store", "on list");

    let rotated = h
        .call(
            "POST",
            &format!("/schedules/{id}/rotate-key"),
            Some(&h.token()),
            json!({}),
        )
        .await;
    assert_eq!(rotated.status, 200, "{}", rotated.body);
    assert_eq!(rotated.cache_control, "no-store", "on rotate");
}

/// The name the person gave the routine comes back. It was accepted and dropped, so the pane had
/// no way to show what it had just been told.
#[tokio::test]
async fn the_name_is_echoed_back() {
    let database_url = database_or_skip!();
    let _clock = clock(&database_url, false).await;
    let email = format!("routine-name-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = advertising(&database_url, &email).await;
    let coworker = h.hire().await;

    let (status, created) = h
        .post(
            "/schedules",
            json!({
                "coworkerId": coworker,
                "kind": "webhook",
                "name": "  Inbox sweeper  ",
                "prompt": "a webhook arrived; report in",
            }),
        )
        .await;
    assert_eq!(status, 201, "{created}");
    assert_eq!(created["name"], json!("Inbox sweeper"), "{created}");
    let id = created["id"].as_str().expect("id").to_string();

    let (_, rows) = h.get("/schedules").await;
    let mine = rows
        .as_array()
        .expect("an array")
        .iter()
        .find(|row| row["id"] == json!(id))
        .expect("the routine")
        .clone();
    assert_eq!(mine["name"], json!("Inbox sweeper"), "{mine}");

    // Unnamed still falls back to the prompt's first words, which is this API's older shape.
    let (_, unnamed) = h
        .post(
            "/schedules",
            json!({
                "coworkerId": coworker,
                "kind": "webhook",
                "prompt": "watch the shared inbox and summarise anything urgent for me",
            }),
        )
        .await;
    assert_eq!(
        unnamed["name"],
        json!("watch the shared inbox and summarise"),
        "{unnamed}"
    );
}

/// A key that leaked has to be takeable back, and the URL it was pasted into has to keep working
/// with the new one. Rotating replaces the bearer and nothing else.
#[tokio::test]
async fn a_rotated_key_works_and_the_old_one_stops() {
    let database_url = database_or_skip!();
    let _clock = clock(&database_url, false).await;
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
        h.fire_hook(&hook, Some(&old)).await,
        401,
        "the old key stops at once: a grace period is a window in which the reason for \
         rotating still holds"
    );
    assert_eq!(h.fire_hook(&hook, Some(new)).await, 202);
    assert!(run_appears(&h.store, &id).await, "the new key really fires");
    assert_eq!(
        runs_in_thread(&h.store, &id).await,
        1,
        "exactly one run: the old key's 401 started nothing"
    );

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
    let _clock = clock(&database_url, false).await;
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
    let _clock = clock(&database_url, false).await;
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
    assert_eq!(created["cron"], json!("0 0 9 * * MON"), "{created}");
    assert!(
        created["nextDueMs"].is_i64(),
        "a clock routine still says when it is next due: {created}"
    );
    assert_eq!(
        created["webhook"],
        Value::Null,
        "and it carries no webhook block at all: {created}"
    );

    // STANDARD CRON'S WEEKDAYS. `1-5` is Monday to Friday, stored by name: the cron crate counts
    // Sunday as 1, and the digits handed to it as written ran Sunday to Thursday.
    let (status, weekdays) = h
        .post(
            "/schedules",
            json!({ "coworkerId": coworker, "cron": "0 9 * * 1-5", "prompt": "weekday report" }),
        )
        .await;
    assert_eq!(status, 201, "{weekdays}");
    assert_eq!(weekdays["cron"], json!("0 0 9 * * MON-FRI"), "{weekdays}");
    let due = weekdays["nextDueMs"]
        .as_i64()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .expect("nextDueMs");
    {
        use chrono::{Datelike, Timelike};
        assert!(
            due.weekday().num_days_from_monday() < 5 && (due.hour(), due.minute()) == (9, 0),
            "next due at 09:00 UTC on a weekday, not {due}"
        );
    }
    let id = weekdays["id"].as_str().expect("id");
    let gone = h
        .call(
            "DELETE",
            &format!("/schedules/{id}"),
            Some(&h.token()),
            Value::Null,
        )
        .await;
    assert!(
        gone.status < 300,
        "a weekday routine left behind would fire in a sweep test"
    );

    // There is no eighth day, and the refusal says what a day is.
    let (status, refused) = h
        .post(
            "/schedules",
            json!({ "coworkerId": coworker, "cron": "0 9 * * 8", "prompt": "no such day" }),
        )
        .await;
    assert_eq!(status, 422, "{refused}");
    assert!(
        refused.to_string().contains("8 is not a day of the week"),
        "refused in words: {refused}"
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
    assert!(
        !refused.to_string().contains("webhok"),
        "a refusal names the field, it does not hand the caller's own bytes back to be rendered \
         somewhere else: {refused}"
    );
}

/// WHICH ADDRESS THE HOOK URL CARRIES. A URL is handed to a phone or a SaaS app, so the host's
/// own advertised address wins over anything auth hands out for itself — and a deployment that
/// has told us no address at all says so with a path rather than inventing a host.
#[tokio::test]
async fn the_hook_url_prefers_the_address_this_host_advertises() {
    let database_url = database_or_skip!();
    let _clock = clock(&database_url, false).await;

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

    // THE BRANCH THAT WAS WRONG. `auth.public_url` defaults to `http://{bind}`, so a deployment
    // that never set `OG_PUBLIC_GATEWAY_URL` handed out a loopback or wildcard address — one that
    // resolves to the CALLER's own machine, or to nothing at all.
    for bound in [
        "http://127.0.0.1:1337",
        "http://0.0.0.0:1337",
        "http://localhost:1337",
        "http://[::1]:1337",
    ] {
        let email = format!("routine-url-{}@og.local", uuid::Uuid::now_v7().simple());
        let h = harness(&database_url, &email, None, bound).await;
        let coworker = h.hire().await;
        let (_, created) = h.webhook_routine(&coworker).await;
        let url = created["webhook"]["url"].as_str().expect("url");
        assert_eq!(
            url,
            format!("/hooks/{}", hook_id_of(url)),
            "{bound} is not an address an outside caller can dial, so it must not be handed \
             out as one: {created}"
        );
    }

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

    // And an advertised address still wins even when the bind is a loopback one, which is the
    // ordinary production shape: bound to 127.0.0.1 behind a proxy that is the public name.
    let proxied = format!("routine-url-d-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(
        &database_url,
        &proxied,
        Some("https://hooks.example.com"),
        "http://127.0.0.1:1337",
    )
    .await;
    let coworker = h.hire().await;
    let (_, created) = h.webhook_routine(&coworker).await;
    let url = created["webhook"]["url"].as_str().expect("url");
    assert_eq!(
        url,
        format!("https://hooks.example.com/hooks/{}", hook_id_of(url)),
        "{created}"
    );
}

/// The row this routine is on in `GET /schedules`, polled until `done` says its last run is
/// where the test needs it. `lastRun` is read from the run journal, so it moves on its own as
/// the run does — which is exactly what a client polling this listing sees.
async fn wait_for_last_run(h: &Harness, id: &str, done: impl Fn(&Value) -> bool) -> Value {
    let mut row = Value::Null;
    for _ in 0..60 {
        let (status, rows) = h.get("/schedules").await;
        assert_eq!(status, 200, "{rows}");
        row = rows
            .as_array()
            .expect("an array")
            .iter()
            .find(|row| row["id"] == json!(id))
            .cloned()
            .expect("the routine");
        if done(&row["lastRun"]) {
            return row;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    panic!("the routine's lastRun never settled: {row}");
}

/// A ROUTINE THAT RUNS AND TELLS NOBODY IS A ROUTINE NOBODY TRUSTS (#177). Its result used to be
/// written as a seam-A transcript entry that no surviving route reads; the person saw nothing
/// while the run spent their points. The routine's own row now says what its newest run came to.
#[tokio::test]
async fn a_fired_routine_reports_its_last_run_on_its_row() {
    let database_url = database_or_skip!();
    let _clock = clock(&database_url, false).await;
    let email = format!("routine-last-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = advertising(&database_url, &email).await;
    let coworker = h.hire().await;
    let (id, created) = h.webhook_routine(&coworker).await;
    assert_eq!(
        created["lastRun"],
        Value::Null,
        "a routine that never ran has no last run, and `null` says so: {created}"
    );
    let key = created["webhook"]["key"].as_str().expect("key").to_string();
    let hook = hook_id_of(created["webhook"]["url"].as_str().expect("url")).to_string();

    assert_eq!(h.fire_hook(&hook, Some(&key)).await, 202);
    let row = wait_for_last_run(&h, &id, |last| last["status"] == json!("finished")).await;
    let last = &row["lastRun"];
    let run_id = last["runId"].as_str().expect("a run id");
    assert!(!run_id.is_empty(), "{row}");
    assert!(last["finishedAtMs"].is_i64(), "{row}");
    let summary = last["summary"].as_str().expect("a summary");
    assert!(
        summary.starts_with("Routine Inbox ran: "),
        "the summary opens with the routine's name, so it says why the coworker spoke: {summary}"
    );
    assert!(
        summary.contains("a webhook arrived; report in"),
        "and carries the head of the answer (the echo door repeats its prompt): {summary}"
    );

    assert_eq!(
        h.store
            .run_fired_by(&id, &RunId::from_stored(run_id))
            .await
            .expect("origin"),
        Some(opengrok_store::FiredBy::Webhook),
        "the routine's own log says it fired this run, and how"
    );

    // The whole run is AG-UI history under the routine's own thread, the same frames a chat
    // turn replays — `lastRun.runId` is the handle a client opens it by.
    let (status, thread) = h.get(&format!("/ag-ui/threads/{id}")).await;
    assert_eq!(status, 200, "{thread}");
    assert!(
        thread.to_string().contains(run_id),
        "the routine's thread replays the run lastRun names: {thread}"
    );
}

fn email(tag: &str) -> String {
    format!("routine-{tag}-{}@og.local", uuid::Uuid::now_v7().simple())
}

/// #235: AN EDIT IS WHAT THE NEXT RUN DOES. The pane saves an edit and the person presses "Run
/// now" to see it work; a run that opened with the old instruction would tell them the edit did
/// not take. The echo door repeats the prompt it was given, so the summary is the proof.
#[tokio::test]
async fn an_edited_routine_runs_now_with_its_new_prompt() {
    let database_url = database_or_skip!();
    let _clock = clock(&database_url, false).await;
    let h = advertising(&database_url, &email("edit-run")).await;
    let coworker = h.hire().await;
    let id = h.cron_routine(&coworker).await;

    let (status, row) = h
        .patch(
            &format!("/schedules/{id}"),
            json!({ "name": "Standup", "prompt": "summarise the standup notes" }),
        )
        .await;
    assert_eq!(status, 200, "{row}");
    assert_eq!(
        row["id"],
        json!(id),
        "an edit keeps the routine's id: {row}"
    );
    assert_eq!(row["name"], json!("Standup"));
    assert_eq!(row["prompt"], json!("summarise the standup notes"));
    assert_eq!(
        row["cron"],
        json!("0 0 9 * * MON"),
        "an edit that names no cron keeps it: {row}"
    );

    let (status, accepted) = h.post(&format!("/schedules/{id}/run"), json!({})).await;
    assert_eq!(status, 202, "{accepted}");
    let run_id = accepted["runId"].as_str().expect("a run id").to_string();

    let row = wait_for_last_run(&h, &id, |last| last["status"] == json!("finished")).await;
    assert_eq!(row["lastRun"]["runId"], json!(run_id), "{row}");
    let summary = row["lastRun"]["summary"].as_str().expect("a summary");
    assert!(
        summary.contains("summarise the standup notes"),
        "the run opened with the edited prompt, not the old one: {summary}"
    );
    assert!(
        !summary.contains("weekly report"),
        "the old prompt is gone: {summary}"
    );
}

/// Run now answers with the run it started, journals it under the routine's own thread, and the
/// routine's history says a person started it.
#[tokio::test]
async fn run_now_starts_a_run_the_history_calls_manual() {
    let database_url = database_or_skip!();
    let _clock = clock(&database_url, false).await;
    let h = advertising(&database_url, &email("run-now")).await;
    let coworker = h.hire().await;
    let id = h.cron_routine(&coworker).await;

    let (status, runs) = h.get(&format!("/schedules/{id}/runs")).await;
    assert_eq!(status, 200, "{runs}");
    assert_eq!(
        runs,
        json!([]),
        "a routine that never ran has an empty history, as an array"
    );

    let (status, accepted) = h.post(&format!("/schedules/{id}/run"), json!({})).await;
    assert_eq!(status, 202, "{accepted}");
    assert_eq!(accepted["accepted"], json!(true), "{accepted}");
    let run_id = accepted["runId"].as_str().expect("a run id").to_string();
    assert!(
        run_appears(&h.store, &id).await,
        "the run is journaled in the routine's thread"
    );
    wait_for_last_run(&h, &id, |last| last["status"] == json!("finished")).await;

    let (status, runs) = h.get(&format!("/schedules/{id}/runs")).await;
    assert_eq!(status, 200, "{runs}");
    let runs = runs.as_array().expect("an array");
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0]["runId"], json!(run_id));
    assert_eq!(runs[0]["cause"], json!("manual"));
    assert_eq!(
        runs[0]["status"],
        json!("ok"),
        "finished reads as ok: {runs:?}"
    );
    assert!(runs[0]["startedAtMs"].is_i64(), "{runs:?}");
    assert!(
        runs[0]["endedAtMs"].is_i64(),
        "an ended run says when: {runs:?}"
    );
}

/// A PERSON'S "RUN NOW" IS THE ONE WAKE A PAUSE DOES NOT REFUSE (the core's rule, `schedule.rs`),
/// and pressing it is not a resume: the routine is still paused afterwards, so the clock stays off.
#[tokio::test]
async fn a_paused_routine_runs_now_and_stays_paused() {
    let database_url = database_or_skip!();
    let _clock = clock(&database_url, false).await;
    let h = advertising(&database_url, &email("paused-run")).await;
    let coworker = h.hire().await;
    let id = h.cron_routine(&coworker).await;
    let (status, body) = h.post(&format!("/schedules/{id}/pause"), json!({})).await;
    assert_eq!(status, 204, "{body}");

    let (status, accepted) = h.post(&format!("/schedules/{id}/run"), json!({})).await;
    assert_eq!(status, 202, "{accepted}");
    let row = wait_for_last_run(&h, &id, |last| last["status"] == json!("finished")).await;
    assert_eq!(
        row["active"],
        json!(false),
        "still paused after it ran: {row}"
    );
    assert_eq!(
        row["nextDueMs"],
        Value::Null,
        "and the clock is still off: {row}"
    );
}

/// What an edit may not do: install a clock that never fires, put a clock on a hook, or move the
/// hook's address and key out from under whatever was wired to them.
#[tokio::test]
async fn an_edit_refuses_what_create_refuses_and_keeps_a_hook_where_it_was() {
    let database_url = database_or_skip!();
    let _clock = clock(&database_url, false).await;
    let h = advertising(&database_url, &email("edit-rules")).await;
    let coworker = h.hire().await;
    let id = h.cron_routine(&coworker).await;

    let (status, body) = h
        .patch(&format!("/schedules/{id}"), json!({ "cron": "not a cron" }))
        .await;
    assert_eq!(status, 422, "{body}");
    let (status, body) = h
        .patch(&format!("/schedules/{id}"), json!({ "prompt": "   " }))
        .await;
    assert_eq!(
        status, 422,
        "an empty prompt is refused, like create: {body}"
    );

    let (status, body) = h.patch(&format!("/schedules/{id}"), json!({})).await;
    assert_eq!(
        status, 422,
        "an edit that changes nothing is refused, not a silent 200: {body}"
    );
    let (status, body) = h
        .patch(&format!("/schedules/{id}"), json!({ "kind": "webhook" }))
        .await;
    assert_eq!(status, 422, "a routine's kind does not change: {body}");

    // Half an hour later on the same weekday: never the same instant, whatever today is.
    let before = h.row(&id).await["nextDueMs"].clone();
    let (status, row) = h
        .patch(&format!("/schedules/{id}"), json!({ "cron": "30 9 * * 1" }))
        .await;
    assert_eq!(status, 200, "{row}");
    assert_eq!(row["cron"], json!("0 30 9 * * MON"), "{row}");
    assert_ne!(
        row["nextDueMs"], before,
        "a new clock is a new next firing: {row}"
    );
    assert_eq!(
        h.row(&id).await["nextDueMs"],
        row["nextDueMs"],
        "and the listing agrees"
    );

    // A NEW CLOCK MAY FIRE SOONER. The projection refuses to move `next_due` earlier only when
    // the clock is unchanged; a person who changes weekly to every minute means the next minute.
    let weekly = row["nextDueMs"].as_i64().expect("nextDueMs");
    let (status, row) = h
        .patch(&format!("/schedules/{id}"), json!({ "cron": "* * * * *" }))
        .await;
    assert_eq!(status, 200, "{row}");
    assert!(
        row["nextDueMs"].as_i64().expect("nextDueMs") < weekly,
        "a clock changed to every minute fires within the minute: {row}"
    );
    let (status, _) = h
        .patch(&format!("/schedules/{id}"), json!({ "cron": "0 9 * * 1" }))
        .await;
    assert_eq!(status, 200);

    let (hook, created) = h.webhook_routine(&coworker).await;
    let (status, body) = h
        .patch(
            &format!("/schedules/{hook}"),
            json!({ "cron": "0 9 * * 1" }),
        )
        .await;
    assert_eq!(
        status, 422,
        "a clock on a hook is refused, not a silent kind change: {body}"
    );
    let (status, row) = h
        .patch(
            &format!("/schedules/{hook}"),
            json!({ "name": "Renamed hook" }),
        )
        .await;
    assert_eq!(status, 200, "{row}");
    assert_eq!(row["kind"], json!("webhook"), "{row}");
    assert_eq!(row["cron"], Value::Null, "{row}");
    assert_eq!(row["webhook"]["url"], created["webhook"]["url"], "{row}");
    assert_eq!(row["webhook"]["key"], created["webhook"]["key"], "{row}");
    assert_eq!(row["nextDueMs"], Value::Null, "a hook has no clock: {row}");
    assert!(
        row["lastRun"].is_null(),
        "never ran, and the edit reply says so: {row}"
    );
    let key = created["webhook"]["key"].as_str().expect("key");
    let hook_id = hook_id_of(created["webhook"]["url"].as_str().expect("url"));
    assert_eq!(
        h.fire_hook(hook_id, Some(key)).await,
        202,
        "the old key still fires it"
    );
}

/// Handing a routine to another coworker is checked like creating one for them, and the
/// listing — which the clock sweep claims from — says who has it now.
#[tokio::test]
async fn an_edit_can_hand_a_routine_to_another_coworker() {
    let database_url = database_or_skip!();
    let _clock = clock(&database_url, false).await;
    let h = advertising(&database_url, &email("edit-coworker")).await;
    let first = h.hire().await;
    let second = h.hire().await;
    let id = h.cron_routine(&first).await;

    let (status, body) = h
        .patch(
            &format!("/schedules/{id}"),
            json!({ "coworkerId": "cw_nobody" }),
        )
        .await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(h.row(&id).await["coworkerId"], json!(first));

    let (status, row) = h
        .patch(&format!("/schedules/{id}"), json!({ "coworkerId": second }))
        .await;
    assert_eq!(status, 200, "{row}");
    assert_eq!(row["coworkerId"], json!(second), "{row}");
    assert_eq!(
        h.row(&id).await["coworkerId"],
        json!(second),
        "the listing follows, or the pane would show the old coworker"
    );
}

/// Somebody else's routine is a 404 on every new route, the same answer as one that does not
/// exist; and no bearer is a 401.
#[tokio::test]
async fn another_account_cannot_edit_run_or_read_the_history() {
    let database_url = database_or_skip!();
    let _clock = clock(&database_url, false).await;
    let h = advertising(&database_url, &email("stranger-edit")).await;
    let coworker = h.hire().await;
    let id = h.cron_routine(&coworker).await;
    let stranger = h.stranger().await;

    let patch = h
        .call(
            "PATCH",
            &format!("/schedules/{id}"),
            Some(&stranger),
            json!({ "name": "mine" }),
        )
        .await;
    assert_eq!(patch.status, 404, "{}", patch.body);
    let run = h
        .call(
            "POST",
            &format!("/schedules/{id}/run"),
            Some(&stranger),
            json!({}),
        )
        .await;
    assert_eq!(run.status, 404, "{}", run.body);
    let runs = h
        .call(
            "GET",
            &format!("/schedules/{id}/runs"),
            Some(&stranger),
            Value::Null,
        )
        .await;
    assert_eq!(runs.status, 404, "{}", runs.body);
    let missing = h.get("/schedules/sch_nope/runs").await;
    assert_eq!(missing.0, 404, "no such is the same answer: {}", missing.1);
    let anonymous = h
        .call("POST", &format!("/schedules/{id}/run"), None, json!({}))
        .await;
    assert_eq!(anonymous.status, 401, "{}", anonymous.body);

    assert_eq!(
        h.row(&id).await["name"],
        json!("Weekly"),
        "the stranger's edit did not land"
    );
    assert_eq!(
        runs_in_thread(&h.store, &id).await,
        0,
        "and their run did not start"
    );
}

/// Run now counts against the same in-flight cap a webhook does: a person mashing the button is
/// a stampede of billed runs too.
#[tokio::test]
async fn run_now_with_too_much_already_running_is_refused() {
    let database_url = database_or_skip!();
    let _clock = clock(&database_url, false).await;
    let h = advertising(&database_url, &email("run-cap")).await;
    let coworker = h.hire().await;
    let id = h.cron_routine(&coworker).await;
    for _ in 0..opengrok_server::autonomy::MAX_RUNS_IN_FLIGHT {
        seed_running_run(&h.store, &h.account, &id).await;
    }
    let (status, body) = h.post(&format!("/schedules/{id}/run"), json!({})).await;
    assert_eq!(status, 429, "{body}");
    assert_eq!(
        runs_in_thread(&h.store, &id).await,
        opengrok_server::autonomy::MAX_RUNS_IN_FLIGHT as usize,
        "no fourth run was started"
    );
}

/// The history is newest first, bounded by `limit`, and a run nobody labelled is the clock's.
#[tokio::test]
async fn the_history_is_newest_first_and_bounded() {
    let database_url = database_or_skip!();
    let _clock = clock(&database_url, false).await;
    let h = advertising(&database_url, &email("history")).await;
    let coworker = h.hire().await;
    let id = h.cron_routine(&coworker).await;
    for _ in 0..3 {
        seed_clock_run(&h, &id).await;
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // A person's reply in the routine's thread: in the thread, but no firing of the routine's.
    seed_running_run(&h.store, &h.account, &id).await;
    let all = h.store.runs_for_thread(&id, 10).await.expect("runs");
    assert_eq!(all.len(), 4);

    let (status, runs) = h.get(&format!("/schedules/{id}/runs?limit=2")).await;
    assert_eq!(status, 200, "{runs}");
    let runs = runs.as_array().expect("an array");
    assert_eq!(runs.len(), 2, "{runs:?}");
    let (_, every) = h.get(&format!("/schedules/{id}/runs")).await;
    let every = every.as_array().expect("an array").clone();
    assert_eq!(every.len(), 3, "only the runs the routine fired: {every:?}");
    let started: Vec<i64> = every
        .iter()
        .map(|run| run["startedAtMs"].as_i64().expect("startedAtMs"))
        .collect();
    assert!(
        started.windows(2).all(|pair| pair[0] >= pair[1]),
        "newest first: {started:?}"
    );
    assert_eq!(
        runs[0]["runId"], every[0]["runId"],
        "the limit keeps the newest"
    );
    assert_eq!(runs[0]["cause"], json!("clock"), "{runs:?}");
    assert_eq!(runs[0]["status"], json!("running"), "{runs:?}");
    assert_eq!(
        runs[0]["endedAtMs"],
        Value::Null,
        "a running run has not ended: {runs:?}"
    );

    let (status, body) = h.get(&format!("/schedules/{id}/runs?limit=abc")).await;
    assert_eq!(
        status, 400,
        "a malformed limit is refused, not guessed at: {body}"
    );
}

/// ONLY THE CLAIM MOVES THE CLOCK FORWARD, AND NOTHING MOVES IT BACK. An edit reads the time,
/// then writes; the sweep can claim the routine in between. Written with the time it read, the
/// edit put `next_due` back on the slot the claim had just taken, and the next tick fired it a
/// second time. An edit that leaves the clock alone must leave `next_due` alone too.
#[tokio::test]
async fn an_edit_written_with_a_stale_clock_does_not_fire_the_slot_twice() {
    use opengrok_core::id::ScheduleId;
    use opengrok_core::schedule::{ScheduleCommand, Wake, next_fire_ms};

    let database_url = database_or_skip!();
    let _clock = clock(&database_url, true).await;
    let h = advertising(&database_url, &email("stale-edit")).await;
    let coworker = h.hire().await;
    let (status, body) = h
        .post(
            "/schedules",
            json!({ "coworkerId": coworker, "cron": "0 * * * * *", "prompt": "every minute" }),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    let id = ScheduleId::from_stored(body["id"].as_str().expect("id"));
    let slot = body["nextDueMs"].as_i64().expect("nextDueMs");

    // The sweep, one millisecond after the slot: it claims the routine and moves the clock on.
    let claimed = h
        .store
        .claim_due_schedules(slot + 1, 1_000_000)
        .await
        .expect("claim");
    assert!(
        claimed.iter().any(|due| due.id == id),
        "the routine was due"
    );

    // The edit, which read the time just BEFORE that claim.
    let stale = slot - 100;
    let (loaded, seq) = h.store.load_schedule(&id).await.expect("load");
    let mut after = loaded.clone();
    let events = loaded
        .decide(ScheduleCommand::Update {
            name: "renamed".to_string(),
            prompt: loaded.prompt.clone(),
            wake: Wake::Cron {
                cron: loaded.cron.clone(),
            },
            coworker_id: None,
            run_limits: None,
            tz: None,
            at_ms: stale,
        })
        .expect("update");
    for event in &events {
        after.apply(event);
    }
    assert_eq!(
        next_fire_ms(&after.cron, &after.tz, stale),
        Some(slot),
        "the stale time lands on the slot"
    );
    h.store
        .append_schedule(&id, &h.account, seq, &events, &after, stale)
        .await
        .expect("append the edit");

    let again = h
        .store
        .claim_due_schedules(slot + 1, 1_000_000)
        .await
        .expect("claim");
    sqlx::query("delete from schedule_view where id = $1")
        .bind(id.as_str())
        .execute(h.store.pool())
        .await
        .expect("clean up");
    assert!(
        !again.iter().any(|due| due.id == id),
        "the slot the sweep already took was handed out a second time"
    );
}

/// A coworker that was retired can neither be handed a routine nor run one. Its key is revoked
/// at retirement, so a firing would have run on the deployment's key, outside its spend cap.
#[tokio::test]
async fn a_retired_coworker_can_neither_take_nor_run_a_routine() {
    let database_url = database_or_skip!();
    let _clock = clock(&database_url, false).await;
    let h = advertising(&database_url, &email("retired")).await;
    let first = h.hire().await;
    let second = h.hire().await;
    let id = h.cron_routine(&first).await;

    let retired = h
        .call(
            "DELETE",
            &format!("/coworkers/{second}"),
            Some(&h.token()),
            Value::Null,
        )
        .await;
    assert!(
        retired.status < 300,
        "retire: {} {}",
        retired.status,
        retired.body
    );
    let (status, body) = h
        .patch(&format!("/schedules/{id}"), json!({ "coworkerId": second }))
        .await;
    assert_eq!(
        status, 404,
        "a retired coworker is not one to hand work to: {body}"
    );
    assert_eq!(h.row(&id).await["coworkerId"], json!(first));

    let retired = h
        .call(
            "DELETE",
            &format!("/coworkers/{first}"),
            Some(&h.token()),
            Value::Null,
        )
        .await;
    assert!(
        retired.status < 300,
        "retire: {} {}",
        retired.status,
        retired.body
    );
    let (status, body) = h.post(&format!("/schedules/{id}/run"), json!({})).await;
    assert_eq!(
        status, 409,
        "the routine is there; its coworker is not: {body}"
    );
    assert_eq!(runs_in_thread(&h.store, &id).await, 0, "and nothing ran");
}

/// Somebody else's runs in a routine's thread are not the routine's load. Thread ids are the
/// client's to choose, so a stranger who learns a routine id could otherwise park three runs on
/// it and hold its owner's "Run now" at 429 for as long as they liked.
#[tokio::test]
async fn another_accounts_runs_do_not_count_against_the_cap() {
    let database_url = database_or_skip!();
    let _clock = clock(&database_url, false).await;
    let h = advertising(&database_url, &email("cap-stranger")).await;
    let coworker = h.hire().await;
    let id = h.cron_routine(&coworker).await;
    let stranger = seed_account(&h.store, &email("cap-parker")).await;
    for _ in 0..opengrok_server::autonomy::MAX_RUNS_IN_FLIGHT {
        seed_running_run(&h.store, &stranger, &id).await;
    }
    let (status, body) = h.post(&format!("/schedules/{id}/run"), json!({})).await;
    assert_eq!(status, 202, "{body}");
}

/// A webhook routine written before keys were stored has no key to carry through an edit. It is
/// refused with the way out, rather than a message about hook ids nobody chose.
#[tokio::test]
async fn a_hook_with_no_stored_key_says_to_rotate_before_an_edit() {
    use opengrok_core::id::ScheduleId;
    use opengrok_core::schedule::{Schedule, ScheduleEvent, WakeKind};

    let database_url = database_or_skip!();
    let _clock = clock(&database_url, false).await;
    let h = advertising(&database_url, &email("keyless")).await;
    let coworker = h.hire().await;
    let id = ScheduleId::new();
    let events = vec![ScheduleEvent::Created {
        coworker_id: CoworkerId::from_stored(coworker.clone()),
        cron: String::new(),
        prompt: "an old hook".to_string(),
        name: "Old hook".to_string(),
        kind: WakeKind::Webhook,
        hook_id: format!("hook_{}", uuid::Uuid::now_v7()),
        secret_hash: "a-hash-from-before".to_string(),
        webhook_key: String::new(),
        run_limits: Default::default(),
        tz: "UTC".to_string(),
        at_ms: now_ms(),
    }];
    let state = Schedule::replay(&events);
    h.store
        .append_schedule(&id, &h.account, 0, &events, &state, now_ms())
        .await
        .expect("an old webhook routine");

    let (status, body) = h
        .patch(
            &format!("/schedules/{}", id.as_str()),
            json!({ "name": "Renamed" }),
        )
        .await;
    assert_eq!(status, 409, "{body}");
    assert!(
        body.to_string().contains("rotate"),
        "the refusal names the way out: {body}"
    );
}

/// ONE ROUTINE THAT CANNOT FIRE MUST NOT COST THE OTHERS THEIR SLOT. The claim advances every
/// claimed routine's clock before any fires; a failure that ended the tick skipped all the ones
/// after it. Here the first claimed routine's log holds an event this binary cannot read.
#[tokio::test]
async fn a_routine_that_cannot_fire_does_not_stop_the_tick() {
    use opengrok_core::id::ScheduleId;

    let database_url = database_or_skip!();
    let _clock = clock(&database_url, true).await;
    let h = in_seconds(&database_url, &email("tick")).await;
    let coworker = h.hire().await;
    let every_second =
        |prompt: &str| json!({ "coworkerId": coworker, "cron": "* * * * * *", "prompt": prompt });
    let (status, broken) = h.post("/schedules", every_second("broken")).await;
    assert_eq!(status, 201, "{broken}");
    let broken = broken["id"].as_str().expect("id").to_string();
    // Due a second later than the broken one, so the claim takes the broken one first.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let (status, healthy) = h.post("/schedules", every_second("healthy")).await;
    assert_eq!(status, 201, "{healthy}");
    let healthy = healthy["id"].as_str().expect("id").to_string();

    sqlx::query(
        "insert into events (stream_id, stream_seq, event_type, payload)
         values ($1, 2, 'schedule-from-the-future', '{\"type\":\"from-the-future\"}')",
    )
    .bind(opengrok_store::schedule_stream(&ScheduleId::from_stored(
        broken.clone(),
    )))
    .execute(h.store.pool())
    .await
    .expect("an unreadable event");
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    let host = HostState::new(h.agui.clone(), None);
    let tick = opengrok_server::autonomy::sweep::schedule_tick(&host).await;
    let healthy_ran = run_appears(&h.store, &healthy).await;

    // Every-second routines left behind would be claimed ahead of the next run's own.
    for id in [&broken, &healthy] {
        sqlx::query("delete from schedule_view where id = $1")
            .bind(id)
            .execute(h.store.pool())
            .await
            .expect("clean up");
    }
    assert!(
        tick.is_ok(),
        "one unreadable routine failed the whole tick: {tick:?}"
    );
    assert!(
        healthy_ran,
        "the routine claimed after the broken one never fired"
    );
}

/// THE LAST SLOT IS NOT HANDED OUT TWICE EITHER. A one-shot clock's claim leaves `next_due`
/// empty; an edit that read the time before that claim must not write the slot back.
#[tokio::test]
async fn an_edit_after_the_last_slot_was_claimed_does_not_fire_it_again() {
    use opengrok_core::id::ScheduleId;
    use opengrok_core::schedule::{ScheduleCommand, Wake};

    let database_url = database_or_skip!();
    let _clock = clock(&database_url, true).await;
    let h = advertising(&database_url, &email("last-slot")).await;
    let coworker = h.hire().await;
    let (status, body) = h
        .post(
            "/schedules",
            json!({ "coworkerId": coworker, "cron": "0 0 9 1 1 * 2031", "prompt": "once" }),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    let id = ScheduleId::from_stored(body["id"].as_str().expect("id"));
    let slot = body["nextDueMs"].as_i64().expect("nextDueMs");

    let claimed = h
        .store
        .claim_due_schedules(slot + 1, 1_000_000)
        .await
        .expect("claim");
    assert!(
        claimed.iter().any(|due| due.id == id),
        "the routine was due"
    );

    let stale = slot - 100;
    let (loaded, seq) = h.store.load_schedule(&id).await.expect("load");
    let mut after = loaded.clone();
    let events = loaded
        .decide(ScheduleCommand::Update {
            name: "renamed".to_string(),
            prompt: loaded.prompt.clone(),
            wake: Wake::Cron {
                cron: loaded.cron.clone(),
            },
            coworker_id: None,
            run_limits: None,
            tz: None,
            at_ms: stale,
        })
        .expect("update");
    for event in &events {
        after.apply(event);
    }
    h.store
        .append_schedule(&id, &h.account, seq, &events, &after, stale)
        .await
        .expect("append the edit");

    let again = h
        .store
        .claim_due_schedules(slot + 1, 1_000_000)
        .await
        .expect("claim");
    sqlx::query("delete from schedule_view where id = $1")
        .bind(id.as_str())
        .execute(h.store.pool())
        .await
        .expect("clean up");
    assert!(
        !again.iter().any(|due| due.id == id),
        "the one slot this clock had was handed out a second time"
    );
}

/// A RESUMED ROUTINE NEVER FIRES AT ONCE (#332): its next slot is counted strictly from the resume,
/// so a slot it missed while paused is dropped and nothing records it. The sweep finds nothing
/// due, and its history and `lastRun` stay empty; "run now" stays the one wake a person asks for.
/// One resume and one read of the list, which the corpus keeps.
#[tokio::test]
async fn a_resumed_routine_never_fires_at_once_and_records_nothing_it_missed() {
    use opengrok_core::schedule::next_fire_ms;
    let database_url = database_or_skip!();
    let _clock = clock(&database_url, true).await;
    let h = advertising(&database_url, &email("resumed")).await;
    let coworker = h.hire().await;
    let id = h.cron_routine(&coworker).await;
    let (status, body) = h.post(&format!("/schedules/{id}/pause"), json!({})).await;
    assert_eq!(status, 204, "{body}");
    // Paused through a week of its slots: its clock as it stood before the pause, long past.
    let a_week_ago = now_ms() - 7 * 24 * 60 * 60 * 1000;
    sqlx::query("update schedule_view set next_due_ms = $2 where id = $1")
        .bind(&id)
        .bind(a_week_ago)
        .execute(h.store.pool())
        .await
        .expect("a missed slot");

    let before = now_ms();
    let (status, body) = h.post(&format!("/schedules/{id}/resume"), json!({})).await;
    assert_eq!(status, 204, "{body}");
    let row = h.row(&id).await;
    let next = row["nextDueMs"].as_i64().expect("its next slot");
    let normal = [before, now_ms()].map(|from| next_fire_ms("0 9 * * 1", "UTC", from));
    assert!(next > before && normal.contains(&Some(next)), "{row}");
    assert_eq!(row["lastRun"], Value::Null, "nothing it missed is recorded");
    let claimed = h.store.claim_due_schedules(now_ms(), 1_000_000).await;
    let claimed = claimed.expect("claim");
    assert!(
        !claimed.iter().any(|due| due.id.as_str() == id),
        "it does not fire at once"
    );
    let history = h
        .store
        .runs_for_thread_owned_by(&id, &h.account, 10)
        .await
        .expect("runs");
    assert!(history.is_empty(), "no run");
    let (loaded, _) = h
        .store
        .load_schedule(&opengrok_core::id::ScheduleId::from_stored(id.as_str()))
        .await
        .expect("load");
    assert!(
        loaded.skipped.is_empty(),
        "no skip recorded for what it missed"
    );
}
