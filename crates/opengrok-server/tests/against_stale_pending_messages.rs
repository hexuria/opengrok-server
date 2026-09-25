//! A queued send fires only as the text and options the server holds for it.
//!
//! An edit on one machine and a send from another's stale copy reach the same row. The send is
//! compared and claimed under the row lock, so the person's edited words are the ones the model
//! hears, or the send is refused and the row stays queued. Needs Postgres; skips loudly without
//! `OG_DATABASE_URL`.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, RunId};
use opengrok_core::run::{Run, RunCommand, RunEvent, RunView};
use opengrok_harness::{DeltaStream, MockDoor, ModelDoor, ModelError, ModelRequest};
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

fn record(run: &mut Run, log: &mut Vec<RunEvent>, produced: Vec<RunEvent>) {
    for event in &produced {
        run.apply(event);
    }
    log.extend(produced);
}

async fn seed_run(store: &PgStore, account: &AccountId, thread: &str, at_ms: i64) -> RunId {
    let id = RunId::new();
    let mut run = Run::default();
    let mut log = Vec::new();
    let produced = run
        .decide(RunCommand::Start {
            thread_id: thread.to_string(),
            coworker_id: None,
            model: None,
            system: None,
            skill_id: None,
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

#[derive(Default)]
struct RecordingDoor(Mutex<Vec<ModelRequest>>);
#[async_trait::async_trait]
impl ModelDoor for RecordingDoor {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
        self.0.lock().unwrap().push(request.clone());
        MockDoor::echoing().stream(request).await
    }
}

struct Harness {
    door: Arc<RecordingDoor>,
    base: String,
    client: reqwest::Client,
    store: PgStore,
    minter: Arc<TokenMinter>,
}

async fn harness(database_url: &str, email: &str) -> Harness {
    // Room for a test-held row lock, two blocked writers, and the poll that watches them.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let minter = Arc::new(TokenMinter::new(b"pending-user-message-secret"));
    let auth = AuthState::new(store.clone(), minter.clone(), email.to_string());
    let door = Arc::new(RecordingDoor::default());
    let agui = AgUiState {
        auth,
        door: door.clone(),
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
        door,
        base: format!("http://127.0.0.1:{}", addr.port()),
        client: reqwest::Client::new(),
        store,
        minter,
    }
}

impl Harness {
    async fn person(&self, email: &str) -> (AccountId, String) {
        let account = seed_account(&self.store, email).await;
        let access = self
            .minter
            .mint_access(
                account.as_str(),
                "sess-pending",
                email,
                "ultra",
                chrono::Utc::now().timestamp(),
                3600,
            )
            .expect("mint access");
        (account, access)
    }

    async fn pending(
        &self,
        method: reqwest::Method,
        access: Option<&str>,
        path: &str,
        body: Option<&Value>,
    ) -> (u16, Value) {
        let mut req = self.client.request(method, format!("{}{path}", self.base));
        if let Some(access) = access {
            req = req.header("Authorization", format!("Bearer {access}"));
        }
        if let Some(body) = body {
            req = req.json(body);
        }
        let res = req.send().await.expect("request");
        let status = res.status().as_u16();
        let text = res.text().await.expect("body");
        let json = serde_json::from_str(&text).unwrap_or(json!(text));
        (status, json)
    }
}

fn stamp() -> String {
    uuid::Uuid::now_v7().simple().to_string()
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

async fn until_waiting(pool: &sqlx::PgPool, holder: i32, count: i64) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while waiting_behind(pool, holder).await < count {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the writers must queue on the locked row");
}

/// Take the row so the test decides which writer reaches it first.
async fn hold_row(store: &PgStore, id: &str) -> (sqlx::Transaction<'static, sqlx::Postgres>, i32) {
    let mut lock = store.pool().begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("select pg_backend_pid()")
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    sqlx::query("select id from pending_user_message where id = $1 for update")
        .bind(id)
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    (lock, pid)
}

fn last_user_text(request: &ModelRequest) -> String {
    request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == "user")
        .map(|message| message.content.clone())
        .unwrap_or_default()
}

#[tokio::test]
async fn stale_send_refuses_without_consuming_or_calling_the_model() {
    let database_url = database_or_skip!();
    let stamp = stamp();
    let h = harness(&database_url, &format!("stale-{stamp}@og.local")).await;
    let (account, access) = h.person(&format!("stale-{stamp}@og.local")).await;
    let thread = format!("stale-{stamp}");
    seed_run(&h.store, &account, &thread, now_ms()).await;
    let path = format!("/ag-ui/threads/{thread}/pending");
    let (_, created) = h
        .pending(
            reqwest::Method::POST,
            Some(&access),
            &path,
            Some(&json!({"content":"old", "clientMessageId":"bubble"})),
        )
        .await;
    let id = created["pendingUserMessage"]["id"].as_str().unwrap();
    let (status, _) = h
        .pending(
            reqwest::Method::PATCH,
            Some(&access),
            &format!("{path}/{id}"),
            Some(&json!({"content":"edited"})),
        )
        .await;
    assert_eq!(status, 200);
    for explicit in [true, false] {
        let props = if explicit {
            json!({"pendingId":id})
        } else {
            json!({})
        };
        let (status, body) = h.pending(reqwest::Method::POST, Some(&access), "/ag-ui", Some(&json!({
            "threadId":thread,"runId":format!("stale-run-{stamp}-{explicit}"),
            "messages":[{"id":"bubble","role":"user","content":"old"}],"forwardedProps":props
        }))).await;
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["error"], "stale-pending-message");
        assert_eq!(body["event"]["value"]["message"]["content"], "edited");
        assert_eq!(
            h.store
                .pending_user_message(id, &account)
                .await
                .unwrap()
                .unwrap()
                .status,
            "pending"
        );
        assert!(h.door.0.lock().unwrap().is_empty());
    }
    let (status, _) = h.pending(reqwest::Method::POST, Some(&access), "/ag-ui", Some(&json!({
        "threadId":thread,"runId":format!("fresh-run-{stamp}"),
        "messages":[{"id":"bubble","role":"user","content":"edited"}],"forwardedProps":{"pendingId":id}
    }))).await;
    assert_eq!(status, 200);
    assert_eq!(h.door.0.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn stale_options_and_changed_same_run_retries_are_refused() {
    let database_url = database_or_skip!();
    let stamp = stamp();
    let h = harness(&database_url, &format!("stale-options-{stamp}@og.local")).await;
    let (account, access) = h.person(&format!("stale-options-{stamp}@og.local")).await;
    let thread = format!("stale-options-{stamp}");
    seed_run(&h.store, &account, &thread, now_ms()).await;
    let path = format!("/ag-ui/threads/{thread}/pending");
    let (_, created) = h
        .pending(
            reqwest::Method::POST,
            Some(&access),
            &path,
            Some(&json!({"content":"current", "clientMessageId":"bubble",
                "recipeId":"rec_current", "recipeValues":{"x":"new"}, "skillId":"skl_current"})),
        )
        .await;
    let id = created["pendingUserMessage"]["id"].as_str().unwrap();
    let run = format!("options-run-{stamp}");
    let base = json!({"threadId":thread,"runId":run,
        "messages":[{"id":"bubble","role":"user","content":"current"}],
        "forwardedProps":{"pendingId":id, "recipe":"rec_current", "recipeValues":{"x":"new"},
            "skill":"skl_current"}});
    for (key, value) in [
        ("recipe", json!("old-recipe")),
        ("recipeValues", json!({"x":"old"})),
        ("skill", json!("old-skill")),
        ("replyTo", json!({"preview":"old"})),
    ] {
        let mut body = base.clone();
        if key == "replyTo" {
            body["messages"][0][key] = value;
        } else {
            body["forwardedProps"][key] = value;
        }
        let (status, result) = h
            .pending(reqwest::Method::POST, Some(&access), "/ag-ui", Some(&body))
            .await;
        assert_eq!(status, 409, "{key}: {result}");
        assert_eq!(result["error"], "stale-pending-message", "{key}: {result}");
        assert_eq!(result["event"]["value"]["op"], "edited", "{key}: {result}");
    }
    assert!(h.door.0.lock().unwrap().is_empty());
    let mut absent = base.clone();
    absent["messages"] = json!([]);
    let (status, _) = h
        .pending(
            reqwest::Method::POST,
            Some(&access),
            "/ag-ui",
            Some(&absent),
        )
        .await;
    assert_eq!(status, 400);
    assert_eq!(
        h.store
            .pending_user_message(id, &account)
            .await
            .unwrap()
            .unwrap()
            .status,
        "pending"
    );
    let (status, _) = h
        .pending(reqwest::Method::POST, Some(&access), "/ag-ui", Some(&base))
        .await;
    assert_eq!(status, 200);
    let mut changed = base.clone();
    changed["messages"][0]["content"] = json!("retry different text");
    let (status, retried) = h
        .pending(
            reqwest::Method::POST,
            Some(&access),
            "/ag-ui",
            Some(&changed),
        )
        .await;
    assert_eq!(status, 409, "{retried}");
    assert_eq!(retried["error"], "stale-pending-message");
    assert_eq!(retried["runId"], run.as_str());
    assert_eq!(retried["event"]["value"]["op"], "drained");
    assert_eq!(h.door.0.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_legitimate_send_matches_however_its_options_are_spelled() {
    let database_url = database_or_skip!();
    let stamp = stamp();
    let h = harness(&database_url, &format!("spelled-{stamp}@og.local")).await;
    let (account, access) = h.person(&format!("spelled-{stamp}@og.local")).await;
    let thread = format!("spelled-{stamp}");
    seed_run(&h.store, &account, &thread, now_ms()).await;
    let cases = [
        (
            json!({"recipeId": "rec_1", "recipeValues": {"n": 5, "on": true, "q": "x"},
                   "skillId": "skl_1", "replyTo": "m1"}),
            json!({"recipe": " rec_1 ", "recipeValues": {"n": "5", "on": "true", "q": "x"},
                   "skill": " skl_1 "}),
            Some(json!("m1")),
        ),
        (
            json!({"recipeValues": {"q": "rides along with no recipe"}}),
            json!({"recipe": "", "skill": null}),
            None,
        ),
        (json!({}), json!({"recipe": 7, "skill": ""}), None),
    ];
    for (index, (saved, props, reply)) in cases.into_iter().enumerate() {
        let bubble = format!("bubble-{index}");
        let mut body = saved.clone();
        body["content"] = json!("same words");
        body["clientMessageId"] = json!(bubble);
        let (status, created) = h
            .pending(
                reqwest::Method::POST,
                Some(&access),
                &format!("/ag-ui/threads/{thread}/pending"),
                Some(&body),
            )
            .await;
        assert_eq!(status, 201, "{created}");
        let id = created["pendingUserMessage"]["id"].as_str().unwrap();
        let mut props = props;
        props["pendingUserMessageId"] = json!(id);
        let mut message = json!({"id": bubble, "role": "user", "content": "same words"});
        if let Some(reply) = reply {
            message["replyTo"] = reply;
        }
        let (status, result) = h
            .pending(
                reqwest::Method::POST,
                Some(&access),
                "/ag-ui",
                Some(&json!({
                    "threadId": thread, "runId": format!("spelled-{stamp}-{index}"),
                    "messages": [message], "forwardedProps": props,
                })),
            )
            .await;
        assert_eq!(status, 200, "case {index}: {result}");
        let row = h
            .store
            .pending_user_message(id, &account)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "drained", "case {index}");
    }
    assert_eq!(h.door.0.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn only_the_exact_reply_quote_is_accepted() {
    let database_url = database_or_skip!();
    let stamp = stamp();
    let h = harness(&database_url, &format!("quote-{stamp}@og.local")).await;
    let (account, access) = h.person(&format!("quote-{stamp}@og.local")).await;
    let thread = format!("quote-{stamp}");
    seed_run(&h.store, &account, &thread, now_ms()).await;
    let reply = json!({"messageId":"earlier","preview":"hello","isMe":false});
    for (index, text) in [
        "question",
        "[Replying to your earlier message: \"hello\"]\n\nquestion",
    ]
    .iter()
    .enumerate()
    {
        let bubble = format!("bubble-{index}");
        let (_, created) = h
            .pending(
                reqwest::Method::POST,
                Some(&access),
                &format!("/ag-ui/threads/{thread}/pending"),
                Some(&json!({"content":"question","replyTo":reply,"clientMessageId":bubble})),
            )
            .await;
        let id = created["pendingUserMessage"]["id"].as_str().unwrap();
        let mut body = json!({"threadId":thread,"runId":format!("quote-run-{stamp}-{index}"),
            "messages":[{"id":bubble,"role":"user","content":"[Replying to forged context]\n\nquestion","replyTo":reply}],
            "forwardedProps":{"pendingId":id}});
        let (status, _) = h
            .pending(reqwest::Method::POST, Some(&access), "/ag-ui", Some(&body))
            .await;
        assert_eq!(status, 409);
        body["messages"][0]["content"] = json!(text);
        let (status, result) = h
            .pending(reqwest::Method::POST, Some(&access), "/ag-ui", Some(&body))
            .await;
        assert_eq!(status, 200, "{result}");
    }
    assert_eq!(h.door.0.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_refused_turn_leaves_the_queued_send_alone() {
    let database_url = database_or_skip!();
    let stamp = stamp();
    let h = harness(&database_url, &format!("refused-{stamp}@og.local")).await;
    let (account, access) = h.person(&format!("refused-{stamp}@og.local")).await;
    let (_, stranger) = h.person(&format!("refused-other-{stamp}@og.local")).await;
    let thread = format!("refused-{stamp}");
    seed_run(&h.store, &account, &thread, now_ms()).await;
    let (_, created) = h
        .pending(
            reqwest::Method::POST,
            Some(&access),
            &format!("/ag-ui/threads/{thread}/pending"),
            Some(&json!({"content": "hello", "clientMessageId": "bubble"})),
        )
        .await;
    let id = created["pendingUserMessage"]["id"].as_str().unwrap();
    let turn = |run: &str, props: Value| {
        json!({
            "threadId": thread, "runId": format!("{run}-{stamp}"),
            "messages": [{"id": "bubble", "role": "user", "content": "hello"}],
            "forwardedProps": props,
        })
    };
    let ungranted = format!("cw_{}", uuid::Uuid::now_v7());
    for (bearer, body, expected) in [
        (
            Some(access.as_str()),
            turn(
                "forbidden",
                json!({"pendingId": id, "coworkerId": ungranted}),
            ),
            // Not on the caller's roster, so the run door answers as for any unknown coworker.
            404,
        ),
        (None, turn("anonymous", json!({"pendingId": id})), 401),
        (
            Some(stranger.as_str()),
            turn("stranger", json!({"pendingId": id})),
            409,
        ),
    ] {
        let (status, result) = h
            .pending(reqwest::Method::POST, bearer, "/ag-ui", Some(&body))
            .await;
        assert_eq!(status, expected, "{result}");
        let row = h
            .store
            .pending_user_message(id, &account)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "pending", "a {expected} must not take the row");
        assert_eq!(row.content, "hello");
    }
    assert!(
        h.door.0.lock().unwrap().is_empty(),
        "a refused turn never reaches the model"
    );
    let (status, result) = h
        .pending(
            reqwest::Method::POST,
            Some(&access),
            "/ag-ui",
            Some(&turn("allowed", json!({"pendingId": id}))),
        )
        .await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(h.door.0.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn another_run_still_hears_already_consumed_whatever_it_sends() {
    let database_url = database_or_skip!();
    let stamp = stamp();
    let h = harness(&database_url, &format!("other-run-{stamp}@og.local")).await;
    let (account, access) = h.person(&format!("other-run-{stamp}@og.local")).await;
    let thread = format!("other-run-{stamp}");
    seed_run(&h.store, &account, &thread, now_ms()).await;
    let (_, created) = h
        .pending(
            reqwest::Method::POST,
            Some(&access),
            &format!("/ag-ui/threads/{thread}/pending"),
            Some(&json!({"content": "once", "clientMessageId": "bubble"})),
        )
        .await;
    let id = created["pendingUserMessage"]["id"].as_str().unwrap();
    let turn = |run: &str, text: &str| {
        json!({
            "threadId": thread, "runId": format!("{run}-{stamp}"),
            "messages": [{"id": "bubble", "role": "user", "content": text}],
            "forwardedProps": {"pendingId": id},
        })
    };
    let (status, result) = h
        .pending(
            reqwest::Method::POST,
            Some(&access),
            "/ag-ui",
            Some(&turn("first", "once")),
        )
        .await;
    assert_eq!(status, 200, "{result}");
    for text in ["once", "different"] {
        let (status, result) = h
            .pending(
                reqwest::Method::POST,
                Some(&access),
                "/ag-ui",
                Some(&turn("second", text)),
            )
            .await;
        assert_eq!(status, 409, "{result}");
        assert_eq!(result["error"], "already-consumed", "{text}: {result}");
        assert_eq!(result["runId"], format!("first-{stamp}"));
    }
    assert_eq!(h.door.0.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn an_edit_that_reaches_the_row_first_makes_the_old_send_stale() {
    let database_url = database_or_skip!();
    let stamp = stamp();
    let h = harness(&database_url, &format!("edit-first-{stamp}@og.local")).await;
    let (account, access) = h.person(&format!("edit-first-{stamp}@og.local")).await;
    let thread = format!("edit-first-{stamp}");
    seed_run(&h.store, &account, &thread, now_ms()).await;
    let path = format!("/ag-ui/threads/{thread}/pending");
    let (_, created) = h
        .pending(
            reqwest::Method::POST,
            Some(&access),
            &path,
            Some(&json!({"content": "old", "clientMessageId": "bubble"})),
        )
        .await;
    let id = created["pendingUserMessage"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (lock, holder) = hold_row(&h.store, &id).await;
    let turn = json!({
        "threadId": thread, "runId": format!("race-{stamp}"),
        "messages": [{"id": "bubble", "role": "user", "content": "old"}],
        "forwardedProps": {"pendingId": id},
    });
    let edit_path = format!("{path}/{id}");
    let edited_body = json!({"content": "edited"});
    let edit = h.pending(
        reqwest::Method::PATCH,
        Some(&access),
        &edit_path,
        Some(&edited_body),
    );
    let send = async {
        until_waiting(h.store.pool(), holder, 1).await;
        h.pending(reqwest::Method::POST, Some(&access), "/ag-ui", Some(&turn))
            .await
    };
    let release = async {
        until_waiting(h.store.pool(), holder, 2).await;
        lock.commit().await.unwrap();
    };
    let ((edited, _), (sent, refusal), ()) = tokio::join!(edit, send, release);
    assert_eq!(edited, 200);
    assert_eq!(sent, 409, "{refusal}");
    assert_eq!(refusal["error"], "stale-pending-message");
    assert_eq!(refusal["event"]["value"]["message"]["content"], "edited");
    let row = h
        .store
        .pending_user_message(&id, &account)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (row.status.as_str(), row.content.as_str()),
        ("pending", "edited")
    );
    assert!(
        h.door.0.lock().unwrap().is_empty(),
        "the old text never reached the model"
    );
}

#[tokio::test]
async fn a_send_that_reaches_the_row_first_makes_the_late_edit_miss() {
    let database_url = database_or_skip!();
    let stamp = stamp();
    let h = harness(&database_url, &format!("send-first-{stamp}@og.local")).await;
    let (account, access) = h.person(&format!("send-first-{stamp}@og.local")).await;
    let thread = format!("send-first-{stamp}");
    seed_run(&h.store, &account, &thread, now_ms()).await;
    let path = format!("/ag-ui/threads/{thread}/pending");
    let (_, created) = h
        .pending(
            reqwest::Method::POST,
            Some(&access),
            &path,
            Some(&json!({"content": "old", "clientMessageId": "bubble"})),
        )
        .await;
    let id = created["pendingUserMessage"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (lock, holder) = hold_row(&h.store, &id).await;
    let turn = json!({
        "threadId": thread, "runId": format!("race-{stamp}"),
        "messages": [{"id": "bubble", "role": "user", "content": "old"}],
        "forwardedProps": {"pendingId": id},
    });
    let edit_path = format!("{path}/{id}");
    let edited_body = json!({"content": "edited"});
    let send = h.pending(reqwest::Method::POST, Some(&access), "/ag-ui", Some(&turn));
    let edit = async {
        until_waiting(h.store.pool(), holder, 1).await;
        h.pending(
            reqwest::Method::PATCH,
            Some(&access),
            &edit_path,
            Some(&edited_body),
        )
        .await
    };
    let release = async {
        until_waiting(h.store.pool(), holder, 2).await;
        lock.commit().await.unwrap();
    };
    let ((sent, stream), (edited, missed), ()) = tokio::join!(send, edit, release);
    assert_eq!(sent, 200, "{stream}");
    assert_eq!(edited, 404, "{missed}");
    let row = h
        .store
        .pending_user_message(&id, &account)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (row.status.as_str(), row.content.as_str()),
        ("drained", "old")
    );
    let door = h.door.0.lock().unwrap();
    assert_eq!(door.len(), 1);
    assert_eq!(last_user_text(&door[0]), "old");
}
