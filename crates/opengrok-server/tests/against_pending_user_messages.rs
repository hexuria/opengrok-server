//! Durable pending user messages: NativeChat's follow-up queue, on the server.
//!
//! A queued send today lives in the client's process (`queued_sends`) and is invisible to every
//! other machine until it drains into `POST /ag-ui`. These routes are the Phase 2 contract so
//! cancel and edit sync. Needs Postgres; skips loudly without `OG_DATABASE_URL`.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, RunId};
use opengrok_core::run::{Run, RunCommand, RunEvent, RunView};
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

struct Harness {
    base: String,
    client: reqwest::Client,
    store: PgStore,
    minter: Arc<TokenMinter>,
}

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
    let minter = Arc::new(TokenMinter::new(b"pending-user-message-secret"));
    let auth = AuthState::new(store.clone(), minter.clone(), email.to_string());
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

#[tokio::test]
async fn enqueue_edit_cancel_and_hydrate_are_per_account() {
    let database_url = database_or_skip!();
    let stamp = stamp();
    let h = harness(&database_url, &format!("pending-owner-{stamp}@og.local")).await;
    let (account, access) = h.person(&format!("pending-owner-{stamp}@og.local")).await;
    let (_, stranger) = h
        .person(&format!("pending-stranger-{stamp}@og.local"))
        .await;
    let thread = format!("th-pending-{stamp}");
    seed_run(&h.store, &account, &thread, now_ms()).await;

    let (status, body) = h
        .pending(
            reqwest::Method::GET,
            Some(&access),
            &format!("/ag-ui/threads/{thread}/pending"),
            None,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["v"], 1);
    assert_eq!(body["pendingUserMessages"], json!([]));
    assert_eq!(body["pendingEvents"], json!([]));

    let (status, created) = h
        .pending(
            reqwest::Method::POST,
            Some(&access),
            &format!("/ag-ui/threads/{thread}/pending"),
            Some(&json!({
                "v": 1,
                "content": "send this after the turn",
                "replyTo": { "messageId": "m1", "preview": "hi" },
                "recipeId": "rec_demo",
                "recipeValues": { "q": "later" },
                "skillId": "skl_demo",
                "clientMessageId": "msg_bubble_1",
                "accountId": "acct_forged",
            })),
        )
        .await;
    assert_eq!(status, 201, "{created}");
    let id = created["pendingUserMessage"]["id"].as_str().expect("id");
    assert!(id.starts_with("pum_"), "{id}");
    assert_eq!(created["pendingUserMessage"]["v"], 1);
    assert_eq!(
        created["pendingUserMessage"]["content"],
        "send this after the turn"
    );
    assert_eq!(
        created["pendingUserMessage"]["clientMessageId"],
        "msg_bubble_1"
    );
    assert_eq!(created["pendingUserMessage"]["status"], "pending");
    let stored = h
        .store
        .pending_user_message(id, &account)
        .await
        .expect("load")
        .expect("row");
    assert_eq!(
        stored.account_id,
        account.as_str(),
        "identity is the bearer, never the body"
    );
    assert_eq!(created["event"]["type"], "CUSTOM");
    assert_eq!(created["event"]["name"], "pending-user-message");
    assert_eq!(created["event"]["value"]["op"], "created");

    let (status, again) = h
        .pending(
            reqwest::Method::POST,
            Some(&access),
            &format!("/ag-ui/threads/{thread}/pending"),
            Some(&json!({
                "v": 1,
                "content": "a retry must not duplicate",
                "clientMessageId": "msg_bubble_1",
            })),
        )
        .await;
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["pendingUserMessage"]["id"], id);
    assert_eq!(
        again["pendingUserMessage"]["content"], "send this after the turn",
        "idempotent create returns the existing row, it does not apply a new body"
    );

    let (status, patched) = h
        .pending(
            reqwest::Method::PATCH,
            Some(&access),
            &format!("/ag-ui/threads/{thread}/pending/{id}"),
            Some(&json!({ "v": 1, "content": "send this instead" })),
        )
        .await;
    assert_eq!(status, 200, "{patched}");
    assert_eq!(
        patched["pendingUserMessage"]["content"],
        "send this instead"
    );
    assert_eq!(patched["event"]["value"]["op"], "edited");
    assert_eq!(
        patched["pendingUserMessage"]["replyTo"]["messageId"], "m1",
        "omitted fields stay"
    );

    let thread_json: Value = serde_json::from_str(
        &h.client
            .get(format!("{}/ag-ui/threads/{thread}", h.base))
            .header("Authorization", format!("Bearer {access}"))
            .send()
            .await
            .expect("thread")
            .text()
            .await
            .expect("text"),
    )
    .expect("json");
    assert_eq!(thread_json["pendingUserMessages"][0]["id"], id);
    assert_eq!(thread_json["pendingEvents"][0]["value"]["op"], "snapshot");

    let (status, theirs) = h
        .pending(
            reqwest::Method::GET,
            Some(&stranger),
            &format!("/ag-ui/threads/{thread}/pending"),
            None,
        )
        .await;
    assert_eq!(
        status, 404,
        "another account's thread is not a 403: {theirs}"
    );

    let (status, unsigned) = h
        .pending(
            reqwest::Method::GET,
            None,
            &format!("/ag-ui/threads/{thread}/pending"),
            None,
        )
        .await;
    assert_eq!(status, 404, "{unsigned}");
    assert_eq!(
        theirs, unsigned,
        "not yours and not signed in are the same 404"
    );

    let (status, canceled) = h
        .pending(
            reqwest::Method::DELETE,
            Some(&access),
            &format!("/ag-ui/threads/{thread}/pending/{id}"),
            None,
        )
        .await;
    assert_eq!(status, 200, "{canceled}");
    assert_eq!(canceled["event"]["value"]["op"], "canceled");
    assert!(canceled["event"]["value"].get("message").is_none());

    let (status, again) = h
        .pending(
            reqwest::Method::DELETE,
            Some(&access),
            &format!("/ag-ui/threads/{thread}/pending/{id}"),
            None,
        )
        .await;
    assert_eq!(status, 200, "cancel is idempotent: {again}");

    let (status, listed) = h
        .pending(
            reqwest::Method::GET,
            Some(&access),
            &format!("/ag-ui/threads/{thread}/pending"),
            None,
        )
        .await;
    assert_eq!(status, 200, "{listed}");
    assert_eq!(listed["pendingUserMessages"], json!([]));
}

