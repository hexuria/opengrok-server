//! A retry of a queued send's reply (#300): NativeChat's "Try again" and "Send this reply on
//! Server" re-POST the reply's bubble, and the queue had already handed that bubble to a run.
//!
//! `forwardedProps.retryOf` names the run whose reply is retried. When that run consumed the
//! bubble's queued send, is the caller's, on this thread, and has ended, the send is taken again
//! by the new run, which answers it afresh on the door the retry names: no new queue entry, and no
//! second copy of the message in the thread, as a retry of a send never queued. Any other `retryOf`
//! (a run still going, another run) and no `retryOf` at all keep the 409 `already-consumed` that
//! stops a queued send firing twice. Needs Postgres; skips loudly without `OG_DATABASE_URL`.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, RunId};
use opengrok_core::run::{RunCommand, RunEvent, RunStatus, RunView};
use opengrok_harness::{DeltaStream, GatewayDoor, MockDoor, ModelDoor, ModelError, ModelRequest};
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::password::hash_password;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::host_state::HostState;
use opengrok_store::{DrainKey, DrainResult, PgStore};
use reqwest::Method;
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

/// The bubble every turn here sends, and the words in it.
const BUBBLE: &str = "bubble-queued";
const WORDS: &str = "summarise the report I sent this morning";

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn stamp() -> String {
    uuid::Uuid::now_v7().simple().to_string()
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

/// Append `command`'s events to the run `id`, as `account`'s.
async fn append(store: &PgStore, account: &AccountId, id: &RunId, command: RunCommand) {
    let (mut run, seq) = store.load_run(id).await.expect("load run");
    let events: Vec<RunEvent> = run.decide(command).expect("decide");
    for event in &events {
        run.apply(event);
    }
    let view = RunView {
        id: id.clone(),
        thread_id: run.thread_id.clone(),
        status: run.status,
        event_count: run.emitted.len() as i64,
        updated_at_ms: now_ms(),
    };
    store
        .append_run(id, seq, &events, &view, Some(account))
        .await
        .expect("append run");
}

/// A run of `account`'s on `thread`, started and still going. Its prompt is journaled, empty, so
/// the thread's history is told from its log as any turn since #6 has it.
async fn running(store: &PgStore, account: &AccountId, thread: &str) -> RunId {
    let id = RunId::new();
    let start = RunCommand::Start {
        thread_id: thread.to_string(),
        coworker_id: None,
        model: None,
        effort: Default::default(),
        inference_source: Default::default(),
        system: None,
        skill_id: None,
        offered_skills: Vec::new(),
        prompt: Some(Vec::new()),
        limits: Default::default(),
        at_ms: now_ms(),
    };
    append(store, account, &id, start).await;
    id
}

async fn finished(store: &PgStore, account: &AccountId, thread: &str) -> RunId {
    let id = running(store, account, thread).await;
    let finish = RunCommand::Finish {
        at_ms: now_ms(),
        reason: None,
    };
    append(store, account, &id, finish).await;
    id
}

async fn fail(store: &PgStore, account: &AccountId, id: &RunId) {
    let fail = RunCommand::Fail {
        reason: "the door went away".to_string(),
        at_ms: now_ms(),
    };
    append(store, account, id, fail).await;
}

/// The gateway is the echoing door; the person's own door is the one the server ships, which
/// refuses a plan with no proxy set in its own words (`plan_unavailable`) before calling anyone.
/// Every request is kept: which door a turn asked, and what it was asked.
struct Doors {
    asked: Mutex<Vec<ModelRequest>>,
    own: GatewayDoor,
}

#[async_trait::async_trait]
impl ModelDoor for Doors {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
        self.asked.lock().unwrap().push(request.clone());
        match request.endpoint {
            None => MockDoor::echoing().stream(request).await,
            Some(_) => self.own.stream(request).await,
        }
    }
}

struct Harness {
    doors: Arc<Doors>,
    base: String,
    client: reqwest::Client,
    store: PgStore,
    minter: Arc<TokenMinter>,
}

