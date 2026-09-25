//! A run id is one run: a POST that reuses one never starts a second loop.
//!
//! A client that retried its POST — its stream dropped, or it sent the same body twice — used to
//! start a second loop on the same run: a second model call, every tool twice, one transcript
//! interleaving both. And because `append_run` let a later batch name a new owner, a stranger
//! who learned a run id could POST to it and take the run. Now the first POST claims the run;
//! its owner gets it back on a retry, replayed and followed to its end, and anybody else is
//! refused (`formal/tla/RunLifecycle.tla` OneDriver, RetryNotRefused).
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, RunId};
use opengrok_core::run::{Run, RunCommand, RunView};
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

fn fresh(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::now_v7().simple())
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

/// The echoing door, counting its calls: "nothing runs twice" is a model call, and a second
/// loop on an ended run leaves no trace in the log, whose ending refuses its frames.
struct CountingDoor(Arc<AtomicUsize>, MockDoor);

#[async_trait::async_trait]
impl ModelDoor for CountingDoor {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        self.1.stream(request).await
    }
}

struct Harness {
    base: String,
    client: reqwest::Client,
    store: PgStore,
    minter: Arc<TokenMinter>,
    calls: Arc<AtomicUsize>,
}

async fn harness(database_url: &str, email: &str) -> Harness {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let minter = Arc::new(TokenMinter::new(b"retried-run-secret"));
    let auth = AuthState::new(store.clone(), minter.clone(), email.to_string());
    let calls = Arc::new(AtomicUsize::new(0));
    let agui = AgUiState {
        auth,
        door: Arc::new(CountingDoor(calls.clone(), MockDoor::echoing())),
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
        calls,
    }
}

impl Harness {
    fn model_calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    async fn person(&self, email: &str) -> (AccountId, String) {
        let account = seed_account(&self.store, email).await;
        let access = self
            .minter
            .mint_access(
                account.as_str(),
                "sess-retry",
                email,
                "ultra",
                chrono::Utc::now().timestamp(),
                3600,
            )
            .expect("mint access");
        (account, access)
    }

    /// POST a turn and read its whole answer: the status and every `data:` frame.
    async fn post(&self, access: Option<&str>, thread: &str, run: &str) -> (u16, Vec<Value>) {
        let mut request = self
            .client
            .post(format!("{}/ag-ui", self.base))
            .json(&json!({
                "threadId": thread,
                "runId": run,
                "messages": [{"id": "m1", "role": "user", "content": "ping"}],
            }));
        if let Some(access) = access {
            request = request.header("Authorization", format!("Bearer {access}"));
        }
        let res = request.send().await.expect("post a turn");
        let status = res.status().as_u16();
        let body = res.text().await.expect("body");
        let frames = body
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str(data).ok())
            .collect();
        (status, frames)
    }

    async fn replay_status(&self, access: &str, run: &str) -> u16 {
        self.client
            .get(format!("{}/ag-ui/runs/{run}", self.base))
            .header("Authorization", format!("Bearer {access}"))
            .send()
            .await
            .expect("replay")
            .status()
            .as_u16()
    }

    /// How many frames of `kind` the run's log holds.
    async fn logged(&self, run: &str, kind: &str) -> usize {
        let (run, _) = self
            .store
            .load_run(&RunId::from_stored(run.to_string()))
            .await
            .expect("load");
        run.emitted
            .iter()
            .filter(|frame| frame["type"] == json!(kind))
            .count()
    }
}

