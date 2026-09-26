//! A person's conversations, listed by the server (#230).
//!
//! `GET /ag-ui/threads/{id}` hands back a conversation to whoever already holds its id, and
//! nothing handed out the ids: NativeChat kept its sidebar in local SQLite, so a fresh device
//! signed in to no history at all. `GET /ag-ui/threads` is the list. It is owner-filtered from the
//! bearer, newest first, leaves out what the person hid and what was never a conversation (the
//! MCP door's audit rows), and never shows one account another's threads — including on an
//! org-shared coworker, where two members write the SAME thread id and only the run's owner tells
//! them apart. Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, CoworkerId, MonitorId, RunId, ScheduleId};
use opengrok_core::monitor::{Monitor, MonitorCommand};
use opengrok_core::run::{Run, RunCommand, RunEvent, RunStatus, RunView};
use opengrok_core::schedule::{Schedule, ScheduleCommand, Wake};
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
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

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::now_v7().simple())
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

struct Person {
    id: AccountId,
    token: String,
}

struct Harness {
    base: String,
    client: reqwest::Client,
    store: PgStore,
    minter: Arc<TokenMinter>,
}

async fn harness(database_url: &str) -> Harness {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let minter = Arc::new(TokenMinter::new(b"thread-list-test-secret"));
    let auth = AuthState::new(store.clone(), minter.clone(), "owner@og.local".to_string());
    let agui = AgUiState {
        auth,
        door: Arc::new(MockDoor::echoing_the_system_prompt()),
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
    let gateway = HostState::new(agui.clone(), Some("http://opengrok.lan:1447".to_string()));
    let app = opengrok_server::router(agui.clone(), gateway);
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
        minter,
    }
}

impl Harness {
    /// A signed-in person, in `org` or in none. The org lands on both the command and the view,
    /// because sharing reads the projection and the aggregate is what a later load rebuilds it
    /// from.
    async fn person(&self, first: &str, org: Option<&str>) -> Person {
        let id = AccountId::new();
        let email = format!("{}@og.local", unique(&first.to_lowercase()));
        let at_ms = now_ms();
        let events = Account::default()
            .decide(AccountCommand::Register {
                email: email.clone(),
                password_hash: "x".to_string(),
                first_name: first.to_string(),
                last_name: "Tester".to_string(),
                org_id: org.unwrap_or_default().to_string(),
                plan: Plan::Ultra,
                verified: true,
                enabled: true,
                at_ms,
            })
            .expect("register");
        let view = AccountView {
            id: id.clone(),
            email: email.clone(),
            plan: Plan::Ultra,
            trial: false,
            updated_at_ms: at_ms,
            password_hash: Some("x".to_string()),
            first_name: first.to_string(),
            last_name: "Tester".to_string(),
            org_id: org.map(str::to_string),
            verified: true,
            enabled: true,
            avatar_url: None,
        };
        self.store
            .append_account(&id, 0, &events, &view)
            .await
            .expect("append account");
        let token = self
            .minter
            .mint_access(
                id.as_str(),
                "sess-thread-list",
                &email,
                "ultra",
                chrono::Utc::now().timestamp(),
                3600,
            )
            .expect("mint access");
        Person { id, token }
    }