#[tokio::test]
async fn a_turn_drains_the_pending_item_and_a_second_turn_cannot_refire_it() {
    let database_url = database_or_skip!();
    let stamp = stamp();
    let h = harness(&database_url, &format!("pending-drain-{stamp}@og.local")).await;
    let (account, access) = h.person(&format!("pending-drain-{stamp}@og.local")).await;
    let thread = format!("th-drain-{stamp}");
    seed_run(&h.store, &account, &thread, now_ms()).await;

    let (status, created) = h
        .pending(
            reqwest::Method::POST,
            Some(&access),
            &format!("/ag-ui/threads/{thread}/pending"),
            Some(&json!({
                "v": 1,
                "content": "the follow-up",
                "clientMessageId": "msg_drain_1",
            })),
        )
        .await;
    assert_eq!(status, 201, "{created}");
    let pending_id = created["pendingUserMessage"]["id"]
        .as_str()
        .expect("id")
        .to_string();

    let run_id = format!("run_drain_{stamp}");
    let res = h
        .client
        .post(format!("{}/ag-ui", h.base))
        .header("Authorization", format!("Bearer {access}"))
        .json(&json!({
            "threadId": thread,
            "runId": run_id,
            "messages": [{ "id": "msg_drain_1", "role": "user", "content": "the follow-up" }],
            "forwardedProps": { "pendingId": pending_id },
        }))
        .send()
        .await
        .expect("turn");
    assert_eq!(
        res.status().as_u16(),
        200,
        "{}",
        res.text().await.unwrap_or_default()
    );
    drop(res);

    let (status, listed) = h
        .pending(
            reqwest::Method::GET,
            Some(&access),
            &format!("/ag-ui/threads/{thread}/pending"),
            None,
        )
        .await;
    assert_eq!(status, 200, "{listed}");
    assert_eq!(
        listed["pendingUserMessages"],
        json!([]),
        "a drained send must not stay in the queue"
    );

    let res = h
        .client
        .post(format!("{}/ag-ui", h.base))
        .header("Authorization", format!("Bearer {access}"))
        .json(&json!({
            "threadId": thread,
            "runId": format!("run_drain_again_{stamp}"),
            "messages": [{ "id": "msg_drain_1", "role": "user", "content": "the follow-up" }],
            "forwardedProps": { "pendingId": pending_id },
        }))
        .send()
        .await
        .expect("second turn");
    assert_eq!(res.status().as_u16(), 409, "double-fire is refused");
    let body: Value = res.json().await.expect("json");
    assert_eq!(body["v"], 1);
    assert_eq!(body["error"], "already-consumed");
    assert_eq!(body["event"]["value"]["op"], "drained");

    let (status, resurrect) = h
        .pending(
            reqwest::Method::POST,
            Some(&access),
            &format!("/ag-ui/threads/{thread}/pending"),
            Some(&json!({
                "v": 1,
                "content": "the follow-up again",
                "clientMessageId": "msg_drain_1",
            })),
        )
        .await;
    assert_eq!(
        status, 409,
        "a drained client message id must not re-queue: {resurrect}"
    );
    assert_eq!(resurrect["error"], "already-consumed");
}