fn types(frames: &[Value]) -> Vec<String> {
    frames
        .iter()
        .map(|frame| frame["type"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn ends_once(frames: &[Value]) -> bool {
    let ends = types(frames)
        .iter()
        .filter(|kind| *kind == "RUN_FINISHED" || *kind == "RUN_ERROR")
        .count();
    ends == 1 && types(frames).last().map(String::as_str) == Some("RUN_FINISHED")
}

fn said(frames: &[Value]) -> String {
    frames
        .iter()
        .filter_map(|frame| frame["delta"].as_str())
        .collect()
}

/// THE RETRY GETS ITS RUN BACK, AND NOTHING RUNS TWICE. The same POST again, after the first
/// answered, is its owner's retry: it gets the run it already had, replayed from the log with
/// one ending, and the log still holds exactly one turn.
#[tokio::test]
async fn a_retried_post_gets_the_run_back_and_nothing_runs_twice() {
    let database_url = database_or_skip!();
    let email = format!("{}@og.local", fresh("retry-once"));
    let h = harness(&database_url, &email).await;
    let (_, access) = h.person(&email).await;
    let (thread, run) = (fresh("th"), fresh("run"));

    let (status, first) = h.post(Some(&access), &thread, &run).await;
    assert_eq!(status, 200);
    assert!(ends_once(&first), "{:?}", types(&first));

    let (status, again) = h.post(Some(&access), &thread, &run).await;
    assert_eq!(status, 200, "the owner's retry is answered, not refused");
    assert!(ends_once(&again), "one ending: {:?}", types(&again));
    assert_eq!(
        types(&again).first().map(String::as_str),
        Some("RUN_STARTED")
    );
    assert_eq!(
        said(&again),
        said(&first),
        "the same answer, not a second one"
    );
    assert_eq!(h.model_calls(), 1, "and the model was asked once");
    assert_eq!(
        h.logged(&run, "RUN_STARTED").await,
        1,
        "one turn in the log"
    );
}

/// TWO POSTS AT ONCE RUN ONE LOOP. Both can pass the check for an existing run; only one
/// appends the run's `Started`, and the other follows that one's run to its end.
#[tokio::test]
async fn two_posts_at_once_run_one_loop() {
    let database_url = database_or_skip!();
    let email = format!("{}@og.local", fresh("retry-race"));
    let h = harness(&database_url, &email).await;
    let (_, access) = h.person(&email).await;

    for round in 0..4 {
        let (thread, run) = (fresh("th"), fresh("run"));
        let ((a_status, a), (b_status, b)) = tokio::join!(
            h.post(Some(&access), &thread, &run),
            h.post(Some(&access), &thread, &run)
        );
        assert_eq!((a_status, b_status), (200, 200));
        for frames in [&a, &b] {
            assert!(ends_once(frames), "{:?}", types(frames));
            assert!(said(frames).contains("ping"), "{:?}", types(frames));
        }
        assert_eq!(
            h.model_calls(),
            round + 1,
            "one loop ran, so one model call"
        );
        assert_eq!(h.logged(&run, "RUN_STARTED").await, 1);
    }
}

/// A RUN ID IS NOT A KEY TO SOMEBODY ELSE'S RUN. Another account's POST with it is refused,
/// adds nothing to the log, and leaves the run its owner's.
#[tokio::test]
async fn another_account_cannot_take_a_run_by_its_id() {
    let database_url = database_or_skip!();
    let email = format!("{}@og.local", fresh("retry-owner"));
    let h = harness(&database_url, &email).await;
    let (_, owner) = h.person(&email).await;
    let (_, stranger) = h
        .person(&format!("{}@og.local", fresh("retry-stranger")))
        .await;
    let (thread, run) = (fresh("th"), fresh("run"));
    let (status, _) = h.post(Some(&owner), &thread, &run).await;
    assert_eq!(status, 200);
    let before = h.logged(&run, "TEXT_MESSAGE_CONTENT").await;

    let (status, _) = h.post(Some(&stranger), &thread, &run).await;
    assert_eq!(status, 409, "a run id somebody else holds is refused");

    assert_eq!(
        h.logged(&run, "TEXT_MESSAGE_CONTENT").await,
        before,
        "nothing added"
    );
    assert_eq!(h.logged(&run, "RUN_STARTED").await, 1);
    assert_eq!(
        h.replay_status(&owner, &run).await,
        200,
        "still the owner's"
    );
    assert_eq!(
        h.replay_status(&stranger, &run).await,
        404,
        "and not the stranger's"
    );
}

/// AN ANONYMOUS CALLER OWNS NOTHING, so it cannot have a run back and cannot run one twice.
#[tokio::test]
async fn an_anonymous_post_cannot_reuse_a_run_id() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, &format!("{}@og.local", fresh("retry-anon"))).await;
    let (thread, run) = (fresh("th"), fresh("run"));
    let (status, first) = h.post(None, &thread, &run).await;
    assert_eq!(status, 200);
    assert!(ends_once(&first), "{:?}", types(&first));
    let (status, _) = h.post(None, &thread, &run).await;
    assert_eq!(status, 409);
    assert_eq!(h.logged(&run, "RUN_STARTED").await, 1);
}

/// THE OWNER IS SET ONCE. A batch written later in somebody else's name does not move the run to
/// them: `append_run` used to prefer the newer account.
#[tokio::test]
async fn a_runs_owner_is_set_once() {
    let database_url = database_or_skip!();
    let h = harness(
        &database_url,
        &format!("{}@og.local", fresh("retry-set-once")),
    )
    .await;
    let owner = seed_account(&h.store, &format!("{}@og.local", fresh("first"))).await;
    let other = seed_account(&h.store, &format!("{}@og.local", fresh("second"))).await;
    let id = RunId::new();
    let at_ms = now_ms();
    let mut run = Run::default();
    let started = run
        .decide(RunCommand::Start {
            thread_id: "th".to_string(),
            coworker_id: None,
            model: None,
            system: None,
            skill_id: None,
            prompt: None,
            at_ms,
        })
        .expect("start");
    for event in &started {
        run.apply(event);
    }
    let view = |run: &Run| RunView {
        id: id.clone(),
        thread_id: "th".to_string(),
        status: run.status,
        event_count: run.emitted.len() as i64,
        updated_at_ms: at_ms,
    };
    let seq = h
        .store
        .append_run(&id, 0, &started, &view(&run), Some(&owner))
        .await
        .expect("start");
    let emitted = run
        .decide(RunCommand::Emit {
            payload: json!({"type": "TEXT_MESSAGE_CONTENT", "delta": "x"}),
            at_ms,
        })
        .expect("emit");
    for event in &emitted {
        run.apply(event);
    }
    h.store
        .append_run(&id, seq, &emitted, &view(&run), Some(&other))
        .await
        .expect("a later batch in another name");

    assert!(h.store.run_owned_by(&id, &owner).await.expect("owner"));
    assert!(!h.store.run_owned_by(&id, &other).await.expect("other"));
}