    async fn send_with(
        &self,
        bearer: Option<&str>,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut request = self.client.request(method, format!("{}{path}", self.base));
        if let Some(bearer) = bearer {
            request = request.header("Authorization", format!("Bearer {bearer}"));
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let res = request.send().await.expect("request");
        let status = res.status().as_u16();
        let text = res.text().await.unwrap_or_default();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn send(
        &self,
        who: &Person,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        self.send_with(Some(&who.token), method, path, body).await
    }

    async fn hire(&self, who: &Person, name: &str) -> String {
        let (status, body) = self
            .send(
                who,
                reqwest::Method::POST,
                "/coworkers",
                Some(json!({ "name": name })),
            )
            .await;
        assert_eq!(status, 201, "hire {name}: {body}");
        body["id"].as_str().expect("id").to_string()
    }

    async fn share(&self, who: &Person, id: &str) {
        let (status, body) = self
            .send(
                who,
                reqwest::Method::PATCH,
                &format!("/coworkers/{id}"),
                Some(json!({ "visibility": "org" })),
            )
            .await;
        assert_eq!(status, 200, "share {id}: {body}");
    }

    /// One real turn on the AG-UI door, on the coworker's conversation, as the app sends it.
    async fn turn(&self, who: &Person, coworker: &str, words: &str) -> String {
        let run_id = uuid::Uuid::now_v7().to_string();
        let res = self
            .client
            .post(format!("{}/ag-ui", self.base))
            .header("Authorization", format!("Bearer {}", who.token))
            .json(&json!({
                "threadId": format!("gateway-{coworker}"),
                "runId": run_id,
                "messages": [{ "id": unique("m"), "role": "user", "content": words }],
                "forwardedProps": { "coworkerId": coworker },
            }))
            .send()
            .await
            .expect("ag-ui turn");
        let status = res.status().as_u16();
        let sse = res.text().await.unwrap_or_default();
        assert_eq!(status, 200, "{sse}");
        run_id
    }

    async fn hide(&self, who: &Person, run: &RunId) {
        let (status, body) = self
            .send(
                who,
                reqwest::Method::POST,
                &format!("/ag-ui/runs/{}/hide", run.as_str()),
                None,
            )
            .await;
        assert_eq!(status, 204, "hide {}: {body}", run.as_str());
    }

    /// The list, asserted to be the array the route promises.
    async fn list(&self, who: &Person, query: &str) -> Vec<Value> {
        let (status, body) = self
            .send(
                who,
                reqwest::Method::GET,
                &format!("/ag-ui/threads{query}"),
                None,
            )
            .await;
        assert_eq!(status, 200, "GET /ag-ui/threads{query}: {body}");
        body.as_array()
            .cloned()
            .unwrap_or_else(|| panic!("the thread list is an array, always: {body}"))
    }

    async fn thread_ids(&self, who: &Person, query: &str) -> Vec<String> {
        self.list(who, query)
            .await
            .iter()
            .map(|row| row["threadId"].as_str().expect("threadId").to_string())
            .collect()
    }
}

fn row<'a>(rows: &'a [Value], thread: &str) -> &'a Value {
    rows.iter()
        .find(|row| row["threadId"] == thread)
        .unwrap_or_else(|| panic!("{thread} is listed: {rows:?}"))
}

fn record(run: &mut Run, log: &mut Vec<RunEvent>, produced: Vec<RunEvent>) {
    for event in &produced {
        run.apply(event);
    }
    log.extend(produced);
}

/// One whole finished run journaled under a thread, with the coworker and the person's words
/// its start carries — a real log, so the list reads what a real turn would have left.
async fn seed_run(
    store: &PgStore,
    account: &AccountId,
    thread: &str,
    coworker: Option<&str>,
    prompt: Option<Vec<Value>>,
    at_ms: i64,
) -> RunId {
    let id = RunId::new();
    let mut run = Run::default();
    let mut log = Vec::new();
    let produced = run
        .decide(RunCommand::Start {
            thread_id: thread.to_string(),
            coworker_id: coworker.map(|id| CoworkerId::from_stored(id.to_string())),
            model: None,
            system: None,
            skill_id: None,
            prompt,
            at_ms,
        })
        .expect("start");
    record(&mut run, &mut log, produced);
    let produced = run.decide(RunCommand::Finish { at_ms }).expect("finish");
    record(&mut run, &mut log, produced);
    let view = RunView {
        id: id.clone(),
        thread_id: thread.to_string(),
        status: run.status,
        event_count: run.emitted.len() as i64,
        updated_at_ms: at_ms,
    };
    store
        .append_run(&id, 0, &log, &view, Some(account))
        .await
        .expect("append run");
    id
}

fn said(words: &str) -> Option<Vec<Value>> {
    Some(vec![
        json!({ "id": unique("m"), "role": "user", "content": words }),
    ])
}

/// A run the projection knows about whose log has no start: it never took its turn.
async fn seed_run_that_never_started(
    store: &PgStore,
    account: &AccountId,
    thread: &str,
    at_ms: i64,
) -> RunId {
    let id = RunId::new();
    let view = RunView {
        id: id.clone(),
        thread_id: thread.to_string(),
        status: RunStatus::Running,
        event_count: 0,
        updated_at_ms: at_ms,
    };
    store
        .append_run(&id, 0, &[], &view, Some(account))
        .await
        .expect("append run view");
    id
}

