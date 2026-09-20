//! A thread comes back whole, in the order it happened.
//!
//! "even if i close the app or change bot i shouldnt be worrying on missing messages, we have it
//! all on server" — a user, 17 Sep 2026, and they were right on both counts: the server has the
//! record and had no way to hand over more than one run of it. `GET /ag-ui/threads/{id}` is that
//! way, so a client never has to treat its own local copy of a conversation as the truth. Needs
//! Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, RunId};
use opengrok_core::run::{Run, RunCommand, RunEvent, RunView, SuspendReason};
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::password::hash_password;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
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

/// How a run ends, as far as a transcript can see.
enum Ending<'a> {
    Finished,
    Failed(&'a str),
    /// Stopped on a card nobody has answered — a run that is still open in the middle of a
    /// history, which is the case a transcript must not silently drop.
    AwaitingApproval,
}

/// Apply what the aggregate decided and keep it for the append, so the seed is a real run log
/// rather than rows shaped to look like one.
fn record(run: &mut Run, log: &mut Vec<RunEvent>, produced: Vec<RunEvent>) {
    for event in &produced {
        run.apply(event);
    }
    log.extend(produced);
}

fn view_of(run: &Run, id: &RunId, thread: &str, at_ms: i64) -> RunView {
    RunView {
        id: id.clone(),
        thread_id: thread.to_string(),
        status: run.status,
        event_count: run.emitted.len() as i64,
        updated_at_ms: at_ms,
    }
}

/// Journal one whole run under a thread: a start, its frames, an ending.
async fn seed_run(
    store: &PgStore,
    account: &AccountId,
    thread: &str,
    at_ms: i64,
    deltas: &[&str],
    ending: Ending<'_>,
) -> RunId {
    let id = RunId::new();
    let mut run = Run::default();
    let mut log = Vec::new();

    let produced = run
        .decide(RunCommand::Start {
            thread_id: thread.to_string(),
            coworker_id: None,
            model: None,
            system: None,
            at_ms,
        })
        .expect("start");
    record(&mut run, &mut log, produced);

    for delta in deltas {
        let produced = run
            .decide(RunCommand::Emit {
                payload: json!({
                    "type": "TEXT_MESSAGE_CONTENT",
                    "messageId": "msg-1",
                    "delta": delta,
                }),
                at_ms,
            })
            .expect("emit");
        record(&mut run, &mut log, produced);
    }

    let produced = match ending {
        Ending::Finished => run.decide(RunCommand::Finish { at_ms }),
        Ending::Failed(reason) => run.decide(RunCommand::Fail {
            reason: reason.to_string(),
            at_ms,
        }),
        Ending::AwaitingApproval => run.decide(RunCommand::Suspend {
            call_id: "call-1".to_string(),
            tool: "shell".to_string(),
            arguments: json!({ "command": "ls" }),
            reason: SuspendReason::ExecConsent,
            at_ms,
        }),
    }
    .expect("ending");
    record(&mut run, &mut log, produced);

    let view = view_of(&run, &id, thread, at_ms);
    store
        .append_run(&id, 0, &log, &view, Some(account))
        .await
        .expect("append run");
    id
}

/// One more frame on a run that has already been journaled, later. The projection keeps the first
/// append's `started_at_ms` and moves `updated_at_ms`, which is the state a still-open run is in
/// while a person is looking at the card it is waiting on.
async fn emit_later(
    store: &PgStore,
    account: &AccountId,
    id: &RunId,
    thread: &str,
    at_ms: i64,
    delta: &str,
) {
    let (mut run, seq) = store.load_run(id).await.expect("load run");
    let produced = run
        .decide(RunCommand::Emit {
            payload: json!({ "type": "TEXT_MESSAGE_CONTENT", "messageId": "msg-1", "delta": delta }),
            at_ms,
        })
        .expect("emit");
    let mut log = Vec::new();
    record(&mut run, &mut log, produced);
    let view = view_of(&run, id, thread, at_ms);
    store
        .append_run(id, seq, &log, &view, Some(account))
        .await
        .expect("append later");
}

/// A run the projection knows about whose log has no start — it never took its turn, so there is
/// nothing of it to read back.
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
        status: opengrok_core::run::RunStatus::Running,
        event_count: 0,
        updated_at_ms: at_ms,
    };
    store
        .append_run(&id, 0, &[], &view, Some(account))
        .await
        .expect("append run view");
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
    let minter = Arc::new(TokenMinter::new(b"thread-replay-secret"));
    let auth = AuthState::new(store.clone(), minter.clone(), email.to_string());
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
    let app = opengrok_server::router(agui);
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
    /// Somebody signed in. Every account minted here is a real one in the store, because the
    /// owner check reads the run's `account_id` and not the token's shape.
    async fn person(&self, email: &str) -> (AccountId, String) {
        let account = seed_account(&self.store, email).await;
        let access = self
            .minter
            .mint_access(
                account.as_str(),
                "sess-thread",
                email,
                "ultra",
                chrono::Utc::now().timestamp(),
                3600,
            )
            .expect("mint access");
        (account, access)
    }

    async fn thread(&self, access: &str, thread_id: &str, query: &str) -> (u16, String) {
        let res = self
            .client
            .get(format!("{}/ag-ui/threads/{thread_id}{query}", self.base))
            .header("Authorization", format!("Bearer {access}"))
            .send()
            .await
            .expect("thread history");
        let status = res.status().as_u16();
        (status, res.text().await.expect("body"))
    }

    async fn thread_json(&self, access: &str, thread_id: &str, query: &str) -> Value {
        let (status, body) = self.thread(access, thread_id, query).await;
        assert_eq!(status, 200, "{body}");
        serde_json::from_str(&body).expect("json body")
    }
}

