//! A monitor reacts to its owner's event log, and to nobody else's.
//!
//! THE EVENTS TABLE IS THE WHOLE DEPLOYMENT'S. It has no account column: every tenant's runs,
//! coworkers and connections append to the same log, and the monitor sweep reads it with one
//! global cursor. Before #179 a monitor matched on `event_type` alone, so Alice's `run-failed`
//! monitor woke her coworker — billed to her points — on every other tenant's failed run, and its
//! prompt quoted their stream id. What these tests hold is that ownership is resolved from the
//! stream before anything fires, and that an owner nobody can name matches nobody.
//!
//! The sweep is driven by hand (`monitor_tick`) so a firing is counted from the monitor's own
//! stream (`Fired` events) rather than timed against a background loop.
//!
//! ONE GLOBAL CURSOR, SO ONE TEST AT A TIME. Tests in one binary run in parallel, and a tick in one
//! test reads — and moves the cursor past — the events another test just seeded. `serial` takes a
//! Postgres advisory lock, which holds under `cargo test` and across nextest's per-test processes
//! alike; `.config/nextest.toml` also puts this binary in a group of one.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, CoworkerId, MonitorId, RunId};
use opengrok_core::run::{Run, RunCommand, RunView};
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::password::hash_password;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::host_state::HostState;
use opengrok_store::PgStore;
use serde_json::{Value, json};
use sqlx::Row;

/// Every test here ticks the one shared log cursor, so they run one at a time. A Postgres
/// advisory lock rather than a mutex: nextest runs each test in its own process, and the database
/// is what they share. It lives on its own connection, released when the guard drops.
async fn serial(database_url: &str) -> sqlx::PgConnection {
    use sqlx::Connection;
    let mut connection = sqlx::PgConnection::connect(database_url)
        .await
        .expect("connect for the serial lock");
    sqlx::query("select pg_advisory_lock($1)")
        .bind(0x5eed_0a11_i64)
        .execute(&mut connection)
        .await
        .expect("take the serial lock");
    connection
}

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
    host: HostState,
    account: AccountId,
    email: String,
}

/// A door that holds every turn open for a minute. A fired run then stays `running` for the whole
/// test, so what the cap counts is the cap and not how quickly the mock answered.
async fn harness(database_url: &str, email: &str) -> Harness {
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
    let minter = Arc::new(TokenMinter::new(b"a-monitor-scoped-to-its-owner-secret"));
    let auth = AuthState::new(store.clone(), minter, email.to_string());
    let agui = AgUiState {
        auth,
        door: Arc::new(MockDoor::echoing().min_turn_ms(60_000)),
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
    let host = HostState::new(agui.clone(), Some("http://opengrok.lan:1447".to_string()));
    let app = opengrok_server::router(agui.clone(), host.clone());
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
        host,
        account,
        email: email.to_string(),
    }
}

impl Harness {
    fn token_for(&self, account: &AccountId, email: &str) -> String {
        self.agui
            .auth
            .minter
            .mint_access(
                account.as_str(),
                "sess-monitors",
                email,
                "ultra",
                chrono::Utc::now().timestamp(),
                3600,
            )
            .expect("mint access")
    }

    fn token(&self) -> String {
        self.token_for(&self.account, &self.email)
    }

    async fn post_as(&self, token: &str, path: &str, body: Value) -> (u16, Value) {
        let res = self
            .client
            .post(format!("{}{path}", self.base))
            .header("authorization", format!("Bearer {token}"))
            .json(&body)
            .send()
            .await
            .expect("request");
        let status = res.status().as_u16();
        let text = res.text().await.expect("body");
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        self.post_as(&self.token(), path, body).await
    }