async fn harness(database_url: &str, email: &str) -> Harness {
    // Room for a test-held row lock, two turns waiting on it, and the poll that watches them.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(10)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let minter = Arc::new(TokenMinter::new(b"retried-reply-secret"));
    let auth = AuthState::new(store.clone(), minter.clone(), email.to_string());
    let doors = Arc::new(Doors {
        asked: Mutex::new(Vec::new()),
        // Never dialled: a plan with no proxy is refused before any request leaves.
        own: GatewayDoor::new("http://127.0.0.1:9", "unused"),
    });
    let agui = AgUiState {
        auth,
        door: doors.clone(),
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
    let app = opengrok_server::router(agui, gateway);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    Harness {
        doors,
        base: format!("http://127.0.0.1:{}", addr.port()),
        client: reqwest::Client::new(),
        store,
        minter,
    }
}

/// One person, and a thread they have run a turn on (the queue's routes need one).
struct Person {
    id: AccountId,
    access: String,
    thread: String,
    earlier: RunId,
}

impl Harness {
    async fn person(&self, name: &str) -> Person {
        let email = format!("{name}-{}@og.local", stamp());
        let id = seed_account(&self.store, &email).await;
        let now = chrono::Utc::now().timestamp();
        let access = self
            .minter
            .mint_access(id.as_str(), "sess-retry", &email, "ultra", now, 3600)
            .expect("mint access");
        let thread = format!("th-{name}-{}", stamp());
        let earlier = finished(&self.store, &id, &thread).await;
        Person {
            id,
            access,
            thread,
            earlier,
        }
    }

    async fn rest(
        &self,
        method: Method,
        who: &Person,
        path: &str,
        body: Option<&Value>,
    ) -> (u16, Value) {
        let mut req = self.client.request(method, format!("{}{path}", self.base));
        req = req.header("Authorization", format!("Bearer {}", who.access));
        if let Some(body) = body {
            req = req.json(body);
        }
        let res = req.send().await.expect("request");
        let status = res.status().as_u16();
        let text = res.text().await.expect("body");
        (status, serde_json::from_str(&text).unwrap_or(json!(text)))
    }

    /// The bubble queued on `who`'s thread, picking `props` (its own door, say).
    async fn queue(&self, who: &Person, bubble: &str, props: Value) -> String {
        let mut body = json!({ "content": WORDS, "clientMessageId": bubble });
        body.as_object_mut()
            .unwrap()
            .extend(props.as_object().cloned().unwrap());
        let path = format!("/ag-ui/threads/{}/pending", who.thread);
        let (status, created) = self.rest(Method::POST, who, &path, Some(&body)).await;
        assert_eq!(status, 201, "{created}");
        created["pendingUserMessage"]["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// One turn sending `bubble` as the run `run`: its status, and its frames when it streamed or
    /// its body when it did not.
    async fn turn(
        &self,
        who: &Person,
        bubble: &str,
        run: &str,
        props: Value,
    ) -> (u16, Vec<Value>, Value) {
        let res = self
            .client
            .post(format!("{}/ag-ui", self.base))
            .header("Authorization", format!("Bearer {}", who.access))
            .json(&json!({
                "threadId": who.thread, "runId": run,
                "messages": [{ "id": bubble, "role": "user", "content": WORDS }],
                "forwardedProps": props,
            }))
            .send()
            .await
            .expect("turn");
        let status = res.status().as_u16();
        let text = res.text().await.expect("body");
        let frames = text
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str(data).ok())
            .collect();
        (
            status,
            frames,
            serde_json::from_str(&text).unwrap_or(Value::Null),
        )
    }

    /// A queued send on the person's own door with no proxy set, fired as its run: the door
    /// refuses it in words and the run fails. That run's reply is the one "Try again" is on.
    async fn a_failed_reply(&self, who: &Person) -> (String, String) {
        let id = self
            .queue(who, BUBBLE, json!({ "inferenceSource": "local_proxy" }))
            .await;
        let run = format!("run-queued-{}", stamp());
        let (status, frames, _) = self
            .turn(who, BUBBLE, &run, json!({ "pendingId": id }))
            .await;
        assert_eq!(status, 200, "{frames:?}");
        let ending = frames.last().cloned().unwrap_or_default();
        assert_eq!(ending["type"], "RUN_ERROR", "{frames:?}");
        assert_eq!(ending["code"], "plan_unavailable", "{ending}");
        assert_eq!(self.status(&run).await, Some(RunStatus::Failed));
        (id, run)
    }

    async fn status(&self, run: &str) -> Option<RunStatus> {
        let id = RunId::from_stored(run.to_string());
        self.store.run_status(&id).await.expect("run status")
    }

    /// The run the queued send `id` is consumed by now.
    async fn drained_by(&self, who: &Person, id: &str) -> String {
        let row = self.store.pending_user_message(id, &who.id).await.unwrap();
        let row = row.expect("the queued send's row");
        assert_eq!(row.status, "drained", "{row:?}");
        row.drained_run_id.expect("drained by a run")
    }

    /// How many requests reached the gateway, and the person's own door.
    fn asked(&self) -> (usize, usize) {
        let asked = self.doors.asked.lock().unwrap();
        let gateway = asked.iter().filter(|r| r.endpoint.is_none()).count();
        (gateway, asked.len() - gateway)
    }
}

/// The 409 a turn that may not fire a consumed send is answered with, naming the run that holds it.
fn already_consumed(body: &Value, run: &str) {
    assert_eq!(body["v"], 1, "{body}");
    assert_eq!(body["error"], "already-consumed", "{body}");
    assert_eq!(body["runId"], run, "{body}");
    assert_eq!(body["event"]["value"]["op"], "drained", "{body}");
}

fn said(frames: &[Value]) -> String {
    frames
        .iter()
        .filter(|frame| frame["type"] == "TEXT_MESSAGE_CONTENT")
        .filter_map(|frame| frame["delta"].as_str())
        .collect()
}

/// Sessions waiting on the row lock `holder` took, directly or behind another waiter.
async fn waiting_behind(pool: &sqlx::PgPool, holder: i32) -> i64 {
    sqlx::query_scalar(
        "select count(*) from pg_stat_activity a
          where $1 = any(pg_blocking_pids(a.pid))
             or exists (select 1 from unnest(pg_blocking_pids(a.pid)) b(pid)
                         where $1 = any(pg_blocking_pids(b.pid)))",
    )
    .bind(holder)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// THE RETRY RUNS AFRESH, ON THE DOOR IT NAMES, AND THE THREAD HOLDS THE MESSAGE ONCE. The send
/// was queued on the person's own door with no proxy set, so its run failed; "Send this reply on
/// Server" retries it on the gateway. The queue gains nothing, the row now names the retry, and
/// the model is asked the message once, from the run that first carried it.
#[tokio::test]
async fn a_retry_of_a_queued_sends_failed_reply_runs_afresh_on_the_door_it_names() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, "retry-runs@og.local").await;
    let me = h.person("retry-runs").await;
    let (id, failed) = h.a_failed_reply(&me).await;
    assert_eq!(h.asked(), (0, 1), "the queued send asked its own door once");

    let retry = format!("run-retry-{}", stamp());
    let props = json!({ "retryOf": failed, "inferenceSource": "gateway" });
    let (status, frames, _) = h.turn(&me, BUBBLE, &retry, props).await;
    assert_eq!(status, 200, "{frames:?}");
    let source = frames
        .iter()
        .find(|f| f["type"] == "CUSTOM" && f["name"] == "opengrok.inferenceSource");
    assert_eq!(source.unwrap()["value"]["kind"], "gateway", "{frames:?}");
    let ending = frames.last().cloned().unwrap_or_default();
    assert_eq!(ending["type"], "RUN_FINISHED", "{frames:?}");
    assert!(
        said(&frames).starts_with(&format!("You said: {WORDS}.")),
        "{frames:?}"
    );
    assert_eq!(
        h.asked(),
        (1, 1),
        "the retry asked the gateway, and only it"
    );
    let asked = h.doors.asked.lock().unwrap().last().cloned().unwrap();
    let users: Vec<_> = asked.messages.iter().filter(|m| m.role == "user").collect();
    assert_eq!(
        users.len(),
        1,
        "the model hears the message once: {users:?}"
    );
    assert_eq!(users[0].content, WORDS);

    // No new queue entry: the one row there ever was now names the retry.
    let path = format!("/ag-ui/threads/{}/pending", me.thread);
    let (status, queue) = h.rest(Method::GET, &me, &path, None).await;
    assert_eq!(status, 200, "{queue}");
    assert_eq!(queue["pendingUserMessages"], json!([]), "{queue}");
    let rows: i64 = sqlx::query_scalar(
        "select count(*) from pending_user_message where account_id = $1 and thread_id = $2",
    )
    .bind(me.id.as_str())
    .bind(&me.thread)
    .fetch_one(h.store.pool())
    .await
    .unwrap();
    assert_eq!(rows, 1);
    assert_eq!(h.drained_by(&me, &id).await, retry);

    // The replay draws the person's message once, under the run that first carried it.
    let path = format!("/ag-ui/threads/{}", me.thread);
    let (status, replay) = h.rest(Method::GET, &me, &path, None).await;
    assert_eq!(status, 200, "{replay}");
    let runs = replay["runs"].as_array().cloned().unwrap_or_default();
    let ids: Vec<_> = runs.iter().map(|run| run["runId"].clone()).collect();
    let earlier = me.earlier.as_str();
    assert_eq!(
        ids,
        [json!(earlier), json!(failed), json!(retry)],
        "{replay}"
    );
    assert_eq!(runs[1]["status"], "failed");
    assert_eq!(runs[2]["status"], "finished");
    let bubbles = |run: &Value| {
        let events = run["events"].as_array().cloned().unwrap_or_default();
        let starts = events
            .into_iter()
            .filter(|e| e["type"] == "TEXT_MESSAGE_START");
        starts
            .filter(|e| e["role"] == "user" && e["messageId"] == BUBBLE)
            .count()
    };
    let drawn: Vec<usize> = runs.iter().map(bubbles).collect();
    assert_eq!(drawn, [0, 1, 0], "{replay}");
}

/// No `retryOf`, no retry: the same POST is the double fire the 409 exists to stop.
#[tokio::test]
async fn a_retry_that_names_no_run_is_still_already_consumed() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, "retry-none@og.local").await;
    let me = h.person("retry-none").await;
    let (id, failed) = h.a_failed_reply(&me).await;
    let again = format!("run-again-{}", stamp());
    let props = json!({ "inferenceSource": "gateway" });
    let (status, _, body) = h.turn(&me, BUBBLE, &again, props).await;
    assert_eq!(status, 409, "{body}");
    already_consumed(&body, &failed);
    assert_eq!(h.drained_by(&me, &id).await, failed);
    assert_eq!(h.asked(), (0, 1), "nothing reached a model");
}

