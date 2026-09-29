//! How hard a coworker thinks (#271): its owner sets it on `PATCH /coworkers/{id}`, every coworker
//! row a client reads carries it, each turn sends it to the gateway as `reasoning_effort`, and a
//! run keeps the effort it started with when it is carried on — after a card, or after a restart.
//!
//! The words are the gateway's own (`oag-proto` `Effort::as_str`) plus `inherit`, which sends
//! nothing, agreed with NativeChat. What reaches the gateway's JSON body is the harness door's to
//! render (`gateway_tests.rs`); these assert what the server asks that door for, through a door
//! that records every request. Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use opengrok_box::{BoxResult, CommandOutput, Computer, StartedCommand};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::coworker::Effort;
use opengrok_core::id::{AccountId, CoworkerId, RunId};
use opengrok_core::run::{Run, RunCommand, RunStatus, RunView};
use opengrok_harness::{DeltaStream, MockDoor, ModelDoor, ModelError, ModelRequest};
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

/// The sentence a word outside the list is refused with, as agreed with the client.
const REFUSAL: &str = "effort must be one of inherit, none, low, medium, high, xhigh, max";

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::now_v7().simple())
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Whose turn and at what effort, for every request the model door was asked, in order. Whose,
/// because the sweep carries on ANY abandoned run in this binary's database through this door,
/// and one left behind by an earlier, killed run of these tests must not count as ours.
struct RecordingDoor {
    inner: MockDoor,
    asked: Mutex<Vec<(Option<String>, Effort)>>,
}

#[async_trait]
impl ModelDoor for RecordingDoor {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
        self.asked
            .lock()
            .expect("asked")
            .push((request.spend_scope.clone(), request.effort));
        self.inner.stream(request).await
    }
}

/// A computer that runs whatever it is given, so a shell card can be answered and carried on.
struct StubComputer;

#[async_trait]
impl Computer for StubComputer {
    async fn create(&self, _ttl_seconds: Option<u64>) -> BoxResult<String> {
        Ok(format!("bx_effort_{}", uuid::Uuid::now_v7().simple()))
    }
    async fn run(&self, _b: &str, _command: &str, _t: u32) -> BoxResult<CommandOutput> {
        Ok(CommandOutput {
            exit_code: 0,
            stdout: "ran".to_string(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            timed_out: false,
        })
    }
    async fn start(&self, _b: &str, _command: &str) -> BoxResult<StartedCommand> {
        Ok(StartedCommand {
            process_id: "p1".to_string(),
            running: false,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
        })
    }
    async fn watch(&self, b: &str, _p: &str) -> BoxResult<StartedCommand> {
        self.start(b, "").await
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
}

struct Person {
    id: AccountId,
    token: String,
}

struct Harness {
    base: String,
    client: reqwest::Client,
    store: PgStore,
    host: HostState,
    minter: Arc<TokenMinter>,
    door: Arc<RecordingDoor>,
}

async fn harness(database_url: &str, door: MockDoor) -> Harness {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let minter = Arc::new(TokenMinter::new(b"effort-test-secret"));
    let auth = AuthState::new(store.clone(), minter.clone(), "owner@og.local".to_string());
    let door = Arc::new(RecordingDoor {
        inner: door,
        asked: Mutex::new(Vec::new()),
    });
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
        host,
        minter,
        door,
    }
}

impl Harness {
    /// A signed-in person, in `org` or in none.
    async fn person(&self, org: Option<&str>) -> Person {
        let id = AccountId::new();
        let email = format!("{}@og.local", unique("effort"));
        let at_ms = now_ms();
        let events = Account::default()
            .decide(AccountCommand::Register {
                email: email.clone(),
                password_hash: "x".to_string(),
                first_name: "Test".to_string(),
                last_name: "User".to_string(),
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
            first_name: "Test".to_string(),
            last_name: "User".to_string(),
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
                "sess-effort",
                &email,
                "ultra",
                chrono::Utc::now().timestamp(),
                3600,
            )
            .expect("mint access");
        Person { id, token }
    }

    async fn send(
        &self,
        who: &Person,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base))
            .header("Authorization", format!("Bearer {}", who.token));
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