async fn seed_schedule(
    store: &PgStore,
    owner: &AccountId,
    coworker: &str,
    name: &str,
    wake: Wake,
) -> ScheduleId {
    let id = ScheduleId::new();
    let at_ms = now_ms();
    let events = Schedule::default()
        .decide(ScheduleCommand::Create {
            coworker_id: CoworkerId::from_stored(coworker.to_string()),
            prompt: "check the queue".to_string(),
            name: name.to_string(),
            wake,
            at_ms,
        })
        .expect("create schedule");
    let state = Schedule::replay(&events);
    store
        .append_schedule(&id, owner, 0, &events, &state, at_ms)
        .await
        .expect("append schedule");
    id
}

async fn seed_monitor(store: &PgStore, owner: &AccountId, coworker: &str) -> MonitorId {
    let id = MonitorId::new();
    let at_ms = now_ms();
    let events = Monitor::default()
        .decide(MonitorCommand::Create {
            coworker_id: CoworkerId::from_stored(coworker.to_string()),
            watches: "run-failed".to_string(),
            prompt: "a run failed; find out why".to_string(),
            at_ms,
        })
        .expect("create monitor");
    let state = Monitor::replay(&events);
    store
        .append_monitor(&id, owner, 0, &events, &state, at_ms)
        .await
        .expect("append monitor");
    id
}

/// Two members of one org talk to one shared coworker. The app names a coworker's conversation
/// after the coworker, so both write `gateway-{coworker}` — the same thread id — and the list must
/// still hand each of them their own conversation: their own last turn, and their own first words
/// as its title, never the other member's.
#[tokio::test]
async fn two_members_on_one_shared_coworker_each_list_only_their_own_conversation() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let org = unique("org");
    let owner = h.person("Ann", Some(&org)).await;
    let member = h.person("Ben", Some(&org)).await;
    let ada = h.hire(&owner, "Ada").await;
    h.share(&owner, &ada).await;

    let owners_first = h.turn(&owner, &ada, "the owner's question").await;
    let members_run = h.turn(&member, &ada, "the member's question").await;
    let owners_last = h.turn(&owner, &ada, "the owner's follow-up").await;
    assert_ne!(owners_first, owners_last);

    let thread = format!("gateway-{ada}");
    let mine = h.list(&member, "").await;
    assert_eq!(mine.len(), 1, "the member has one conversation: {mine:?}");
    let listed = row(&mine, &thread);
    assert_eq!(
        listed["lastRunId"], members_run,
        "the member's own last turn, not the owner's newer one: {listed}"
    );
    assert_eq!(listed["title"], "the member's question", "{listed}");
    assert_eq!(listed["coworkerId"], ada.as_str(), "{listed}");
    assert_eq!(listed["origin"], "chat", "{listed}");
    assert!(listed["lastStatus"].is_string(), "{listed}");
    assert!(listed["updatedAtMs"].is_i64(), "{listed}");

    let theirs = h.list(&owner, "").await;
    assert_eq!(
        theirs.len(),
        1,
        "the owner has one conversation: {theirs:?}"
    );
    let listed = row(&theirs, &thread);
    assert_eq!(listed["lastRunId"], owners_last, "{listed}");
    assert_eq!(
        listed["title"], "the owner's question",
        "the owner's first words, not the member's: {listed}"
    );
}

/// A hidden turn is gone from the list the way it is gone from the thread: the thread moves back
/// to its last visible turn, its title to its first visible one, and a thread whose every turn
/// was hidden is not listed at all.
#[tokio::test]
async fn hidden_turns_move_a_thread_back_and_hiding_them_all_removes_it() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let me = h.person("Cal", None).await;
    let base = now_ms();

    let first = seed_run(&h.store, &me.id, "talk", None, said("first"), base).await;
    let second = seed_run(&h.store, &me.id, "talk", None, said("second"), base + 10).await;
    let third = seed_run(&h.store, &me.id, "talk", None, said("third"), base + 20).await;

    let rows = h.list(&me, "").await;
    let listed = row(&rows, "talk");
    assert_eq!(listed["lastRunId"], third.as_str(), "{listed}");
    assert_eq!(listed["updatedAtMs"], base + 20, "{listed}");
    assert_eq!(listed["title"], "first", "{listed}");

    h.hide(&me, &third).await;
    let rows = h.list(&me, "").await;
    let listed = row(&rows, "talk");
    assert_eq!(
        listed["lastRunId"],
        second.as_str(),
        "the last VISIBLE turn: {listed}"
    );
    assert_eq!(listed["updatedAtMs"], base + 10, "{listed}");

    h.hide(&me, &first).await;
    let rows = h.list(&me, "").await;
    let listed = row(&rows, "talk");
    assert_eq!(
        listed["title"], "second",
        "the first VISIBLE turn names it: {listed}"
    );

    h.hide(&me, &second).await;
    assert_eq!(
        h.list(&me, "").await,
        Vec::<Value>::new(),
        "a thread with nothing visible left is not listed"
    );
}