/// A REPLY STILL BEING WRITTEN IS NOT RETRIED: its run may yet answer, and a second turn beside it
/// would fire the send twice. Once that run has ended, the same retry runs.
#[tokio::test]
async fn a_retry_of_a_reply_still_running_is_already_consumed() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, "retry-running@og.local").await;
    let me = h.person("retry-running").await;
    let id = h.queue(&me, BUBBLE, json!({})).await;
    let going = running(&h.store, &me.id, &me.thread).await;
    let claim = (going.as_str(), None);
    let drained = h.store.drain_pending_user_message(
        DrainKey::Id(&id),
        &me.id,
        &me.thread,
        claim,
        now_ms(),
        |_| true,
    );
    assert!(matches!(drained.await.unwrap(), DrainResult::Drained(_)));

    let props = json!({ "retryOf": going.as_str(), "inferenceSource": "gateway" });
    let early = format!("run-early-{}", stamp());
    let (status, _, body) = h.turn(&me, BUBBLE, &early, props.clone()).await;
    assert_eq!(status, 409, "{body}");
    already_consumed(&body, going.as_str());
    assert_eq!(h.drained_by(&me, &id).await, going.as_str());
    assert_eq!(h.asked(), (0, 0), "nothing reached a model");

    fail(&h.store, &me.id, &going).await;
    let retry = format!("run-retry-{}", stamp());
    let (status, frames, _) = h.turn(&me, BUBBLE, &retry, props).await;
    assert_eq!(status, 200, "{frames:?}");
    assert_eq!(h.drained_by(&me, &id).await, retry);
    assert_eq!(h.asked(), (1, 0));
}