    /// The whole hire reply.
    async fn hire(&self, who: &Person, name: &str) -> Value {
        let (status, body) = self
            .send(
                who,
                reqwest::Method::POST,
                "/coworkers",
                Some(json!({ "name": name })),
            )
            .await;
        assert_eq!(status, 201, "hire {name}: {body}");
        body
    }

    async fn patch(&self, who: &Person, id: &str, body: Value) -> (u16, Value) {
        self.send(
            who,
            reqwest::Method::PATCH,
            &format!("/coworkers/{id}"),
            Some(body),
        )
        .await
    }

    /// The roster row the app lists: the projection's answer, not the reply's.
    async fn row(&self, who: &Person, id: &str) -> Value {
        let (status, listed) = self
            .send(who, reqwest::Method::GET, "/coworkers", None)
            .await;
        assert_eq!(status, 200, "{listed}");
        listed
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["id"] == id).cloned())
            .unwrap_or(Value::Null)
    }

    /// One turn on the AG-UI door, as the app sends it; the reply is the whole stream.
    async fn turn(&self, who: &Person, id: &str) -> String {
        let res = self
            .client
            .post(format!("{}/ag-ui", self.base))
            .header("Authorization", format!("Bearer {}", who.token))
            .json(&json!({
                "threadId": unique("thr"),
                "runId": uuid::Uuid::now_v7().to_string(),
                "messages": [{ "id": unique("m"), "role": "user", "content": "run a command" }],
                "forwardedProps": { "coworkerId": id },
            }))
            .send()
            .await
            .expect("ag-ui turn");
        assert_eq!(res.status().as_u16(), 200, "ag-ui turn status");
        res.text().await.expect("sse")
    }

    /// The effort each of this coworker's turns asked the door for, in order.
    fn efforts(&self, coworker: &str) -> Vec<Effort> {
        self.door
            .asked
            .lock()
            .expect("asked")
            .iter()
            .filter(|(scope, _)| scope.as_deref() == Some(coworker))
            .map(|(_, effort)| *effort)
            .collect()
    }

    /// The run this person is waiting on, and the call its card is for.
    async fn wait_for_pending(&self, who: &Person) -> (RunId, String) {
        for _ in 0..100 {
            for id in self
                .store
                .awaiting_approval(&who.id)
                .await
                .expect("awaiting")
            {
                if let Ok((run, _)) = self.store.load_run(&id).await
                    && let Some(pending) = run.pending.as_ref()
                {
                    return (id, pending.call_id.clone());
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("no run suspended for an approval in 10s");
    }

    async fn wait_for_ending(&self, run_id: &RunId) -> Run {
        for _ in 0..100 {
            let (run, _) = self.store.load_run(run_id).await.expect("run");
            if run.status.is_terminal() {
                return run;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("the run did not end in 10s");
    }
}

#[tokio::test]
async fn an_owner_sets_how_hard_a_coworker_thinks_and_every_row_says_so() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, MockDoor::echoing()).await;
    let owner = h.person(None).await;

    // A coworker nobody has chosen an effort for inherits its route's, and every row says so:
    // the hire reply and the roster, as a word, never a null or a missing key.
    let hired = h.hire(&owner, "Ada").await;
    assert_eq!(hired["effort"], "inherit", "{hired}");
    let ada = hired["id"].as_str().expect("id").to_string();
    assert_eq!(h.row(&owner, &ada).await["effort"], "inherit");

    let (status, patched) = h.patch(&owner, &ada, json!({ "effort": "high" })).await;
    assert_eq!(status, 200, "an effort alone is a change: {patched}");
    assert_eq!(patched["effort"], "high", "the reply says so: {patched}");
    assert_eq!(
        patched["name"], "Ada",
        "and changes nothing else: {patched}"
    );
    let row = h.row(&owner, &ada).await;
    assert_eq!(row["effort"], "high", "and so does the roster: {row}");
    assert_eq!(row, patched, "the PATCH reply is the roster row");

    // Every word the gateway speaks is one a coworker can be set to.
    for word in ["none", "low", "medium", "xhigh", "max"] {
        let (status, patched) = h.patch(&owner, &ada, json!({ "effort": word })).await;
        assert_eq!(status, 200, "{word}: {patched}");
        assert_eq!(h.row(&owner, &ada).await["effort"], word);
    }

    // A fresh coworker beside it is still on its route's default.
    let bob = h.hire(&owner, "Bob").await;
    let bob = bob["id"].as_str().expect("id");
    assert_eq!(h.row(&owner, bob).await["effort"], "inherit");
}

#[tokio::test]
async fn an_unknown_effort_is_refused_and_nothing_in_the_patch_is_applied() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, MockDoor::echoing()).await;
    let owner = h.person(None).await;
    let hired = h.hire(&owner, "Ada").await;
    let ada = hired["id"].as_str().expect("id").to_string();
    assert_eq!(
        h.patch(&owner, &ada, json!({ "effort": "low" })).await.0,
        200
    );

    // THE BODY IS ONE DECISION. A rename sent beside an effort we cannot send is not half-done:
    // a 400 that had renamed the coworker anyway would be a failure that changed something.
    for wrong in [
        json!("loud"),
        json!("minimal"),
        json!("HIGH"),
        json!(""),
        json!(7),
    ] {
        let (status, refused) = h
            .patch(
                &owner,
                &ada,
                json!({ "name": "Greendale", "effort": wrong, "role": "Keep it short." }),
            )
            .await;
        assert_eq!(status, 400, "{wrong}: {refused}");
        assert_eq!(refused, json!({ "error": REFUSAL }), "{wrong}");
    }
    let row = h.row(&owner, &ada).await;
    assert_eq!(
        row["name"], "Ada",
        "the name beside it was not applied: {row}"
    );
    assert_eq!(row["role"], Value::Null, "nor the role: {row}");
    assert_eq!(row["effort"], "low", "and the stored effort stands: {row}");
}

