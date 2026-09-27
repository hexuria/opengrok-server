//! A run a restart interrupted is carried on, not failed — when that is safe (#91).
//!
//! The recovery sweep used to fail every run whose process died. Now it reads the log: a run
//! interrupted between two steps is resumed in its next generation and finishes; one whose tool
//! may have acted (its start is on record without its result) is failed, naming the tool; one
//! already carried on twice is failed the third time; and one a person had answered, whose call
//! never started, has that answer carried out rather than asked again.
//!
//! The runs are written straight to the store as an interrupted process would have left them,
//! with a log quiet for longer than a lease, and the sweep is driven by hand.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use sqlx::Row;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use opengrok_box::{BoxResult, CommandOutput, Computer, StartedCommand};

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, CoworkerId, RunId};
use opengrok_core::run::{
    Run, RunCommand, RunEvent, RunStatus, RunView, StartedTool, SuspendReason,
};
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

/// ONE SWEEP AT A TIME. `sweep_once` claims whatever is abandoned database-wide, so one test's
/// sweep can claim another's run and spawn its continuation in its own process — which nextest
/// ends with that test, killing the continuation mid-run. A Postgres advisory lock, held for the
/// test's length on a connection of its own, keeps the sweeps apart across processes.
async fn one_sweeper(database_url: &str) -> sqlx::PgConnection {
    use sqlx::Connection;
    let mut connection = sqlx::PgConnection::connect(database_url)
        .await
        .expect("connect for the sweep lock");
    sqlx::query("select pg_advisory_lock($1)")
        .bind(0x5eed_0091_i64)
        .execute(&mut connection)
        .await
        .expect("take the sweep lock");
    connection
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// A box that runs anything and says so.
#[derive(Default)]
struct StubBox {
    ran: Mutex<Vec<String>>,
}

#[async_trait]
impl Computer for StubBox {
    async fn create(&self, _ttl_seconds: Option<u64>) -> BoxResult<String> {
        Ok(format!("bx_form_{}", uuid::Uuid::now_v7().simple()))
    }
    async fn run(&self, _b: &str, command: &str, _t: u32) -> BoxResult<CommandOutput> {
        self.ran.lock().expect("ran").push(command.to_string());
        Ok(CommandOutput {
            exit_code: 0,
            stdout: "ran".to_string(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            timed_out: false,
        })
    }
    async fn start(&self, _b: &str, _c: &str) -> BoxResult<StartedCommand> {
        Ok(StartedCommand {
            process_id: "p1".to_string(),
            running: false,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
        })
    }
    async fn watch(&self, _b: &str, _p: &str) -> BoxResult<StartedCommand> {
        Ok(StartedCommand {
            process_id: "p1".to_string(),
            running: false,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
        })
    }
    async fn read_file(&self, _b: &str, _p: &str) -> BoxResult<String> {
        Ok(String::new())
    }
    async fn write_file(&self, _b: &str, _p: &str, _c: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn expose_port(&self, _b: &str, _p: u16, _t: &str) -> BoxResult<String> {
        Ok("http://stub.invalid".to_string())
    }
    async fn stop(&self, _b: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn resume(&self, _b: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn destroy(&self, _b: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn state(&self, _b: &str) -> BoxResult<String> {
        Ok("running".to_string())
    }
    async fn offers_a_screen(&self, _b: &str) -> bool {
        true
    }
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
    stub: Arc<StubBox>,
    store: PgStore,
    host: HostState,
    account: AccountId,
    coworker: CoworkerId,
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
    let account = seed_account(&store, email).await;
    let stub = Arc::new(StubBox::default());
    let minter = Arc::new(TokenMinter::new(b"a-run-carried-on-after-a-restart"));
    let auth = AuthState::new(store.clone(), minter, email.to_string());
    let agui = AgUiState {
        auth,
        door: Arc::new(MockDoor::echoing()),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer: Some(stub.clone()),
        vault: None,
        connectors: Connectors {
            providers: Arc::new(BTreeMap::new()),
            redirect_uri: "http://127.0.0.1/callback".to_string(),
        },
        plugins: Arc::new(BTreeMap::new()),
        host_settings: None,
    };
    let host = HostState::new(agui.clone(), None);
    let app = opengrok_server::router(agui.clone(), host.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base = format!(
        "http://127.0.0.1:{}",
        listener.local_addr().expect("addr").port()
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    let token = agui
        .auth
        .minter
        .mint_access(
            account.as_str(),
            "sess-interrupted",
            email,
            "ultra",
            chrono::Utc::now().timestamp(),
            3600,
        )
        .expect("mint access");
    let hired: Value = reqwest::Client::new()
        .post(format!("{base}/coworkers"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({ "name": "Nightshift" }))
        .send()
        .await
        .expect("hire")
        .json()
        .await
        .expect("hired");
    let coworker = CoworkerId::from_stored(hired["id"].as_str().expect("coworker id"));
    Harness {
        stub,
        store,
        host,
        account,
        coworker,
    }
}

/// A run as a dead process left it: started, the given commands applied, and a log quiet for
/// longer than a lease, so the sweep may claim it.
async fn interrupted_run(h: &Harness, commands: Vec<RunCommand>) -> RunId {
    let id = RunId::new();
    let thread = format!("th-{}", uuid::Uuid::now_v7().simple());
    let quiet_since = now_ms() - 3 * opengrok_server::recovery::LEASE_MS;
    let mut run = Run::default();
    let mut log = Vec::new();
    let start = RunCommand::Start {
        thread_id: thread.clone(),
        coworker_id: Some(h.coworker.clone()),
        model: Some("oag/cheap".to_string()),
        system: None,
        skill_id: None,
        prompt: Some(vec![json!({
            "id": "m-person",
            "role": "user",
            "content": "summarise the inbox",
        })]),
        at_ms: quiet_since,
    };
    for command in std::iter::once(start).chain(commands) {
        for event in run.decide(command).expect("a command the run accepts") {
            run.apply(&event);
            log.push(event);
        }
    }
    let view = RunView {
        id: id.clone(),
        thread_id: thread,
        status: run.status,
        event_count: run.emitted.len() as i64,
        updated_at_ms: quiet_since,
    };
    h.store
        .append_run(&id, 0, &log, &view, Some(&h.account))
        .await
        .expect("append the interrupted run");
    id
}

fn said(text: &str) -> RunCommand {
    RunCommand::Emit {
        payload: json!({ "type": "TEXT_MESSAGE_CONTENT", "messageId": "m-said", "delta": text }),
        at_ms: now_ms(),
    }
}

/// Sweep until THIS run leaves `running`, then give its continuation time to finish. The sweep
/// claims a bounded batch of whatever is abandoned database-wide, so one sweep is not enough.
async fn settle(h: &Harness, run_id: &RunId) -> Run {
    for _ in 0..80 {
        opengrok_server::recovery::sweep_once(&h.host)
            .await
            .expect("sweep");
        let (run, _) = h.store.load_run(run_id).await.expect("load");
        if run.status.is_terminal() {
            return run;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    let (run, _) = h.store.load_run(run_id).await.expect("load");
    let types: Vec<String> =
        sqlx::query("select event_type from events where stream_id = $1 order by stream_seq")
            .bind(opengrok_store::run_stream(run_id))
            .fetch_all(h.store.pool())
            .await
            .expect("events")
            .into_iter()
            .map(|row| row.try_get::<String, _>("event_type").unwrap_or_default())
            .collect();
    panic!(
        "the run never settled: {:?} generation {}, events {types:?}, ran {:?}",
        run.status,
        run.generation,
        h.stub.ran.lock().expect("ran")
    );
}

fn email(tag: &str) -> String {
    format!(
        "interrupted-{tag}-{}@og.local",
        uuid::Uuid::now_v7().simple()
    )
}

/// THE ONE #91 IS FOR. A run interrupted between two steps — nothing started that the log does
/// not show — is carried on in its next generation and finishes, where it used to be failed.
#[tokio::test]
async fn a_run_interrupted_between_steps_is_carried_on_and_finishes() {
    let database_url = database_or_skip!();
    let _sweeper = one_sweeper(&database_url).await;
    let h = harness(&database_url, &email("fresh")).await;
    let run_id = interrupted_run(&h, vec![said("Reading the inbox now.")]).await;

    let run = settle(&h, &run_id).await;
    assert_eq!(
        run.status,
        RunStatus::Finished,
        "carried on, not failed: {:?}",
        run.failure
    );
    assert_eq!(run.generation, 1, "in its next generation");
    let spoken: String = run
        .emitted
        .iter()
        .filter(|frame| frame.get("type").and_then(Value::as_str) == Some("TEXT_MESSAGE_CONTENT"))
        .filter_map(|frame| frame.get("delta").and_then(Value::as_str))
        .collect();
    assert!(
        spoken.contains("summarise the inbox"),
        "the model was asked again with what the person asked: {spoken}"
    );
}

/// A tool whose start is on record without its result may have acted: the run is failed, and
/// the reason names the tool and does not claim to know whether it ran.
#[tokio::test]
async fn a_run_interrupted_with_a_tool_open_fails_naming_the_tool() {
    let database_url = database_or_skip!();
    let _sweeper = one_sweeper(&database_url).await;
    let h = harness(&database_url, &email("open-tool")).await;
    let run_id = interrupted_run(
        &h,
        vec![RunCommand::StartTools {
            tools: vec![StartedTool {
                call_id: "call-rm".to_string(),
                tool: "shell".to_string(),
            }],
            at_ms: now_ms(),
        }],
    )
    .await;

    let run = settle(&h, &run_id).await;
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.generation, 0, "never carried on");
    let reason = run.failure.unwrap_or_default();
    assert!(reason.contains("shell"), "names the tool: {reason}");
    assert!(
        reason.contains("unknown"),
        "does not claim to know if it ran: {reason}"
    );
}

/// A run that keeps being interrupted stops being carried on: after two resumes, the third
/// interruption fails it.
#[tokio::test]
async fn a_run_carried_on_twice_fails_the_third_time() {
    let database_url = database_or_skip!();
    let _sweeper = one_sweeper(&database_url).await;
    let h = harness(&database_url, &email("twice")).await;
    let resume = || RunCommand::Resume {
        reason: "interrupted by a restart".to_string(),
        at_ms: now_ms(),
    };
    let run_id = interrupted_run(&h, vec![resume(), resume()]).await;

    let run = settle(&h, &run_id).await;
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.generation, 2, "not carried on a third time");
    let reason = run.failure.unwrap_or_default();
    assert!(reason.contains("carried on 2 times"), "{reason}");
}

/// AN ANSWER IS CARRIED OUT, NOT ASKED AGAIN. A person answered a card and the process died
/// before the call started: the resume runs that call, through the answer's own path, so the
/// person does not get a second card for something they already approved.
#[tokio::test]
async fn an_answer_whose_call_never_started_is_carried_out() {
    let database_url = database_or_skip!();
    let _sweeper = one_sweeper(&database_url).await;
    let h = harness(&database_url, &email("answered")).await;
    let run_id = interrupted_run(
        &h,
        vec![
            RunCommand::Suspend {
                call_id: "call-ls".to_string(),
                tool: "shell".to_string(),
                arguments: json!({ "command": "ls" }),
                reason: SuspendReason::default(),
                at_ms: now_ms(),
            },
            RunCommand::Answer {
                call_id: "call-ls".to_string(),
                approved: true,
                by: "the person".to_string(),
                at_ms: now_ms(),
            },
        ],
    )
    .await;

    let run = settle(&h, &run_id).await;
    assert_eq!(
        run.generation, 1,
        "carried on: {:?} {:?}",
        run.status, run.failure
    );
    assert!(run.unstarted_answer.is_none(), "the answer was carried out");
    let started: Vec<Vec<StartedTool>> = sqlx::query(
        "select payload from events where stream_id = $1 and event_type = 'run-tool-started'",
    )
    .bind(opengrok_store::run_stream(&run_id))
    .fetch_all(h.store.pool())
    .await
    .expect("events")
    .into_iter()
    .filter_map(|row| {
        let payload: Value = row.try_get("payload").ok()?;
        match serde_json::from_value::<RunEvent>(payload).ok()? {
            RunEvent::ToolStarted { tools, .. } => Some(tools),
            _ => None,
        }
    })
    .collect();
    assert!(
        started
            .iter()
            .flatten()
            .any(|tool| tool.call_id == "call-ls"),
        "the answered call itself was started, not re-asked under a new id: {started:?}"
    );
    assert!(
        h.stub
            .ran
            .lock()
            .expect("ran")
            .iter()
            .any(|command| command.contains("ls")),
        "and it ran on the coworker's computer"
    );
}