/// `retryOf` must name the run the send went to: an earlier turn on the thread, ended and the
/// person's own, is still another run.
#[tokio::test]
async fn a_retry_naming_another_run_is_already_consumed() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, "retry-other@og.local").await;
    let me = h.person("retry-other").await;
    let (id, failed) = h.a_failed_reply(&me).await;
    let props = json!({ "retryOf": me.earlier.as_str(), "inferenceSource": "gateway" });
    let elsewhere = format!("run-elsewhere-{}", stamp());
    let (status, _, body) = h.turn(&me, BUBBLE, &elsewhere, props).await;
    assert_eq!(status, 409, "{body}");
    already_consumed(&body, &failed);
    assert_eq!(h.drained_by(&me, &id).await, failed);
    assert_eq!(h.asked(), (0, 1), "nothing reached the gateway");
}

/// ONE RETRY PER ENDED RUN. Two "Try again"s on one reply (two machines, a double click) reach the
/// row together: one takes the send, and the other is told which run did. That run's own reply can
/// be retried in turn once it ends, as a reply that was never queued can be retried again.
#[tokio::test]
async fn two_retries_of_one_reply_run_once_and_the_retry_can_be_retried() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, "retry-twice@og.local").await;
    let me = h.person("retry-twice").await;
    let (id, failed) = h.a_failed_reply(&me).await;
    let mut lock = h.store.pool().begin().await.unwrap();
    let holder: i32 = sqlx::query_scalar("select pg_backend_pid()")
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    sqlx::query("select id from pending_user_message where id = $1 for update")
        .bind(&id)
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    let props = json!({ "retryOf": failed, "inferenceSource": "gateway" });
    let (one, two) = (
        format!("run-one-{}", stamp()),
        format!("run-two-{}", stamp()),
    );
    let release = async {
        let waited = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while waiting_behind(h.store.pool(), holder).await < 2 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        });
        waited
            .await
            .expect("both retries must reach the locked row");
        lock.commit().await.unwrap();
    };
    let (first, second, ()) = tokio::join!(
        h.turn(&me, BUBBLE, &one, props.clone()),
        h.turn(&me, BUBBLE, &two, props),
        release,
    );
    let (ran, refused) = match (first.0, second.0) {
        (200, 409) => ((one, first), second),
        (409, 200) => ((two, second), first),
        statuses => panic!("one retry runs and one is refused: {statuses:?}"),
    };
    let (winner, (_, frames, _)) = ran;
    assert_eq!(frames.last().unwrap()["type"], "RUN_FINISHED", "{frames:?}");
    already_consumed(&refused.2, &winner);
    assert_eq!(h.drained_by(&me, &id).await, winner);
    assert_eq!(h.asked(), (1, 1), "the gateway was asked once");

    let props = json!({ "retryOf": winner, "inferenceSource": "gateway" });
    let third = format!("run-three-{}", stamp());
    let (status, frames, _) = h.turn(&me, BUBBLE, &third, props).await;
    assert_eq!(status, 200, "{frames:?}");
    assert_eq!(h.drained_by(&me, &id).await, third);
    assert_eq!(h.asked(), (2, 1));
}