#[tokio::test]
async fn null_or_inherit_puts_a_coworker_back_on_its_routes_default() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, MockDoor::echoing()).await;
    let owner = h.person(None).await;
    let hired = h.hire(&owner, "Ada").await;
    let ada = hired["id"].as_str().expect("id").to_string();

    for reset in [Value::Null, json!("inherit")] {
        assert_eq!(
            h.patch(&owner, &ada, json!({ "effort": "max" })).await.0,
            200
        );
        assert_eq!(h.row(&owner, &ada).await["effort"], "max");
        let (status, patched) = h.patch(&owner, &ada, json!({ "effort": reset })).await;
        assert_eq!(status, 200, "{reset}: {patched}");
        assert_eq!(patched["effort"], "inherit", "{reset}: {patched}");
        assert_eq!(h.row(&owner, &ada).await["effort"], "inherit", "{reset}");
    }

    // Absent is not null: a PATCH that does not name the effort leaves it where it is.
    assert_eq!(
        h.patch(&owner, &ada, json!({ "effort": "low" })).await.0,
        200
    );
    let (status, patched) = h.patch(&owner, &ada, json!({ "name": "Grace" })).await;
    assert_eq!(status, 200, "{patched}");
    assert_eq!(patched["effort"], "low", "{patched}");
}

