//! An org's run ceiling, and a routine's own limits under it.
//!
//! WHAT AN ADMIN SETS IS A CEILING. It binds every run by the org's coworkers — a chat as much as
//! a routine — and a routine may ask for less, never more. A routine saved under a ceiling that
//! was lowered later is held to the new one when it runs, and a run that waits on a card keeps
//! the limits it started with when the card is answered, whatever the ceiling says by then.
//!
//! The model here never stops: while a tool is on offer it asks for one, so every run in this
//! file ends on its budget, and the count of rounds is the limit it was held to.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use opengrok_box::{BoxResult, CommandOutput, Computer, StartedCommand};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, CoworkerId, OrgId, RunId};
use opengrok_core::limits::RunLimits;
use opengrok_core::org::{Org, OrgCommand};
use opengrok_core::run::{Run, RunCommand, RunView};
use opengrok_harness::{DeltaStream, ModelDelta, ModelDoor, ModelError, ModelRequest};
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

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// A model that never stops while a tool is on offer: `shell` first, then `then` every round
/// after, and words only on the wrap-up call, which offers none.
struct EndlessToolDoor {
    then: &'static str,
    calls: Mutex<usize>,
}

impl EndlessToolDoor {
    fn calls(&self) -> usize {
        *self.calls.lock().unwrap()
    }
}

#[async_trait]
impl ModelDoor for EndlessToolDoor {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
        let call = {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            *calls
        };
        let script = if request.tools.is_empty() {
            vec![ModelDelta::Text(
                "I kept at it until this run's limit.".to_string(),
            )]
        } else {
            let (name, arguments) = match (call, self.then) {
                (1, _) | (_, "shell") => ("shell", json!({ "command": format!("echo {call}") })),
                (_, tool) => (tool, json!({ "path": format!("/tmp/notes-{call}.txt") })),
            };
            let id = format!("call_{call}");
            vec![
                ModelDelta::ToolCallStart {
                    id: id.clone(),
                    name: name.to_string(),
                },
                ModelDelta::ToolCallArgs {
                    id: id.clone(),
                    delta: arguments.to_string(),
                },
                ModelDelta::ToolCallEnd { id },
            ]
        };
        Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
    }
}

/// A computer on which every command succeeds.
struct StubComputer;