/// Newest first, `limit` honoured and clamped, and the keyset cursor walks the whole list with no
/// thread twice and none skipped — including three threads that moved in the same millisecond on
/// a page boundary, and a thread with an older run on the far side of the cursor.
#[tokio::test]
async fn the_list_is_newest_first_and_pages_without_a_duplicate_or_a_gap() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let me = h.person("Dee", None).await;
    let base = now_ms();

    seed_run(&h.store, &me.id, "oldest", None, said("a"), base + 100).await;
    seed_run(&h.store, &me.id, "multi", None, said("b"), base + 150).await;
    seed_run(&h.store, &me.id, "multi", None, said("c"), base + 250).await;
    seed_run(&h.store, &me.id, "tie-a", None, said("d"), base + 300).await;
    seed_run(&h.store, &me.id, "tie-b", None, said("e"), base + 300).await;
    seed_run(&h.store, &me.id, "tie-c", None, said("f"), base + 300).await;

    let everything = h.thread_ids(&me, "").await;
    assert_eq!(
        everything,
        vec!["tie-c", "tie-b", "tie-a", "multi", "oldest"],
        "newest first; a tie falls back to the thread id, descending"
    );
    assert_eq!(h.thread_ids(&me, "?limit=2").await, vec!["tie-c", "tie-b"]);
    assert_eq!(
        h.thread_ids(&me, "?limit=0").await.len(),
        1,
        "a limit below one is one"
    );
    assert_eq!(
        h.thread_ids(&me, "?limit=100000").await,
        everything,
        "a huge limit is clamped, not refused"
    );

    let mut walked = Vec::new();
    let mut query = "?limit=2".to_string();
    for _ in 0..10 {
        let page = h.list(&me, &query).await;
        let Some(last) = page.last() else { break };
        query = format!(
            "?limit=2&before={}&beforeThreadId={}",
            last["updatedAtMs"],
            last["threadId"].as_str().expect("threadId")
        );
        walked.extend(
            page.iter()
                .map(|row| row["threadId"].as_str().expect("threadId").to_string()),
        );
    }
    assert_eq!(
        walked, everything,
        "paging walks the list once: no duplicate, no gap"
    );

    assert_eq!(
        h.thread_ids(&me, &format!("?before={}", base + 300)).await,
        vec!["multi", "oldest"],
        "`before` alone is strictly older than that millisecond"
    );
    let (status, body) = h
        .send(
            &me,
            reqwest::Method::GET,
            "/ag-ui/threads?beforeThreadId=tie-b",
            None,
        )
        .await;
    assert_eq!(
        status, 400,
        "a thread-id cursor with no time is a client bug, not page one: {body}"
    );
    assert!(body["error"].is_string(), "{body}");
}