    async fn send_as(&self, method: &str, token: &str, path: &str, body: Value) -> (u16, Value) {
        let url = format!("{}{path}", self.base);
        let request = match method {
            "PATCH" => self.client.patch(url).json(&body),
            "GET" => self.client.get(url),
            _ => self.client.post(url).json(&body),
        };
        let res = request
            .header("authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("request");
        let status = res.status().as_u16();
        let text = res.text().await.expect("body");
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn patch(&self, path: &str, body: Value) -> (u16, Value) {
        self.send_as("PATCH", &self.token(), path, body).await
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        self.send_as("GET", &self.token(), path, Value::Null).await
    }

    async fn hire_as(&self, token: &str, name: &str) -> String {
        let (status, hired) = self
            .post_as(token, "/coworkers", json!({ "name": name }))
            .await;
        assert_eq!(status, 201, "{hired}");
        hired["id"].as_str().expect("coworker id").to_string()
    }

    async fn monitor(&self, coworker: &str, watches: &str) -> MonitorId {
        let (status, body) = self
            .post(
                "/monitors",
                json!({ "coworkerId": coworker, "watches": watches, "prompt": "something happened; look" }),
            )
            .await;
        assert_eq!(status, 201, "{body}");
        MonitorId::from_stored(body["id"].as_str().expect("monitor id"))
    }

    /// Another signed-in account on the same deployment: its id and a bearer for it.
    async fn bob(&self) -> (AccountId, String) {
        let email = format!("monitor-bob-{}@og.local", uuid::Uuid::now_v7().simple());
        let account = seed_account(&self.store, &email).await;
        let token = self.token_for(&account, &email);
        (account, token)
    }
}

/// A run that started and failed, written straight to the store as one account's (or nobody's).
async fn seed_failed_run(store: &PgStore, account: Option<&AccountId>, thread: &str) -> RunId {
    let id = RunId::new();
    let mut run = Run::default();
    let at_ms = now_ms();
    let mut events = run
        .decide(RunCommand::Start {
            thread_id: thread.to_string(),
            coworker_id: Some(CoworkerId::from_stored("cw_somebody")),
            model: Some("oag/cheap".to_string()),
            effort: Default::default(),
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
    let failed = run
        .decide(RunCommand::Fail {
            reason: "boom".to_string(),
            at_ms,
        })
        .expect("fail");
    for event in &failed {
        run.apply(event);
    }
    events.extend(failed);
    let view = RunView {
        id: id.clone(),
        thread_id: thread.to_string(),
        status: run.status,
        event_count: 0,
        updated_at_ms: at_ms,
    };
    store
        .append_run(&id, 0, &events, &view, account)
        .await
        .expect("append run");
    id
}

/// Move the shared cursor to the log's end without firing anything, so the events a test seeds
/// next are the only ones its ticks see.
async fn drain_log(store: &PgStore) {
    while !store.next_log_span(1000).await.expect("drain").is_empty() {}
}

/// Tick the sweep until the cursor has passed everything written so far.
async fn tick_until_past(h: &Harness) {
    let end: i64 = sqlx::query("select coalesce(max(id), 0) as end from events")
        .fetch_one(h.store.pool())
        .await
        .expect("log end")
        .try_get("end")
        .expect("end");
    for _ in 0..50 {
        opengrok_server::autonomy::sweep::monitor_tick(&h.host)
            .await
            .expect("a monitor tick");
        let cursor: i64 = sqlx::query("select last_event_id from monitor_cursor where id = 1")
            .fetch_one(h.store.pool())
            .await
            .expect("cursor")
            .try_get("last_event_id")
            .expect("cursor id");
        if cursor >= end {
            return;
        }
    }
    panic!("the cursor never reached {end}");
}

/// How many times this monitor has fired, by hand or by the log. Counted from the aggregate's
/// firings, not the stream's length: an edit or a pause is on the stream too.
async fn firings(h: &Harness, monitor: &MonitorId) -> i64 {
    let (loaded, _) = h.store.load_monitor(monitor).await.expect("load monitor");
    i64::try_from(loaded.manual_runs.len() + loaded.event_runs.len()).expect("a count")
}

/// Leave nothing active behind: a later run of this file ticks the same database.
async fn delete_monitor(h: &Harness, monitor: &MonitorId) {
    let status = h
        .client
        .delete(format!("{}/monitors/{monitor}", h.base))
        .header("authorization", format!("Bearer {}", h.token()))
        .send()
        .await
        .expect("delete a monitor")
        .status();
    assert!(status.is_success(), "delete answered {status}");
}

/// THE ONE #179 IS FOR. Bob's failed run is none of Alice's business; Alice's own is.
#[tokio::test]
async fn a_foreign_failed_run_fires_nothing_and_ones_own_fires_once() {
    let database_url = database_or_skip!();
    let _serial = serial(&database_url).await;
    let email = format!("monitor-alice-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let cw = h.hire_as(&h.token(), "Watcher").await;
    let monitor = h.monitor(&cw, "run-failed").await;
    let (bob, _) = h.bob().await;

    drain_log(&h.store).await;
    seed_failed_run(&h.store, Some(&bob), "thr-bob").await;
    tick_until_past(&h).await;
    assert_eq!(
        firings(&h, &monitor).await,
        0,
        "Bob's failed run must not wake Alice's coworker, bill Alice's points, or tell her \
         coworker the id of Bob's run"
    );

    seed_failed_run(&h.store, None, "thr-nobody").await;
    tick_until_past(&h).await;
    assert_eq!(
        firings(&h, &monitor).await,
        0,
        "a run with no owner belongs to nobody — it must match no monitor, not every monitor"
    );

    let own = seed_failed_run(&h.store, Some(&h.account), "thr-alice").await;
    tick_until_past(&h).await;
    assert_eq!(
        firings(&h, &monitor).await,
        1,
        "Alice's own failed run fires her monitor, exactly once"
    );

    // What the approvals queue reads as a card's `origin`: the fired run is the monitor's by its
    // own log, and a run on the monitor's thread that it never fired is not.
    let mut fired = None;
    for _ in 0..40 {
        if let Some(run) = h
            .store
            .runs_for_thread(monitor.as_str(), 1)
            .await
            .expect("thread")
            .into_iter()
            .next()
        {
            fired = Some(run.id);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    let fired = fired.expect("the monitor's run journaled under its id");
    assert_eq!(
        h.store
            .run_fired_by(monitor.as_str(), &fired)
            .await
            .expect("origin"),
        Some(opengrok_store::FiredBy::Monitor)
    );
    assert_eq!(
        h.store
            .run_fired_by(monitor.as_str(), &own)
            .await
            .expect("origin"),
        None,
        "a thread id spelled like a monitor's is not the monitor's word"
    );

    delete_monitor(&h, &monitor).await;
}

/// Not only runs: a coworker stream resolves to the account that hired it.
#[tokio::test]
async fn a_foreign_coworker_event_fires_nothing() {
    let database_url = database_or_skip!();
    let _serial = serial(&database_url).await;
    let email = format!("monitor-hire-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let cw = h.hire_as(&h.token(), "Watcher").await;
    let monitor = h.monitor(&cw, "coworker-hired").await;
    let (_, bob_token) = h.bob().await;

    drain_log(&h.store).await;
    h.hire_as(&bob_token, "Bob's hire").await;
    tick_until_past(&h).await;
    assert_eq!(
        firings(&h, &monitor).await,
        0,
        "Bob hiring a coworker is not an event in Alice's log"
    );

    h.hire_as(&h.token(), "Alice's second hire").await;
    tick_until_past(&h).await;
    assert_eq!(firings(&h, &monitor).await, 1, "Alice hiring one is");

    delete_monitor(&h, &monitor).await;
}

/// The published prefix table, read back one row at a time. An owner nobody can name — an org's
/// stream, a prefix this server never wrote, an unowned run — is `None`, which matches nobody.
#[tokio::test]
async fn every_stream_prefix_resolves_to_its_owner_or_to_nobody() {
    let database_url = database_or_skip!();
    let email = format!("monitor-owner-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let cw = h.hire_as(&h.token(), "Owned").await;
    let mine = Some(h.account.clone());

    let owner = |stream: String| {
        let store = h.store.clone();
        async move { store.stream_owner(&stream).await.expect("resolve") }
    };
    assert_eq!(owner(format!("account/{}", h.account)).await, mine);
    assert_eq!(owner(format!("coworker/{cw}")).await, mine);
    let run = seed_failed_run(&h.store, Some(&h.account), "thr-owned").await;
    assert_eq!(owner(format!("run/{run}")).await, mine);
    let orphan = seed_failed_run(&h.store, None, "thr-orphan").await;
    assert_eq!(owner(format!("run/{orphan}")).await, None);
    let monitor = h.monitor(&cw, "run-finished").await;
    assert_eq!(owner(format!("monitor/{monitor}")).await, mine);
    let (status, schedule) = h
        .post(
            "/schedules",
            json!({ "coworkerId": cw, "cron": "0 0 9 * * *", "prompt": "morning" }),
        )
        .await;
    assert_eq!(status, 201, "{schedule}");
    let schedule = schedule["id"].as_str().expect("schedule id");
    assert_eq!(owner(format!("schedule/{schedule}")).await, mine);
    assert_eq!(owner("org/org_anything".to_string()).await, None);
    assert_eq!(owner("room/rm_old".to_string()).await, None);
    assert_eq!(owner("no-slash-at-all".to_string()).await, None);
    assert_eq!(owner("coworker/cw_nobody_hired".to_string()).await, None);

    delete_monitor(&h, &monitor).await;
}

/// A typo in `watches` is refused, not accepted and silently never fired.
#[tokio::test]
async fn an_unknown_watch_is_refused_with_a_422() {
    let database_url = database_or_skip!();
    let email = format!("monitor-typo-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let cw = h.hire_as(&h.token(), "Typo").await;
    for watches in [
        "run-faild",
        "not-an-event",
        "run-emitted",
        "session-refreshed",
    ] {
        let (status, body) = h
            .post(
                "/monitors",
                json!({ "coworkerId": cw, "watches": watches, "prompt": "look" }),
            )
            .await;
        assert_eq!(status, 422, "watching {watches} answered {status}: {body}");
    }
}

/// A MONITOR MAY NOT BE DRIVEN INTO UNBOUNDED WORK. A burst of matching events in one span fires
/// at most `MAX_RUNS_IN_FLIGHT` runs while the earlier ones are still working.
#[tokio::test]
async fn firings_per_monitor_are_capped() {
    let database_url = database_or_skip!();
    let _serial = serial(&database_url).await;
    let email = format!("monitor-cap-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let cw = h.hire_as(&h.token(), "Burst").await;
    let monitor = h.monitor(&cw, "run-failed").await;

    drain_log(&h.store).await;
    for n in 0..5 {
        seed_failed_run(&h.store, Some(&h.account), &format!("thr-burst-{n}")).await;
    }
    tick_until_past(&h).await;
    assert_eq!(
        firings(&h, &monitor).await,
        opengrok_server::autonomy::MAX_RUNS_IN_FLIGHT,
        "five matches in one burst, and the monitor fired only as many runs as may be in flight"
    );

    delete_monitor(&h, &monitor).await;
}

/// The run this monitor started, once its first row is journaled. A firing is spawned, so the
/// 202 is the server accepting the wake, not the run existing yet.
async fn journaled_run(h: &Harness, run_id: &str) -> opengrok_core::run::Run {
    for _ in 0..60 {
        if let Ok((run, _)) = h.store.load_run(&RunId::from_stored(run_id)).await
            && run.started
        {
            return run;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    panic!("run {run_id} never journaled");
}

/// #235: AN EDIT IS WHAT THE NEXT RUN DOES. Edit a monitor, press "run now", and the run opens
/// with the edited prompt; the history lists it as the person's.
#[tokio::test]
async fn an_edited_monitor_runs_now_with_its_new_prompt() {
    let database_url = database_or_skip!();
    let _serial = serial(&database_url).await;
    let email = format!("monitor-edit-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let cw = h.hire_as(&h.token(), "Watcher").await;
    let monitor = h.monitor(&cw, "run-failed").await;

    let (status, runs) = h.get(&format!("/monitors/{monitor}/runs")).await;
    assert_eq!(status, 200, "{runs}");
    assert_eq!(
        runs,
        json!([]),
        "a monitor that never fired has an empty history, as an array"
    );

    let (status, row) = h
        .patch(
            &format!("/monitors/{monitor}"),
            json!({ "watches": "connection-disconnected", "prompt": "reconnect the mailbox" }),
        )
        .await;
    assert_eq!(status, 200, "{row}");
    assert_eq!(row["id"], json!(monitor.as_str()), "{row}");
    assert_eq!(row["watches"], json!("connection-disconnected"), "{row}");
    assert_eq!(row["prompt"], json!("reconnect the mailbox"), "{row}");
    assert_eq!(row["coworkerId"], json!(cw), "{row}");

    let (status, accepted) = h.post(&format!("/monitors/{monitor}/run"), json!({})).await;
    assert_eq!(status, 202, "{accepted}");
    assert_eq!(accepted["accepted"], json!(true), "{accepted}");
    let run_id = accepted["runId"].as_str().expect("a run id").to_string();
    let run = journaled_run(&h, &run_id).await;
    assert_eq!(
        run.thread_id,
        monitor.as_str(),
        "the run is in the monitor's own thread"
    );
    let prompt = serde_json::to_string(&run.prompt).expect("prompt");
    assert!(
        prompt.contains("reconnect the mailbox"),
        "the run opened with the edit: {prompt}"
    );
    assert!(
        !prompt.contains("something happened"),
        "not the old prompt: {prompt}"
    );

    let (status, runs) = h.get(&format!("/monitors/{monitor}/runs")).await;
    assert_eq!(status, 200, "{runs}");
    let runs = runs.as_array().expect("an array");
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0]["runId"], json!(run_id));
    assert_eq!(runs[0]["cause"], json!("manual"));
    assert_eq!(
        runs[0]["status"],
        json!("running"),
        "the door holds the turn open: {runs:?}"
    );
    assert_eq!(runs[0]["endedAtMs"], Value::Null, "{runs:?}");

    delete_monitor(&h, &monitor).await;
}

/// What create refuses, an edit refuses — the loop guard's root rule most of all — and an edit
/// that changes nothing is not a silent 200.
#[tokio::test]
async fn a_monitor_edit_refuses_what_create_refuses() {
    let database_url = database_or_skip!();
    let _serial = serial(&database_url).await;
    let email = format!("monitor-rules-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let cw = h.hire_as(&h.token(), "Watcher").await;
    let monitor = h.monitor(&cw, "run-failed").await;
    let path = format!("/monitors/{monitor}");

    for (body, why) in [
        (
            json!({ "watches": "monitor-fired" }),
            "watching firings is the cascade",
        ),
        (json!({ "watches": "run-faild" }), "a typo is refused"),
        (json!({ "prompt": "  " }), "an empty prompt is refused"),
        (json!({}), "an edit that changes nothing is refused"),
    ] {
        let (status, refusal) = h.patch(&path, body).await;
        assert_eq!(status, 422, "{why}: {refusal}");
    }
    let (status, refusal) = h.patch(&path, json!({ "coworkerId": "cw_nobody" })).await;
    assert_eq!(status, 404, "{refusal}");

    let second = h.hire_as(&h.token(), "Relief").await;
    let (status, row) = h.patch(&path, json!({ "coworkerId": second })).await;
    assert_eq!(status, 200, "{row}");
    let (_, rows) = h.get("/monitors").await;
    let listed = rows
        .as_array()
        .expect("an array")
        .iter()
        .find(|row| row["id"] == json!(monitor.as_str()))
        .cloned()
        .expect("the monitor");
    assert_eq!(
        listed["coworkerId"],
        json!(second),
        "the listing follows the handover: {listed}"
    );

    delete_monitor(&h, &monitor).await;
}

/// A PERSON'S RUN NOW IS THE ONE WAKE A PAUSE DOES NOT REFUSE; the monitor stays paused.
#[tokio::test]
async fn a_paused_monitor_runs_now_and_stays_paused() {
    let database_url = database_or_skip!();
    let _serial = serial(&database_url).await;
    let email = format!("monitor-paused-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let cw = h.hire_as(&h.token(), "Watcher").await;
    let monitor = h.monitor(&cw, "run-failed").await;
    let (status, body) = h
        .post(&format!("/monitors/{monitor}/pause"), json!({}))
        .await;
    assert_eq!(status, 204, "{body}");

    let (status, accepted) = h.post(&format!("/monitors/{monitor}/run"), json!({})).await;
    assert_eq!(status, 202, "{accepted}");
    journaled_run(&h, accepted["runId"].as_str().expect("run id")).await;
    let (_, rows) = h.get("/monitors").await;
    let listed = rows
        .as_array()
        .expect("an array")
        .iter()
        .find(|row| row["id"] == json!(monitor.as_str()))
        .cloned()
        .expect("the monitor");
    assert_eq!(listed["active"], json!(false), "still paused: {listed}");

    delete_monitor(&h, &monitor).await;
}

/// THE LOOP GUARD HOLDS FOR A RUN A PERSON STARTED. A monitor on `run-started`, pressed by hand,
/// starts a run whose own `run-started` must not wake it again. A second, unrelated run of the
/// owner's proves the tick did see the log.
#[tokio::test]
async fn a_monitors_own_manual_run_never_wakes_it() {
    let database_url = database_or_skip!();
    let _serial = serial(&database_url).await;
    let email = format!("monitor-loop-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let cw = h.hire_as(&h.token(), "Watcher").await;
    let monitor = h.monitor(&cw, "run-started").await;
    drain_log(&h.store).await;

    let (status, accepted) = h.post(&format!("/monitors/{monitor}/run"), json!({})).await;
    assert_eq!(status, 202, "{accepted}");
    journaled_run(&h, accepted["runId"].as_str().expect("run id")).await;
    seed_failed_run(&h.store, Some(&h.account), "thr-unrelated").await;
    tick_until_past(&h).await;

    assert_eq!(
        firings(&h, &monitor).await,
        2,
        "the hand-pressed run and the unrelated run's start: never its own run's start"
    );

    delete_monitor(&h, &monitor).await;
}

/// Somebody else's monitor is a 404 on every new route; and "run now" is held to the same
/// in-flight cap the log's firings are.
#[tokio::test]
async fn a_strangers_monitor_is_404_and_run_now_is_capped() {
    let database_url = database_or_skip!();
    let _serial = serial(&database_url).await;
    let email = format!(
        "monitor-stranger-{}@og.local",
        uuid::Uuid::now_v7().simple()
    );
    let h = harness(&database_url, &email).await;
    let cw = h.hire_as(&h.token(), "Watcher").await;
    let monitor = h.monitor(&cw, "run-failed").await;
    let (_, bob) = h.bob().await;

    for (method, path) in [
        ("PATCH", format!("/monitors/{monitor}")),
        ("POST", format!("/monitors/{monitor}/run")),
        ("GET", format!("/monitors/{monitor}/runs")),
    ] {
        let (status, body) = h
            .send_as(method, &bob, &path, json!({ "prompt": "mine now" }))
            .await;
        assert_eq!(status, 404, "{method} {path}: {body}");
    }
    assert_eq!(
        firings(&h, &monitor).await,
        0,
        "the stranger started nothing"
    );

    // The door holds each turn for a minute, so these stay in flight.
    for _ in 0..opengrok_server::autonomy::MAX_RUNS_IN_FLIGHT {
        let (status, body) = h.post(&format!("/monitors/{monitor}/run"), json!({})).await;
        assert_eq!(status, 202, "{body}");
    }
    let (status, body) = h.post(&format!("/monitors/{monitor}/run"), json!({})).await;
    assert_eq!(status, 429, "{body}");
    assert_eq!(
        firings(&h, &monitor).await,
        opengrok_server::autonomy::MAX_RUNS_IN_FLIGHT,
        "no fourth firing was recorded"
    );

    delete_monitor(&h, &monitor).await;
}

/// An edit to what a monitor watches is what the next tick matches: the old event type no longer
/// wakes it, the new one does.
#[tokio::test]
async fn after_an_edit_the_monitor_wakes_on_the_new_event_only() {
    let database_url = database_or_skip!();
    let _serial = serial(&database_url).await;
    let email = format!("monitor-rewatch-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let cw = h.hire_as(&h.token(), "Watcher").await;
    let monitor = h.monitor(&cw, "run-failed").await;
    let (status, row) = h
        .patch(
            &format!("/monitors/{monitor}"),
            json!({ "watches": "run-started" }),
        )
        .await;
    assert_eq!(status, 200, "{row}");

    drain_log(&h.store).await;
    // One run of the owner's: it writes a `run-started` and a `run-failed`. Only the first may
    // wake the monitor now.
    seed_failed_run(&h.store, Some(&h.account), "thr-rewatch").await;
    tick_until_past(&h).await;
    assert_eq!(
        firings(&h, &monitor).await,
        1,
        "woken once, by the start, not the failure"
    );
    // The firing is spawned, so its run is listed once it has journaled its first row.
    let (loaded, _) = h.store.load_monitor(&monitor).await.expect("load");
    let fired = loaded
        .event_runs
        .iter()
        .next()
        .cloned()
        .expect("the log's firing");
    journaled_run(&h, &fired).await;
    let (status, runs) = h.get(&format!("/monitors/{monitor}/runs")).await;
    assert_eq!(status, 200, "{runs}");
    assert_eq!(runs[0]["runId"], json!(fired), "{runs}");
    assert_eq!(
        runs[0]["cause"],
        json!("event"),
        "the history calls it the log's firing: {runs}"
    );

    delete_monitor(&h, &monitor).await;
}

/// ONE MONITOR THAT CANNOT FIRE MUST NOT COST THE OTHERS THEIR MATCH. The cursor is past the span
/// before any monitor fires, so a failure that ended the tick dropped every other monitor's
/// matches in it for good. Here the first monitor's log holds an event this binary cannot read.
#[tokio::test]
async fn a_monitor_that_cannot_fire_does_not_cost_the_others_their_match() {
    let database_url = database_or_skip!();
    let _serial = serial(&database_url).await;
    let email = format!("monitor-broken-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let cw = h.hire_as(&h.token(), "Watcher").await;
    let broken = h.monitor(&cw, "run-failed").await;
    let healthy = h.monitor(&cw, "run-failed").await;
    sqlx::query(
        "insert into events (stream_id, stream_seq, event_type, payload)
         values ($1, 2, 'monitor-from-the-future', '{\"type\":\"from-the-future\"}')",
    )
    .bind(opengrok_store::monitor_stream(&broken))
    .execute(h.store.pool())
    .await
    .expect("an unreadable event");

    drain_log(&h.store).await;
    seed_failed_run(&h.store, Some(&h.account), "thr-broken").await;
    tick_until_past(&h).await;
    assert_eq!(
        firings(&h, &healthy).await,
        1,
        "the healthy monitor still fired"
    );

    sqlx::query("delete from monitor_view where id = $1")
        .bind(broken.as_str())
        .execute(h.store.pool())
        .await
        .expect("clean up the broken monitor");
    delete_monitor(&h, &healthy).await;
}

/// THE AGGREGATE HAS THE LAST WORD ON WHAT A MONITOR WATCHES. The sweep matches on the row it
/// read at the start of the tick; an edit landing since must not let it fire on the old type.
/// The stale row is written straight to the projection to stand for that moment.
#[tokio::test]
async fn a_stale_row_does_not_fire_a_monitor_on_what_it_no_longer_watches() {
    let database_url = database_or_skip!();
    let _serial = serial(&database_url).await;
    let email = format!("monitor-stale-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let cw = h.hire_as(&h.token(), "Watcher").await;
    let monitor = h.monitor(&cw, "run-failed").await;
    let (status, row) = h
        .patch(
            &format!("/monitors/{monitor}"),
            json!({ "watches": "coworker-renamed" }),
        )
        .await;
    assert_eq!(status, 200, "{row}");
    sqlx::query("update monitor_view set watches = 'run-failed' where id = $1")
        .bind(monitor.as_str())
        .execute(h.store.pool())
        .await
        .expect("stand in for a row read before the edit");

    drain_log(&h.store).await;
    seed_failed_run(&h.store, Some(&h.account), "thr-stale").await;
    tick_until_past(&h).await;
    assert_eq!(
        firings(&h, &monitor).await,
        0,
        "it no longer watches run-failed"
    );

    delete_monitor(&h, &monitor).await;
}