#[async_trait]
impl Computer for StubComputer {
    async fn create(&self, _ttl_seconds: Option<u64>) -> BoxResult<String> {
        Ok(format!("bx_stub_{}", uuid::Uuid::now_v7().simple()))
    }
    async fn run(&self, _box: &str, _command: &str, _timeout: u32) -> BoxResult<CommandOutput> {
        Ok(CommandOutput {
            exit_code: 0,
            stdout: "ran".to_string(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            timed_out: false,
        })
    }
    async fn start(&self, _box_id: &str, _command: &str) -> BoxResult<StartedCommand> {
        self.watch("", "").await
    }
    async fn watch(&self, _box_id: &str, _process_id: &str) -> BoxResult<StartedCommand> {
        Ok(StartedCommand {
            process_id: "p1".to_string(),
            running: false,
            stdout: "ran".to_string(),
            stderr: String::new(),
            exit_code: Some(0),
        })
    }
    async fn read_file(&self, _box_id: &str, _path: &str) -> BoxResult<String> {
        Ok("a note".to_string())
    }
    async fn write_file(&self, _box_id: &str, _path: &str, _content: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn expose_port(&self, _box_id: &str, _port: u16, _title: &str) -> BoxResult<String> {
        Ok("http://stub.invalid".to_string())
    }
    async fn stop(&self, _box_id: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn resume(&self, _box_id: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn destroy(&self, _box_id: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn state(&self, _box_id: &str) -> BoxResult<String> {
        Ok("running".to_string())
    }
}

async fn seed_account(store: &PgStore, email: &str, org_id: &OrgId) -> AccountId {
    let id = AccountId::new();
    let at_ms = now_ms();
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.to_string(),
            password_hash: "x".to_string(),
            first_name: "Ada".to_string(),
            last_name: String::new(),
            org_id: org_id.as_str().to_string(),
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
        password_hash: Some("x".to_string()),
        first_name: "Ada".to_string(),
        last_name: String::new(),
        org_id: Some(org_id.as_str().to_string()),
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
    host: HostState,
    store: PgStore,
    door: Arc<EndlessToolDoor>,
    client: reqwest::Client,
    /// The org's admin, and a member who is not.
    admin: String,
    member: String,
    member_id: AccountId,
}

/// A server with a never-ending model, and an org of two: its admin and a member.
async fn harness(database_url: &str, then: &'static str) -> Harness {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let org_id = OrgId::new();
    let domain = format!("limits-{}.test", uuid::Uuid::now_v7().simple());
    let (admin_email, member_email) = (format!("admin@{domain}"), format!("member@{domain}"));
    let admin_id = seed_account(&store, &admin_email, &org_id).await;
    let member_id = seed_account(&store, &member_email, &org_id).await;
    let events = Org::default()
        .decide(OrgCommand::Create {
            name: "Acme".to_string(),
            admin: admin_id.clone(),
            domains: vec![domain],
            at_ms: now_ms(),
        })
        .expect("create org");
    store
        .append_org(&org_id, 0, &events, &Org::replay(&events), now_ms())
        .await
        .expect("append org");

    let door = Arc::new(EndlessToolDoor {
        then,
        calls: Mutex::new(0),
    });
    let auth = AuthState::new(
        store.clone(),
        Arc::new(TokenMinter::new(b"run-limits-secret")),
        admin_email.clone(),
    );
    let agui = AgUiState {
        auth,
        door: door.clone(),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer: Some(Arc::new(StubComputer)),
        vault: None,
        connectors: Connectors {
            providers: Arc::new(BTreeMap::new()),
            redirect_uri: "http://127.0.0.1/callback".to_string(),
        },
        plugins: Arc::new(BTreeMap::new()),
        host_settings: None,
    };
    let token = |id: &AccountId, email: &str| {
        agui.auth
            .minter
            .mint_access(
                id.as_str(),
                "sess-test",
                email,
                "ultra",
                now_ms() / 1000,
                3600,
            )
            .expect("mint access")
    };
    let (admin, member) = (
        token(&admin_id, &admin_email),
        token(&member_id, &member_email),
    );
    let gateway = HostState::new(agui.clone(), Some("http://opengrok.lan:1447".to_string()));
    let app = opengrok_server::router(agui.clone(), gateway.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    Harness {
        base: format!("http://127.0.0.1:{}", addr.port()),
        host: gateway,
        store,
        door,
        client: reqwest::Client::new(),
        admin,
        member,
        member_id,
    }
}

impl Harness {
    async fn send(&self, method: &str, path: &str, token: &str, body: Value) -> (u16, Value) {
        let url = format!("{}{path}", self.base);
        let request = match method {
            "GET" => self.client.get(url),
            "PUT" => self.client.put(url).json(&body),
            "PATCH" => self.client.patch(url).json(&body),
            _ => self.client.post(url).json(&body),
        };
        let reply = request
            .header("authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("request");
        let status = reply.status().as_u16();
        let text = reply.text().await.expect("body");
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn set_ceiling(&self, ceiling: Value) {
        let (status, reply) = self
            .send("PUT", "/admin/run-limits", &self.admin, ceiling)
            .await;
        assert_eq!(status, 200, "{reply}");
    }

    /// A coworker of the member's own, on the stub computer.
    async fn hire(&self) -> String {
        let (status, hired) = self
            .send("POST", "/coworkers", &self.member, json!({ "name": "Ada" }))
            .await;
        assert_eq!(status, 201, "{hired}");
        hired["id"].as_str().expect("coworker id").to_string()
    }

    /// One chat turn, as NativeChat sends it; the whole stream is read, so the turn has ended
    /// (or parked) when this returns.
    async fn chat(&self, agent: &str) -> RunId {
        let run_id = uuid::Uuid::now_v7().to_string();
        let reply = self
            .client
            .post(format!("{}/ag-ui", self.base))
            .header("authorization", format!("Bearer {}", self.member))
            .json(&json!({
                "threadId": format!("thr-{}", uuid::Uuid::now_v7()),
                "runId": run_id,
                "messages": [{ "id": "m1", "role": "user", "content": "keep going" }],
                "forwardedProps": { "coworkerId": agent },
            }))
            .send()
            .await
            .expect("ag-ui turn");
        assert_eq!(reply.status().as_u16(), 200, "ag-ui turn status");
        reply.text().await.expect("sse");
        RunId::from_stored(run_id)
    }

    async fn ended(&self, run_id: &RunId) -> Run {
        for _ in 0..150 {
            let (run, _) = self.store.load_run(run_id).await.expect("run");
            if run.status.is_terminal() {
                return run;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("run {run_id} did not end in 15s");
    }
}

fn frames_of<'a>(run: &'a Run, kind: &str) -> Vec<&'a Value> {
    run.emitted
        .iter()
        .filter(|frame| frame["type"] == kind)
        .collect()
}

/// The budget the run's own record says it was held to, from its last `run-timing` frame.
fn timed_budget(run: &Run) -> Value {
    frames_of(run, "CUSTOM")
        .into_iter()
        .rev()
        .find(|frame| frame["name"] == "run-timing")
        .map(|frame| frame["value"]["budget"].clone())
        .expect("a run-timing frame")
}

#[tokio::test]
async fn the_org_admin_sets_the_ceiling_and_nobody_else_may() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, "shell").await;

    let (status, unset) = h
        .send("GET", "/admin/run-limits", &h.admin, Value::Null)
        .await;
    assert_eq!(status, 200, "{unset}");
    assert_eq!(unset["maxRounds"], Value::Null, "no ceiling yet: {unset}");
    assert_eq!(unset["serverLimits"]["maxRounds"], 8, "{unset}");

    let ceiling = json!({ "maxRounds": 3, "maxComputerRounds": null, "maxWallMs": 60000 });
    let (status, set) = h.send("PUT", "/admin/run-limits", &h.admin, ceiling).await;
    assert_eq!(status, 200, "{set}");
    assert_eq!(set["maxRounds"], 3, "{set}");
    assert_eq!(set["maxComputerRounds"], Value::Null, "{set}");
    let (_, read) = h
        .send("GET", "/admin/run-limits", &h.admin, Value::Null)
        .await;
    assert_eq!(read["maxRounds"], 3, "the ceiling was kept: {read}");
    assert_eq!(read["maxWallMs"], 60000, "{read}");

    // A member is not the admin: neither reading nor setting is theirs, and nothing changes.
    for (method, body) in [("PUT", json!({ "maxRounds": 8 })), ("GET", Value::Null)] {
        let (status, refused) = h.send(method, "/admin/run-limits", &h.member, body).await;
        assert_eq!(status, 403, "{method}: {refused}");
    }
    let (_, read) = h
        .send("GET", "/admin/run-limits", &h.admin, Value::Null)
        .await;
    assert_eq!(
        read["maxRounds"], 3,
        "the member's write did not land: {read}"
    );

    // Zero, nonsense and a misspelt limit are refused with a sentence, and change nothing.
    for (sent, says) in [
        (
            json!({ "maxRounds": 0 }),
            "maxRounds must be a whole number",
        ),
        (json!({ "maxRounds": 9 }), "from 1 to 8"),
        (json!({ "maxRound": 3 }), "maxRound is not a run limit"),
    ] {
        let (status, refused) = h.send("PUT", "/admin/run-limits", &h.admin, sent).await;
        assert_eq!(status, 422, "{refused}");
        assert!(
            refused["error"]
                .as_str()
                .is_some_and(|why| why.contains(says)),
            "{refused}"
        );
    }
    let (_, read) = h
        .send("GET", "/admin/run-limits", &h.admin, Value::Null)
        .await;
    assert_eq!(
        read["maxRounds"], 3,
        "a refused write changed nothing: {read}"
    );
}

#[tokio::test]
async fn a_routine_above_the_ceiling_is_refused_naming_it() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, "shell").await;
    h.set_ceiling(json!({ "maxRounds": 2 })).await;
    let agent = h.hire().await;
    let routine = |limits: Value| {
        json!({
            "coworkerId": agent,
            "prompt": "keep going",
            "cron": "0 9 * * 1",
            "runLimits": limits,
        })
    };

    let (status, refused) = h
        .send(
            "POST",
            "/schedules",
            &h.member,
            routine(json!({ "maxRounds": 3 })),
        )
        .await;
    assert_eq!(status, 422, "{refused}");
    let why = refused["error"].as_str().unwrap_or_default();
    assert!(
        why.contains("maxRounds 3") && why.contains("ceiling of 2"),
        "the refusal names the ceiling: {refused}"
    );
    let (status, refused) = h
        .send(
            "POST",
            "/schedules",
            &h.member,
            routine(json!({ "maxRounds": 0 })),
        )
        .await;
    assert_eq!(status, 422, "zero is refused too: {refused}");

    // Under the ceiling is kept, and read back on the row.
    let (status, created) = h
        .send(
            "POST",
            "/schedules",
            &h.member,
            routine(json!({ "maxRounds": 1, "maxWallMs": 60000 })),
        )
        .await;
    assert_eq!(status, 201, "{created}");
    assert_eq!(created["runLimits"]["maxRounds"], 1, "{created}");
    let id = created["id"].as_str().expect("routine id").to_string();
    let (_, listed) = h.send("GET", "/schedules", &h.member, Value::Null).await;
    let row = listed
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["id"] == id.as_str()))
        .expect("the routine is listed");
    assert_eq!(
        row["runLimits"],
        json!({ "maxRounds": 1, "maxComputerRounds": null, "maxWallMs": 60000 })
    );

    // An edit is held to the same ceiling, and one that leaves limits out keeps them.
    let path = format!("/schedules/{id}");
    let (status, refused) = h
        .send(
            "PATCH",
            &path,
            &h.member,
            json!({ "runLimits": { "maxRounds": 5 } }),
        )
        .await;
    assert_eq!(status, 422, "{refused}");
    assert!(
        refused["error"]
            .as_str()
            .is_some_and(|why| why.contains("ceiling of 2")),
        "{refused}"
    );
    let (status, renamed) = h
        .send("PATCH", &path, &h.member, json!({ "name": "renamed" }))
        .await;
    assert_eq!(status, 200, "{renamed}");
    assert_eq!(renamed["runLimits"]["maxRounds"], 1, "{renamed}");
}

/// A routine saved under a ceiling that was lowered afterwards runs under the lower one.
#[tokio::test]
async fn a_routine_run_ends_at_the_org_ceiling() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, "shell").await;
    h.set_ceiling(json!({ "maxRounds": 3 })).await;
    let agent = h.hire().await;
    let (status, created) = h
        .send(
            "POST",
            "/schedules",
            &h.member,
            json!({ "coworkerId": agent, "prompt": "keep going", "cron": "0 9 * * 1",
                    "runLimits": { "maxRounds": 3 } }),
        )
        .await;
    assert_eq!(status, 201, "{created}");
    h.set_ceiling(json!({ "maxRounds": 2 })).await;