/// `coworkerId` narrows the list to one coworker's conversations, and each row says what started
/// it: a chat, a cron schedule, a webhook routine or a monitor — read from the caller's own
/// routines, so another account's routine id used as a chat thread reads `chat` and lends it no
/// name.
#[tokio::test]
async fn the_list_filters_by_coworker_and_names_each_threads_origin() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let me = h.person("Eve", None).await;
    let stranger = h.person("Fay", None).await;
    let base = now_ms();

    seed_run(
        &h.store,
        &me.id,
        "chat-a",
        Some("cw_a"),
        said("to a"),
        base + 1,
    )
    .await;
    seed_run(
        &h.store,
        &me.id,
        "chat-b",
        Some("cw_b"),
        said("to b"),
        base + 2,
    )
    .await;

    let cron = seed_schedule(
        &h.store,
        &me.id,
        "cw_a",
        "Morning digest",
        Wake::Cron {
            cron: "0 0 9 * * *".to_string(),
        },
    )
    .await;
    let hook = seed_schedule(
        &h.store,
        &me.id,
        "cw_a",
        "Deploy hook",
        Wake::Webhook {
            hook_id: unique("hook"),
            secret_hash: "0".repeat(64),
            webhook_key: unique("key"),
        },
    )
    .await;
    let unnamed = seed_schedule(
        &h.store,
        &me.id,
        "cw_a",
        "",
        Wake::Cron {
            cron: "0 0 10 * * *".to_string(),
        },
    )
    .await;
    let monitor = seed_monitor(&h.store, &me.id, "cw_a").await;
    let theirs = seed_schedule(
        &h.store,
        &stranger.id,
        "cw_z",
        "Their secret routine",
        Wake::Cron {
            cron: "0 0 11 * * *".to_string(),
        },
    )
    .await;

    seed_run(
        &h.store,
        &me.id,
        cron.as_str(),
        Some("cw_a"),
        said("check the queue"),
        base + 3,
    )
    .await;
    seed_run(
        &h.store,
        &me.id,
        hook.as_str(),
        Some("cw_a"),
        said("deploy landed"),
        base + 4,
    )
    .await;
    seed_run(
        &h.store,
        &me.id,
        unnamed.as_str(),
        Some("cw_a"),
        said("nameless"),
        base + 5,
    )
    .await;
    seed_run(
        &h.store,
        &me.id,
        monitor.as_str(),
        Some("cw_a"),
        said("a run failed; find out why"),
        base + 6,
    )
    .await;
    seed_run(
        &h.store,
        &me.id,
        theirs.as_str(),
        Some("cw_a"),
        said("my own words"),
        base + 7,
    )
    .await;

    let only_b = h.thread_ids(&me, "?coworkerId=cw_b").await;
    assert_eq!(only_b, vec!["chat-b"]);
    let only_a = h.thread_ids(&me, "?coworkerId=cw_a").await;
    assert_eq!(only_a.len(), 6, "{only_a:?}");
    assert!(!only_a.contains(&"chat-b".to_string()), "{only_a:?}");
    assert!(
        h.list(&me, "?coworkerId=cw_nobody").await.is_empty(),
        "a coworker with no conversations is an empty list"
    );

    let rows = h.list(&me, "").await;
    let chat = row(&rows, "chat-a");
    assert_eq!(chat["origin"], "chat", "{chat}");
    assert_eq!(chat["coworkerId"], "cw_a", "{chat}");
    assert_eq!(chat["title"], "to a", "{chat}");

    let listed = row(&rows, cron.as_str());
    assert_eq!(listed["origin"], "schedule", "{listed}");
    assert_eq!(
        listed["title"], "Morning digest",
        "the routine's name: {listed}"
    );

    let listed = row(&rows, hook.as_str());
    assert_eq!(listed["origin"], "webhook", "{listed}");
    assert_eq!(listed["title"], "Deploy hook", "{listed}");

    let listed = row(&rows, unnamed.as_str());
    assert_eq!(listed["origin"], "schedule", "{listed}");
    assert_eq!(
        listed["title"], "nameless",
        "an unnamed routine falls back to the first words: {listed}"
    );

    let listed = row(&rows, monitor.as_str());
    assert_eq!(listed["origin"], "monitor", "{listed}");
    assert_eq!(listed["title"], "a run failed; find out why", "{listed}");

    let listed = row(&rows, theirs.as_str());
    assert_eq!(
        listed["origin"], "chat",
        "somebody else's routine id is only a thread name here: {listed}"
    );
    assert_eq!(
        listed["title"], "my own words",
        "and it lends the thread none of its name: {listed}"
    );
}