#[tokio::test]
async fn only_the_owner_can_change_a_shared_coworkers_effort() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, MockDoor::echoing()).await;
    let org = unique("org");
    let owner = h.person(Some(&org)).await;
    let member = h.person(Some(&org)).await;
    let hired = h.hire(&owner, "Ada").await;
    let ada = hired["id"].as_str().expect("id").to_string();
    assert_eq!(
        h.patch(&owner, &ada, json!({ "visibility": "org" }))
            .await
            .0,
        200
    );
    assert_eq!(
        h.row(&member, &ada).await["effort"],
        "inherit",
        "a shared row carries it too"
    );

    let (status, refused) = h.patch(&member, &ada, json!({ "effort": "high" })).await;
    assert_eq!(status, 403, "{refused}");
    assert!(
        refused["error"]
            .as_str()
            .unwrap_or_default()
            .starts_with("only the person who hired this coworker can change it"),
        "a sentence, as JSON: {refused}"
    );
    assert_eq!(h.row(&owner, &ada).await["effort"], "inherit", "unchanged");
}

#[tokio::test]
async fn a_turn_sends_the_coworkers_effort_and_inherit_sends_none() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, MockDoor::echoing()).await;
    let owner = h.person(None).await;
    let hired = h.hire(&owner, "Ada").await;
    let ada = hired["id"].as_str().expect("id").to_string();
    assert_eq!(
        h.patch(&owner, &ada, json!({ "effort": "high" })).await.0,
        200
    );
    h.turn(&owner, &ada).await;

    let hired = h.hire(&owner, "Bob").await;
    let bob = hired["id"].as_str().expect("id").to_string();
    h.turn(&owner, &bob).await;

    // What the gateway door then sends: `reasoning_effort: "high"` for Ada's turn, and no
    // `reasoning_effort` at all for Bob's (`gateway_tests.rs` holds the door to that).
    assert_eq!(h.efforts(&ada), [Effort::High]);
    assert_eq!(h.efforts(&bob), [Effort::Inherit]);
}

/// The effort is the turn's, not the coworker's as it stands when a person gets round to the
/// card: changing it while the run waited must not change how the continuation thinks, for the
/// reason a repin must not change its model.
#[tokio::test]
async fn a_run_answered_after_the_effort_changed_keeps_the_effort_it_started_with() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, MockDoor::asking_for_a_tool()).await;
    let owner = h.person(None).await;
    let hired = h.hire(&owner, "Ada").await;
    let ada = hired["id"].as_str().expect("id").to_string();
    let (status, set) = h
        .send(
            &owner,
            reqwest::Method::POST,
            &format!("/coworkers/{ada}/approvals"),
            Some(json!({ "tools": ["shell"] })),
        )
        .await;
    assert_eq!(status, 200, "shell needs a person's yes: {set}");
    assert_eq!(
        h.patch(&owner, &ada, json!({ "effort": "high" })).await.0,
        200
    );

    h.turn(&owner, &ada).await;
    let (run_id, call_id) = h.wait_for_pending(&owner).await;
    let (run, _) = h.store.load_run(&run_id).await.expect("run");
    assert_eq!(run.effort, Effort::High, "captured on the run's start");

    // While the card waits, its owner turns it down.
    assert_eq!(
        h.patch(&owner, &ada, json!({ "effort": "low" })).await.0,
        200
    );
    let (status, answered) = h
        .send(
            &owner,
            reqwest::Method::POST,
            &format!("/ag-ui/runs/{}/answer", run_id.as_str()),
            Some(json!({ "call_id": call_id, "approved": true })),
        )
        .await;
    assert_eq!(status, 200, "{answered}");
    let run = h.wait_for_ending(&run_id).await;
    assert_eq!(run.status, RunStatus::Finished, "{:?}", run.failure);
    assert_eq!(
        h.efforts(&ada),
        [Effort::High, Effort::High],
        "the turn and its continuation both thought as the turn started"
    );

    // The change was real: the next turn is the one that thinks less.
    h.turn(&owner, &ada).await;
    assert_eq!(h.efforts(&ada).last(), Some(&Effort::Low));
}

/// ONE SWEEP AT A TIME, as in `against_an_interrupted_run.rs`: `sweep_once` claims whatever is
/// abandoned in this binary's database, so one test's sweep could carry another's run on in its
/// own process, through its own door. An advisory lock held for the test keeps the two apart.
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