/// THE RUN A RETRY TAKES A SEND FROM IS THE CALLER'S, ON THIS THREAD. A send held by another
/// account's run, or by a run of the person's on another thread (a client that reused a run id
/// leaves either), is not retried; and another account naming the person's run reaches only a
/// queue of its own, so the person's send stays with the run it went to.
#[tokio::test]
async fn a_retry_of_another_accounts_run_or_another_threads_is_already_consumed() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, "retry-theirs@og.local").await;
    let me = h.person("retry-theirs").await;
    let stranger = h.person("retry-stranger").await;
    let props = |run: &str| json!({ "retryOf": run, "inferenceSource": "gateway" });
    let elsewhere = format!("th-elsewhere-{}", stamp());
    let cases = [
        ("bubble-theirs", &stranger.id, me.thread.as_str()),
        ("bubble-elsewhere", &me.id, elsewhere.as_str()),
    ];
    for (bubble, owner, thread) in cases {
        let held_by = running(&h.store, owner, thread).await;
        fail(&h.store, owner, &held_by).await;
        let id = h.queue(&me, bubble, json!({})).await;
        let claim = (held_by.as_str(), None);
        let drained = h.store.drain_pending_user_message(
            DrainKey::Id(&id),
            &me.id,
            &me.thread,
            claim,
            now_ms(),
            |_| true,
        );
        assert!(matches!(drained.await.unwrap(), DrainResult::Drained(_)));
        let retry = format!("run-retry-{}", stamp());
        let (status, _, body) = h.turn(&me, bubble, &retry, props(held_by.as_str())).await;
        assert_eq!(status, 409, "{bubble}: {body}");
        already_consumed(&body, held_by.as_str());
        assert_eq!(h.drained_by(&me, &id).await, held_by.as_str());
    }
    assert_eq!(h.asked(), (0, 0), "nothing reached a model");

    // Whatever the stranger's POST comes to, it is a turn of their own and never takes this send.
    let (id, failed) = h.a_failed_reply(&me).await;
    let on_my_thread = Person {
        thread: me.thread.clone(),
        ..stranger
    };
    let theirs = format!("run-theirs-{}", stamp());
    h.turn(&on_my_thread, BUBBLE, &theirs, props(&failed)).await;
    assert_eq!(h.drained_by(&me, &id).await, failed);
}