#[tokio::test]
async fn a_turn_without_pending_id_still_drains_by_the_bubble_id() {
    let database_url = database_or_skip!();
    let stamp = stamp();
    let h = harness(&database_url, &format!("pending-bubble-{stamp}@og.local")).await;
    let (account, access) = h.person(&format!("pending-bubble-{stamp}@og.local")).await;
    let thread = format!("th-bubble-{stamp}");
    seed_run(&h.store, &account, &thread, now_ms()).await;

    let (status, created) = h
        .pending(
            reqwest::Method::POST,
            Some(&access),
            &format!("/ag-ui/threads/{thread}/pending"),
            Some(&json!({
                "v": 1,
                "content": "queued then sent",
                "clientMessageId": "msg_bubble_drain",
            })),
        )
        .await;
    assert_eq!(status, 201, "{created}");

    let res = h
        .client
        .post(format!("{}/ag-ui", h.base))
        .header("Authorization", format!("Bearer {access}"))
        .json(&json!({
            "threadId": thread,
            "runId": format!("run_bubble_{stamp}"),
            "messages": [{
                "id": "msg_bubble_drain",
                "role": "user",
                "content": "queued then sent"
            }],
            "forwardedProps": {},
        }))
        .send()
        .await
        .expect("turn");
    assert_eq!(res.status().as_u16(), 200);
    drop(res);

    let (status, listed) = h
        .pending(
            reqwest::Method::GET,
            Some(&access),
            &format!("/ag-ui/threads/{thread}/pending"),
            None,
        )
        .await;
    assert_eq!(status, 200, "{listed}");
    assert_eq!(listed["pendingUserMessages"], json!([]));
}

#[tokio::test]
async fn cancel_then_enqueue_the_same_bubble_is_a_new_pending_row() {
    let database_url = database_or_skip!();
    let stamp = stamp();
    let h = harness(&database_url, &format!("pending-requeue-{stamp}@og.local")).await;
    let (account, access) = h.person(&format!("pending-requeue-{stamp}@og.local")).await;
    let thread = format!("th-requeue-{stamp}");
    seed_run(&h.store, &account, &thread, now_ms()).await;

    let (status, first) = h
        .pending(
            reqwest::Method::POST,
            Some(&access),
            &format!("/ag-ui/threads/{thread}/pending"),
            Some(&json!({
                "v": 1,
                "content": "take this back",
                "clientMessageId": "msg_requeue",
            })),
        )
        .await;
    assert_eq!(status, 201, "{first}");
    let first_id = first["pendingUserMessage"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, _) = h
        .pending(
            reqwest::Method::DELETE,
            Some(&access),
            &format!("/ag-ui/threads/{thread}/pending/{first_id}"),
            None,
        )
        .await;
    assert_eq!(status, 200);

    let (status, second) = h
        .pending(
            reqwest::Method::POST,
            Some(&access),
            &format!("/ag-ui/threads/{thread}/pending"),
            Some(&json!({
                "v": 1,
                "content": "queue it again",
                "clientMessageId": "msg_requeue",
            })),
        )
        .await;
    assert_eq!(status, 201, "{second}");
    assert_ne!(second["pendingUserMessage"]["id"], first_id);
    assert_eq!(second["pendingUserMessage"]["content"], "queue it again");
}