    let id = created["id"].as_str().expect("routine id");
    let (status, fired) = h
        .send(
            "POST",
            &format!("/schedules/{id}/run"),
            &h.member,
            Value::Null,
        )
        .await;
    assert_eq!(status, 202, "{fired}");
    let run = h
        .ended(&RunId::from_stored(
            fired["runId"].as_str().expect("run id").to_string(),
        ))
        .await;

    assert_eq!(
        frames_of(&run, "TOOL_CALL_START").len(),
        2,
        "two rounds, the lowered ceiling, not the routine's three"
    );
    assert_eq!(h.door.calls(), 3, "two rounds, then the wrap-up");
    assert_eq!(timed_budget(&run)["max_rounds"], 2);
    assert_eq!(
        run.limits.max_rounds.map(|rounds| rounds.get()),
        Some(2),
        "and the run captured it"
    );
}

#[tokio::test]
async fn a_chat_run_ends_at_the_org_ceiling() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, "shell").await;
    h.set_ceiling(json!({ "maxRounds": 2 })).await;
    let agent = h.hire().await;

    let run = h.ended(&h.chat(&agent).await).await;

    assert_eq!(
        frames_of(&run, "TOOL_CALL_START").len(),
        2,
        "two rounds, then the run wraps up"
    );
    assert_eq!(h.door.calls(), 3, "two rounds, then the wrap-up");
    assert_eq!(timed_budget(&run)["max_rounds"], 2);
    // Captured whole: the org's ceiling where it set one, the server's budget where it did not.
    let limits = serde_json::to_value(run.limits).expect("limits");
    assert_eq!(
        limits,
        json!({ "max_rounds": 2, "max_computer_rounds": 24, "max_wall_ms": 900000 })
    );
}