fn runs_of(body: &Value) -> &Vec<Value> {
    body["runs"].as_array().expect("runs array")
}

fn ids_of(body: &Value) -> Vec<String> {
    runs_of(body)
        .iter()
        .map(|run| run["runId"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test]
async fn a_thread_reads_oldest_first_with_every_frame_it_emitted() {
    let database_url = database_or_skip!();
    let email = format!("thread-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let (account, access) = h.person(&email).await;
    let thread = format!("th-{}", uuid::Uuid::now_v7().simple());
    let base = now_ms();

    // The first turn is still waiting on an approval card and was touched LAST, which is exactly
    // the case that tells the two orderings apart: by when it last moved it would sort to the
    // bottom, and a conversation that rearranges itself while a run streams is the bug.
    let first = seed_run(
        &h.store,
        &account,
        &thread,
        base + 1_000,
        &["hello ", "world"],
        Ending::AwaitingApproval,
    )
    .await;
    let second = seed_run(
        &h.store,
        &account,
        &thread,
        base + 2_000,
        &["the second turn"],
        Ending::Finished,
    )
    .await;
    let third = seed_run(
        &h.store,
        &account,
        &thread,
        base + 3_000,
        &["the third turn"],
        Ending::Failed("the model gateway refused"),
    )
    .await;
    emit_later(
        &h.store,
        &account,
        &first,
        &thread,
        base + 9_000,
        " (still here)",
    )
    .await;

    let body = h.thread_json(&access, &thread, "").await;
    assert_eq!(body["threadId"], json!(thread));
    assert_eq!(
        ids_of(&body),
        vec![first.as_str(), second.as_str(), third.as_str()],
        "oldest first, by when each run began: {body}"
    );

    let runs = runs_of(&body);
    assert_eq!(runs[0]["status"], json!("awaiting-approval"), "{body}");
    assert_eq!(runs[1]["status"], json!("finished"), "{body}");
    assert_eq!(runs[2]["status"], json!("failed"), "{body}");
    assert_eq!(runs[0]["failure"], Value::Null, "{body}");
    assert_eq!(
        runs[2]["failure"],
        json!("the model gateway refused"),
        "a failed turn says why, the way replay of a single run does: {body}"
    );

    assert_eq!(runs[0]["startedAtMs"], json!(base + 1_000), "{body}");
    assert_eq!(
        runs[0]["updatedAtMs"],
        json!(base + 9_000),
        "the start is kept and the last move is the new one: {body}"
    );
    assert_eq!(runs[1]["startedAtMs"], json!(base + 2_000), "{body}");

    let deltas: Vec<&str> = runs[0]["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|event| event["delta"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        deltas,
        vec!["hello ", "world", " (still here)"],
        "every frame the run emitted, in order: {body}"
    );
    assert_eq!(
        runs[1]["events"][0]["delta"],
        json!("the second turn"),
        "{body}"
    );
}

#[tokio::test]
async fn a_run_that_never_started_is_left_out_of_the_thread() {
    let database_url = database_or_skip!();
    let email = format!(
        "thread-unstarted-{}@og.local",
        uuid::Uuid::now_v7().simple()
    );
    let h = harness(&database_url, &email).await;
    let (account, access) = h.person(&email).await;
    let thread = format!("th-{}", uuid::Uuid::now_v7().simple());
    let base = now_ms();

    let spoke = seed_run(
        &h.store,
        &account,
        &thread,
        base + 1_000,
        &["a turn that happened"],
        Ending::Finished,
    )
    .await;
    let never = seed_run_that_never_started(&h.store, &account, &thread, base + 2_000).await;

    let body = h.thread_json(&access, &thread, "").await;
    assert_eq!(
        ids_of(&body),
        vec![spoke.as_str()],
        "a run with no start took no turn, so it is not part of the transcript: {body}"
    );
    assert!(
        !body.to_string().contains(never.as_str()),
        "the unstarted run must not appear at all: {body}"
    );
}

#[tokio::test]
async fn somebody_elses_thread_and_no_thread_at_all_answer_exactly_the_same() {
    let database_url = database_or_skip!();
    let owner_email = format!("thread-owner-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &owner_email).await;
    let (owner, owner_access) = h.person(&owner_email).await;
    let stranger_email = format!("thread-stranger-{}@og.local", uuid::Uuid::now_v7().simple());
    let (_stranger, stranger_access) = h.person(&stranger_email).await;
    let thread = format!("th-{}", uuid::Uuid::now_v7().simple());

    seed_run(
        &h.store,
        &owner,
        &thread,
        now_ms(),
        &["something private"],
        Ending::Finished,
    )
    .await;

    // The thread is really there, so the 404 below is the owner check and not an empty database.
    let mine = h.thread_json(&owner_access, &thread, "").await;
    assert_eq!(runs_of(&mine).len(), 1, "{mine}");

    let (theirs_status, theirs_body) = h.thread(&stranger_access, &thread, "").await;
    let unknown = format!("th-{}", uuid::Uuid::now_v7().simple());
    let (unknown_status, unknown_body) = h.thread(&stranger_access, &unknown, "").await;

    assert_eq!(
        theirs_status, 404,
        "a thread that is not yours is not a 403: that would confirm it exists"
    );
    assert_eq!(unknown_status, 404, "{unknown_body}");
    assert_eq!(
        (theirs_status, theirs_body.as_str()),
        (unknown_status, unknown_body.as_str()),
        "the two answers must be indistinguishable, or a thread id can be probed"
    );
    assert!(!theirs_body.contains("something private"), "{theirs_body}");

    // No token at all is the same answer again, for the same reason.
    let res = h
        .client
        .get(format!("{}/ag-ui/threads/{thread}", h.base))
        .send()
        .await
        .expect("no-token request");
    assert_eq!(res.status().as_u16(), 404);
}

#[tokio::test]
async fn the_limit_keeps_the_newest_runs_and_is_capped() {
    let database_url = database_or_skip!();
    let email = format!("thread-limit-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let (account, access) = h.person(&email).await;
    let thread = format!("th-{}", uuid::Uuid::now_v7().simple());
    let base = now_ms();

    // One more than the cap, so the cap has something to cut.
    let mut seeded = Vec::new();
    for turn in 0..101_i64 {
        seeded.push(
            seed_run(
                &h.store,
                &account,
                &thread,
                base + turn,
                &["a turn"],
                Ending::Finished,
            )
            .await,
        );
    }
    let id_at = |turn: usize| seeded[turn].as_str().to_string();

    let capped = h.thread_json(&access, &thread, "?limit=1000").await;
    let capped_ids = ids_of(&capped);
    assert_eq!(
        capped_ids.len(),
        100,
        "a limit past the cap is clamped, not honoured"
    );
    assert_eq!(
        capped_ids.first(),
        Some(&id_at(1)),
        "the oldest turn is the one dropped"
    );
    assert_eq!(
        capped_ids.last(),
        Some(&id_at(100)),
        "and the newest is kept"
    );

    let by_default = h.thread_json(&access, &thread, "").await;
    assert_eq!(
        ids_of(&by_default).len(),
        20,
        "asking for no number gets the default depth"
    );

    let two = h.thread_json(&access, &thread, "?limit=2").await;
    assert_eq!(
        ids_of(&two),
        vec![id_at(99), id_at(100)],
        "the last two turns, still oldest first: {two}"
    );

    let one = h.thread_json(&access, &thread, "?limit=0").await;
    assert_eq!(
        ids_of(&one),
        vec![id_at(100)],
        "a nonsense limit is clamped rather than refused: {one}"
    );
}

#[tokio::test]
async fn asking_without_the_frames_still_says_which_runs_exist() {
    let database_url = database_or_skip!();
    let email = format!("thread-listing-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let (account, access) = h.person(&email).await;
    let thread = format!("th-{}", uuid::Uuid::now_v7().simple());
    let base = now_ms();

    let first = seed_run(
        &h.store,
        &account,
        &thread,
        base + 1_000,
        &["a long answer, one frame at a time"],
        Ending::Finished,
    )
    .await;
    let second = seed_run(
        &h.store,
        &account,
        &thread,
        base + 2_000,
        &["and another"],
        Ending::Failed("the model gateway refused"),
    )
    .await;

    let listed = h.thread_json(&access, &thread, "?events=false").await;
    assert_eq!(ids_of(&listed), vec![first.as_str(), second.as_str()]);
    for run in runs_of(&listed) {
        assert!(
            run.get("events").is_none(),
            "no frames means the key is absent, not an empty list that claims the run said \
             nothing: {run}"
        );
    }
    assert_eq!(runs_of(&listed)[1]["status"], json!("failed"), "{listed}");
    assert_eq!(
        runs_of(&listed)[1]["failure"],
        json!("the model gateway refused"),
        "everything but the frames is still answered: {listed}"
    );

    let whole = h.thread_json(&access, &thread, "").await;
    assert_eq!(
        runs_of(&whole)[0]["events"][0]["delta"],
        json!("a long answer, one frame at a time"),
        "the frames are the default: {whole}"
    );
}