#[tokio::test]
async fn a_payload_from_the_next_version_is_refused_unread() {
    let database_url = database_or_skip!();
    let stamp = stamp();
    let h = harness(&database_url, &format!("pending-v2-{stamp}@og.local")).await;
    let (account, access) = h.person(&format!("pending-v2-{stamp}@og.local")).await;
    let thread = format!("th-v2-{stamp}");
    seed_run(&h.store, &account, &thread, now_ms()).await;

    let (status, body) = h
        .pending(
            reqwest::Method::POST,
            Some(&access),
            &format!("/ag-ui/threads/{thread}/pending"),
            Some(&json!({ "v": 2, "content": "no" })),
        )
        .await;
    assert_eq!(status, 400, "{body}");
}

#[tokio::test]
async fn a_thread_the_account_has_never_run_does_not_grow_a_queue() {
    let database_url = database_or_skip!();
    let stamp = stamp();
    let h = harness(&database_url, &format!("pending-unknown-{stamp}@og.local")).await;
    let (_, access) = h.person(&format!("pending-unknown-{stamp}@og.local")).await;
    let (status, body) = h
        .pending(
            reqwest::Method::POST,
            Some(&access),
            "/ag-ui/threads/th-never-heard-of/pending",
            Some(&json!({ "v": 1, "content": "no" })),
        )
        .await;
    assert_eq!(status, 404, "{body}");
}

#[tokio::test]
async fn nullable_options_distinguish_omission_null_and_replacement() {
    let database_url = database_or_skip!();
    let stamp = stamp();
    let h = harness(&database_url, &format!("pending-null-{stamp}@og.local")).await;
    let (account, access) = h.person(&format!("pending-null-{stamp}@og.local")).await;
    let thread = format!("th-null-{stamp}");
    seed_run(&h.store, &account, &thread, now_ms()).await;
    let path = format!("/ag-ui/threads/{thread}/pending");
    let options = json!({"replyTo": "m1", "recipeId": "rec_1", "recipeValues": {"q": "old"}, "skillId": "skl_1"});
    let mut body = options.clone();
    body["content"] = json!("later");
    let (status, created) = h
        .pending(reqwest::Method::POST, Some(&access), &path, Some(&body))
        .await;
    assert_eq!(status, 201, "{created}");
    let id = created["pendingUserMessage"]["id"].as_str().unwrap();
    let edit_path = format!("{path}/{id}");
    for body in [json!({}), json!({"content": "edited"})] {
        let (status, response) = h
            .pending(
                reqwest::Method::PATCH,
                Some(&access),
                &edit_path,
                Some(&body),
            )
            .await;
        assert_eq!(status, 200, "{response}");
        for key in ["replyTo", "recipeId", "recipeValues", "skillId"] {
            assert_eq!(response["pendingUserMessage"][key], options[key]);
        }
    }
    let nulls = json!({"replyTo": null, "recipeId": null, "recipeValues": null, "skillId": null});
    let (status, cleared) = h
        .pending(
            reqwest::Method::PATCH,
            Some(&access),
            &edit_path,
            Some(&nulls),
        )
        .await;
    assert_eq!(status, 200, "{cleared}");
    let (status, listed) = h
        .pending(reqwest::Method::GET, Some(&access), &path, None)
        .await;
    assert_eq!(status, 200, "{listed}");
    for key in ["replyTo", "recipeId", "recipeValues", "skillId"] {
        assert_eq!(cleared["pendingUserMessage"][key], Value::Null, "{key}");
        assert_eq!(
            cleared["event"]["value"]["message"][key],
            Value::Null,
            "{key}"
        );
        assert_eq!(listed["pendingUserMessages"][0][key], Value::Null, "{key}");
    }
    let stored = h
        .store
        .pending_user_message(id, &account)
        .await
        .unwrap()
        .unwrap();
    assert!(
        stored.reply_to.is_none()
            && stored.recipe_id.is_none()
            && stored.recipe_values.is_none()
            && stored.skill_id.is_none()
    );
    let replacements = json!({"replyTo": {"messageId": "m2"}, "recipeId": "rec_2", "recipeValues": {"q": "new"}, "skillId": "skl_2"});
    let (status, replaced) = h
        .pending(
            reqwest::Method::PATCH,
            Some(&access),
            &edit_path,
            Some(&replacements),
        )
        .await;
    assert_eq!(status, 200, "{replaced}");
    for key in ["replyTo", "recipeId", "recipeValues", "skillId"] {
        assert_eq!(replaced["pendingUserMessage"][key], replacements[key]);
    }
    for invalid in [
        json!({"replyTo": 7}),
        json!({"recipeId": []}),
        json!({"skillId": {}}),
    ] {
        let (status, response) = h
            .pending(
                reqwest::Method::PATCH,
                Some(&access),
                &edit_path,
                Some(&invalid),
            )
            .await;
        assert_eq!(status, 400, "{response}");
    }
    for mut body in [json!({}), nulls] {
        body["content"] = json!("new send");
        let (status, response) = h
            .pending(reqwest::Method::POST, Some(&access), &path, Some(&body))
            .await;
        assert_eq!(status, 201, "{response}");
        for key in ["replyTo", "recipeId", "recipeValues", "skillId"] {
            assert_eq!(response["pendingUserMessage"][key], Value::Null, "{key}");
        }
    }
}