/// What is listed and what is not: the MCP door's audit thread for a coworker is a log of calls,
/// not a conversation, so it is left out — exactly, never by prefix, so a chat somebody named
/// `mcp-…` is still theirs to see. A run that never started and a turn with no coworker are
/// conversations the person had, and are listed. The title is the first line of the first thing
/// the person said.
#[tokio::test]
async fn the_mcp_audit_thread_is_left_out_and_every_real_conversation_is_listed() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let me = h.person("Gus", None).await;
    let base = now_ms();

    seed_run(
        &h.store,
        &me.id,
        "mcp-cw_a",
        Some("cw_a"),
        Some(Vec::new()),
        base + 1,
    )
    .await;
    seed_run(
        &h.store,
        &me.id,
        "mcp-other",
        Some("cw_a"),
        said("named like one"),
        base + 2,
    )
    .await;
    seed_run_that_never_started(&h.store, &me.id, "never", base + 3).await;
    seed_run(
        &h.store,
        &me.id,
        "alone",
        None,
        said("no coworker"),
        base + 4,
    )
    .await;
    seed_run(
        &h.store,
        &me.id,
        "lines",
        None,
        Some(vec![
            json!({ "id": unique("m"), "role": "developer", "content": "not the person" }),
            json!({ "id": unique("m"), "role": "user", "content": "" }),
            json!({ "id": unique("m"), "role": "user", "content": "  Plan the offsite  \nwith details" }),
        ]),
        base + 5,
    )
    .await;
    let long = "x".repeat(300);
    seed_run(&h.store, &me.id, "long", None, said(&long), base + 6).await;

    let rows = h.list(&me, "").await;
    let ids: Vec<&str> = rows
        .iter()
        .map(|row| row["threadId"].as_str().expect("threadId"))
        .collect();
    assert!(
        !ids.contains(&"mcp-cw_a"),
        "the coworker's own MCP audit thread is not a conversation: {ids:?}"
    );
    assert_eq!(ids, vec!["long", "lines", "alone", "never", "mcp-other"]);

    let never = row(&rows, "never");
    assert_eq!(never["coworkerId"], Value::Null, "{never}");
    assert_eq!(never["title"], Value::Null, "{never}");
    assert_eq!(never["origin"], "chat", "{never}");
    assert_eq!(never["lastStatus"], "running", "{never}");

    let alone = row(&rows, "alone");
    assert_eq!(alone["coworkerId"], Value::Null, "{alone}");
    assert_eq!(alone["title"], "no coworker", "{alone}");

    assert_eq!(row(&rows, "mcp-other")["coworkerId"], "cw_a");
    assert_eq!(
        row(&rows, "lines")["title"],
        "Plan the offsite",
        "the first line the person wrote, trimmed"
    );
    assert_eq!(
        row(&rows, "long")["title"]
            .as_str()
            .map(|t| t.chars().count()),
        Some(200),
        "a title is capped"
    );
}

/// Who may ask: a signed-in person, and only as themselves. No bearer and a bot key are both a
/// 401 with an `{error}` a client can show; a person with no history gets `[]`, never a 404.
#[tokio::test]
async fn only_a_signed_in_person_may_list_and_an_empty_history_is_an_empty_array() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let me = h.person("Hal", None).await;

    let (status, body) = h
        .send_with(None, reqwest::Method::GET, "/ag-ui/threads", None)
        .await;
    assert_eq!(status, 401, "{body}");
    assert!(body["error"].is_string(), "a 401 says why: {body}");

    let (status, body) = h
        .send_with(
            Some("not-a-token"),
            reqwest::Method::GET,
            "/ag-ui/threads",
            None,
        )
        .await;
    assert_eq!(status, 401, "{body}");

    let ada = h.hire(&me, "Ada").await;
    let (status, minted) = h
        .send(
            &me,
            reqwest::Method::POST,
            &format!("/coworkers/{ada}/keys"),
            None,
        )
        .await;
    assert_eq!(status, 201, "{minted}");
    let key = minted["key"].as_str().expect("key");
    let (status, body) = h
        .send_with(Some(key), reqwest::Method::GET, "/ag-ui/threads", None)
        .await;
    assert_eq!(
        status, 401,
        "a bot key speaks for a coworker, not for the person's history: {body}"
    );
    assert!(body["error"].is_string(), "{body}");

    // A malformed query is still asked who is calling first, and its refusal is JSON too.
    let (status, body) = h
        .send_with(None, reqwest::Method::GET, "/ag-ui/threads?limit=abc", None)
        .await;
    assert_eq!(status, 401, "signed out outranks a bad limit: {body}");
    let (status, body) = h
        .send(&me, reqwest::Method::GET, "/ag-ui/threads?before=x", None)
        .await;
    assert_eq!(status, 400, "{body}");
    assert!(body["error"].is_string(), "a 400 says why, as JSON: {body}");

    let (status, body) = h
        .send(&me, reqwest::Method::GET, "/ag-ui/threads", None)
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!([]), "no history is an empty array");
}