/// The limits a run started with bind its continuation after a card, even once the ceiling is
/// lifted: an answer must not buy a run a budget it never had.
#[tokio::test]
async fn a_card_approved_resume_keeps_the_ceiling_it_started_under() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, "read_file").await;
    h.set_ceiling(json!({ "maxRounds": 2 })).await;
    let agent = h.hire().await;
    let (status, set) = h
        .send(
            "POST",
            &format!("/coworkers/{agent}/approvals"),
            &h.member,
            json!({ "tools": ["shell"] }),
        )
        .await;
    assert_eq!(status, 200, "{set}");

    let run_id = h.chat(&agent).await;
    let (parked, _) = h.store.load_run(&run_id).await.expect("run");
    let call_id = parked
        .pending
        .as_ref()
        .expect("the shell call waits on the card")
        .call_id
        .clone();
    assert_eq!(parked.limits.max_rounds.map(|rounds| rounds.get()), Some(2));

    h.set_ceiling(json!({})).await;
    let asked_before = h.door.calls();
    let (status, answered) = h
        .send(
            "POST",
            &format!("/ag-ui/runs/{}/answer", run_id.as_str()),
            &h.member,
            json!({ "call_id": call_id, "approved": true }),
        )
        .await;
    assert_eq!(status, 200, "{answered}");
    let run = h.ended(&run_id).await;

    assert_eq!(
        h.door.calls() - asked_before,
        3,
        "two rounds after the card, then the wrap-up: the captured ceiling, not the server's"
    );
    assert_eq!(timed_budget(&run)["max_rounds"], 2);
    assert!(
        h.store
            .awaiting_approval(&h.member_id)
            .await
            .expect("awaiting")
            .is_empty(),
        "nothing else is waiting"
    );
}