/// A hired coworker's run as a dead process left it: started at high, `then` applied, and quiet
/// for longer than a lease. Its owner turns the coworker down to low afterwards, and the sweep is
/// driven until the run ends.
async fn carried_on_after_a_restart(h: &Harness, then: Vec<RunCommand>) -> (String, Run) {
    let owner = h.person(None).await;
    let hired = h.hire(&owner, "Nightshift").await;
    let id = hired["id"].as_str().expect("id").to_string();
    let run_id = RunId::new();
    let thread = unique("th");
    let quiet_since = now_ms() - 3 * opengrok_server::recovery::LEASE_MS;
    let start = RunCommand::Start {
        thread_id: thread.clone(),
        coworker_id: Some(CoworkerId::from_stored(id.clone())),
        model: Some("oag/cheap".to_string()),
        effort: Effort::High,
        system: None,
        skill_id: None,
        prompt: Some(vec![
            json!({ "id": "m-person", "role": "user", "content": "summarise the inbox" }),
        ]),
        limits: Default::default(),
        at_ms: quiet_since,
    };
    let mut run = Run::default();
    let mut log = Vec::new();
    for command in std::iter::once(start).chain(then) {
        for event in run.decide(command).expect("a command the run accepts") {
            run.apply(&event);
            log.push(event);
        }
    }
    let view = RunView {
        id: run_id.clone(),
        thread_id: thread,
        status: run.status,
        event_count: run.emitted.len() as i64,
        updated_at_ms: quiet_since,
    };
    h.store
        .append_run(&run_id, 0, &log, &view, Some(&owner.id))
        .await
        .expect("append the interrupted run");
    assert_eq!(
        h.patch(&owner, &id, json!({ "effort": "low" })).await.0,
        200
    );

    for _ in 0..80 {
        opengrok_server::recovery::sweep_once(&h.host)
            .await
            .expect("sweep");
        let (run, _) = h.store.load_run(&run_id).await.expect("load");
        if run.status.is_terminal() {
            return (id, run);
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    panic!("the sweep never carried the run to an ending");
}

/// The same for a run the recovery sweep carries on after a restart (#91): its effort is the one
/// its start recorded, not the coworker's by the time the sweep finds it.
#[tokio::test]
async fn a_run_carried_on_after_a_restart_keeps_the_effort_it_started_with() {
    let database_url = database_or_skip!();
    let _sweeper = one_sweeper(&database_url).await;
    let h = harness(&database_url, MockDoor::echoing()).await;
    let said = RunCommand::Emit {
        payload: json!({ "type": "TEXT_MESSAGE_CONTENT", "messageId": "m-said", "delta": "Reading." }),
        at_ms: now_ms(),
    };
    let (id, run) = carried_on_after_a_restart(&h, vec![said]).await;
    assert_eq!(run.status, RunStatus::Finished, "{:?}", run.failure);
    assert_eq!(run.generation, 1, "carried on, in its next generation");
    assert_eq!(h.efforts(&id), [Effort::High], "as hard as it started");
}

/// And for a person's answer the sweep carries out after a restart, whose call never started: the
/// path a submitted form takes too (`resume_suspended_run`).
#[tokio::test]
async fn an_answer_carried_out_after_a_restart_keeps_the_effort_it_started_with() {
    let database_url = database_or_skip!();
    let _sweeper = one_sweeper(&database_url).await;
    let h = harness(&database_url, MockDoor::echoing()).await;
    let (id, run) = carried_on_after_a_restart(
        &h,
        vec![
            RunCommand::Suspend {
                call_id: "call-ls".to_string(),
                tool: "shell".to_string(),
                arguments: json!({ "command": "ls" }),
                reason: Default::default(),
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
    assert_eq!(run.status, RunStatus::Finished, "{:?}", run.failure);
    assert!(run.unstarted_answer.is_none(), "the answer was carried out");
    assert_eq!(h.efforts(&id), [Effort::High], "as hard as it started");
}