#[tokio::test]
async fn concurrent_disjoint_edits_preserve_both_changes() {
    let database_url = database_or_skip!();
    let stamp = stamp();
    let h = harness(&database_url, &format!("pending-race-{stamp}@og.local")).await;
    let (account, access) = h.person(&format!("pending-race-{stamp}@og.local")).await;
    let thread = format!("th-race-{stamp}");
    seed_run(&h.store, &account, &thread, now_ms()).await;
    let path = format!("/ag-ui/threads/{thread}/pending");
    let (status, created) = h
        .pending(
            reqwest::Method::POST,
            Some(&access),
            &path,
            Some(&json!({"content": "old", "skillId": "skl_old"})),
        )
        .await;
    assert_eq!(status, 201, "{created}");
    let id = created["pendingUserMessage"]["id"].as_str().unwrap();
    let edit_path = format!("{path}/{id}");
    // Hold the row until both writers have reached UPDATE. A read/modify/write
    // implementation has already read the old values at this point.
    let mut lock = h.store.pool().begin().await.unwrap();
    let lock_pid: i32 = sqlx::query_scalar("select pg_backend_pid()")
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    sqlx::query("select id from pending_user_message where id = $1 for update")
        .bind(id)
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    let content = json!({"content": "new"});
    let skill = json!({"skillId": "skl_new"});
    let edits = async {
        tokio::join!(
            h.pending(
                reqwest::Method::PATCH,
                Some(&access),
                &edit_path,
                Some(&content)
            ),
            h.pending(
                reqwest::Method::PATCH,
                Some(&access),
                &edit_path,
                Some(&skill)
            ),
        )
    };
    let release = async {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let blocked: i64 = sqlx::query_scalar(
                    "select count(*) from pg_stat_activity a where $1 = any(pg_blocking_pids(a.pid)) or exists (select 1 from unnest(pg_blocking_pids(a.pid)) b(pid) where $1 = any(pg_blocking_pids(b.pid)))"
                ).bind(lock_pid).fetch_one(h.store.pool()).await.unwrap();
                if blocked >= 2 { break; }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await.expect("both PATCH requests must reach the locked row");
        lock.commit().await.unwrap();
    };
    let ((first, second), ()) = tokio::join!(edits, release);
    assert_eq!(first.0, 200, "{}", first.1);
    assert_eq!(second.0, 200, "{}", second.1);
    let stored = h
        .store
        .pending_user_message(id, &account)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.content, "new");
    assert_eq!(stored.skill_id.as_deref(), Some("skl_new"));
}