/// A run the sweep carries on after a restart is held to the limits it started with, too.
#[tokio::test]
async fn a_run_carried_on_after_a_restart_keeps_the_limits_it_started_with() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, "shell").await;
    let agent = h.hire().await;
    // As a process that died between two steps left it: started under a ceiling of two rounds,
    // which nobody has set since, and quiet for longer than a lease.
    let quiet_since = now_ms() - 3 * opengrok_server::recovery::LEASE_MS;
    let (run_id, thread) = (RunId::new(), format!("thr-{}", uuid::Uuid::now_v7()));
    let started = Run::default()
        .decide(RunCommand::Start {
            thread_id: thread.clone(),
            coworker_id: Some(CoworkerId::from_stored(agent)),
            model: Some("oag/cheap".to_string()),
            system: None,
            skill_id: None,
            prompt: Some(vec![
                json!({ "id": "m1", "role": "user", "content": "keep going" }),
            ]),
            limits: RunLimits {
                max_rounds: std::num::NonZeroU32::new(2),
                ..RunLimits::default()
            },
            at_ms: quiet_since,
        })
        .expect("start");
    let view = RunView {
        id: run_id.clone(),
        thread_id: thread,
        status: Run::replay(&started).status,
        event_count: 0,
        updated_at_ms: quiet_since,
    };
    h.store
        .append_run(&run_id, 0, &started, &view, Some(&h.member_id))
        .await
        .expect("append the interrupted run");

    // Swept until it is claimed: a sweep takes a bounded batch of whatever this database holds
    // abandoned, oldest first, and a run an earlier test left behind may come first.
    for _ in 0..50 {
        opengrok_server::recovery::sweep_once(&h.host)
            .await
            .expect("sweep");
        let (run, _) = h.store.load_run(&run_id).await.expect("run");
        if run.generation > 0 || run.status.is_terminal() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let run = h.ended(&run_id).await;

    assert_eq!(
        run.generation, 1,
        "carried on, not failed: {:?}",
        run.failure
    );
    assert_eq!(h.door.calls(), 3, "two rounds, then the wrap-up");
    assert_eq!(timed_budget(&run)["max_rounds"], 2);
}
