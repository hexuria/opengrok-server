//! A Bot's routine tools (#316): `list_routines`, `create_routine`, `update_routine` and
//! `delete_routine`, through the server as a turn reaches them — the executor's gates, the desk
//! `POST`/`PATCH`/`DELETE /schedules` use, and the card a delete raises.
//!
//! The model here does what the person's message says: a message of `{"tool": …, "arguments":
//! …}` makes that one call, and once the call's result is in the conversation it says the result
//! back in words. So a test drives exactly the call it means, and reads exactly what the model
//! was told back, through the same turn a person's chat takes.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use opengrok_box::{BoxResult, CommandOutput, Computer, StartedCommand};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, RunId};
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

const NO_WHEN: &str = "ask the person for the time and days with request_user_form first.";
const ONE_SCHEDULE: &str =
    "a routine has one schedule for now; make a second routine for another time";
const NO_WEBHOOK: &str = "a Bot can't make a webhook trigger: its key must not pass through \
                          chat. Ask the person to add one in Routines.";
const FLOOR: &str = "a routine can wake at most once a minute: use 5 fields, like */5 * * * *.";
const ROUTINE_TOOLS: [&str; 5] = [
    "list_routines",
    "create_routine",
    "update_routine",
    "delete_routine",
    "run_routine",
];

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::now_v7().simple())
}

/// A model that makes the one call its person's message asks for, then says the result back.
#[derive(Default)]
struct Caller {
    asked: Mutex<Vec<ModelRequest>>,
}

#[async_trait]
impl ModelDoor for Caller {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
        self.asked.lock().unwrap().push(request.clone());
        let last = request.messages.last();
        let deltas = match last {
            Some(message) if message.role == "tool" => {
                vec![ModelDelta::Text(format!("result: {}", message.content))]
            }
            _ => {
                let asked = request.messages.iter().rev().find(|m| m.role == "user");
                let wanted: Value = asked
                    .and_then(|message| serde_json::from_str(&message.content).ok())
                    .unwrap_or_default();
                match wanted["tool"].as_str() {
                    Some(name) => vec![
                        ModelDelta::ToolCallStart {
                            id: "call-routine".to_string(),
                            name: name.to_string(),
                        },
                        ModelDelta::ToolCallArgs {
                            id: "call-routine".to_string(),
                            delta: wanted["arguments"].to_string(),
                        },
                        ModelDelta::ToolCallEnd {
                            id: "call-routine".to_string(),
                        },
                    ],
                    None => vec![ModelDelta::Text("nothing to do".to_string())],
                }
            }
        };
        Ok(Box::pin(futures::stream::iter(deltas.into_iter().map(Ok))))
    }
}

/// A computer that is always up, so a Bot is given its tools.
struct StubComputer;

#[async_trait]
impl Computer for StubComputer {
    async fn create(&self, _ttl_seconds: Option<u64>) -> BoxResult<String> {
        Ok(format!("bx_routines_{}", uuid::Uuid::now_v7().simple()))
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
    token: String,
}

struct Harness {
    base: String,
    client: reqwest::Client,
    store: PgStore,
    minter: Arc<TokenMinter>,
    door: Arc<Caller>,
}

/// What one tool call came to, as the turn's frames say it.
struct Called {
    /// The thread the turn was taken on, for its replay.
    thread: String,
    /// `TOOL_CALL_RESULT`'s `ok` and `content`, when the call ran or was refused.
    result: Option<(bool, String)>,
    /// The `run-awaiting-approval` frame, when the call parked on a card.
    parked: Option<Value>,
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
    let minter = Arc::new(TokenMinter::new(b"routine-tools-test-secret"));
    let auth = AuthState::new(store.clone(), minter.clone(), "owner@og.local".to_string());
    let door = Arc::new(Caller::default());
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
    let app = opengrok_server::router(agui, host);
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
        door,
    }
}

impl Harness {
    async fn person(&self) -> Person {
        let id = AccountId::new();
        let email = format!("{}@og.local", unique("routines"));
        let at_ms = chrono::Utc::now().timestamp_millis();
        let events = Account::default()
            .decide(AccountCommand::Register {
                email: email.clone(),
                password_hash: "x".to_string(),
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
            email: email.clone(),
            plan: Plan::Ultra,
            trial: false,
            updated_at_ms: at_ms,
            password_hash: Some("x".to_string()),
            first_name: "Test".to_string(),
            last_name: "User".to_string(),
            org_id: None,
            verified: true,
            enabled: true,
            avatar_url: None,
        };
        self.store
            .append_account(&id, 0, &events, &view)
            .await
            .expect("append account");
        let now = chrono::Utc::now().timestamp();
        let token = self
            .minter
            .mint_access(id.as_str(), "sess-routines", &email, "ultra", now, 3600)
            .expect("mint access");
        Person { token }
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
            .bearer_auth(&who.token);
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

    async fn hire(&self, who: &Person, name: &str) -> String {
        let body = Some(json!({ "name": name }));
        let (status, hired) = self
            .send(who, reqwest::Method::POST, "/coworkers", body)
            .await;
        assert_eq!(status, 201, "hire {name}: {hired}");
        hired["id"].as_str().expect("id").to_string()
    }

    /// The person's routines, as the Routines pane reads them.
    async fn routines(&self, who: &Person) -> Vec<Value> {
        let (status, rows) = self
            .send(who, reqwest::Method::GET, "/schedules", None)
            .await;
        assert_eq!(status, 200, "{rows}");
        rows.as_array().cloned().unwrap_or_default()
    }

    async fn rest_routine(&self, who: &Person, body: Value) -> Value {
        let (status, made) = self
            .send(who, reqwest::Method::POST, "/schedules", Some(body))
            .await;
        assert_eq!(status, 201, "{made}");
        made
    }

    /// One turn in which `bot` makes the one call `tool(arguments)`.
    async fn call(&self, who: &Person, bot: &str, tool: &str, arguments: Value) -> Called {
        let said = json!({ "tool": tool, "arguments": arguments }).to_string();
        let thread = unique("thr");
        let res = self
            .client
            .post(format!("{}/ag-ui", self.base))
            .bearer_auth(&who.token)
            .json(&json!({
                "threadId": thread,
                "runId": uuid::Uuid::now_v7().to_string(),
                "messages": [{ "id": unique("m"), "role": "user", "content": said }],
                "forwardedProps": { "coworkerId": bot },
            }))
            .send()
            .await
            .expect("ag-ui turn");
        assert_eq!(res.status().as_u16(), 200, "ag-ui turn status");
        let text = res.text().await.expect("sse");
        let frames: Vec<Value> = text
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|frame| serde_json::from_str(frame).ok())
            .collect();
        let result = frames
            .iter()
            .find(|frame| frame["type"] == "TOOL_CALL_RESULT")
            .map(|frame| {
                let content = frame["content"].as_str().unwrap_or_default();
                (frame["ok"] == true, content.to_string())
            });
        let parked = frames
            .iter()
            .find(|frame| frame["name"] == "run-awaiting-approval")
            .cloned();
        Called {
            thread,
            result,
            parked,
        }
    }

    /// The call's result, which must be one the model can read.
    async fn answer(
        &self,
        who: &Person,
        bot: &str,
        tool: &str,
        arguments: Value,
    ) -> (bool, String) {
        let called = self.call(who, bot, tool, arguments).await;
        called.result.expect("the call came back with a result")
    }

    /// The routine tools the last request to the model was offered.
    fn offered(&self) -> Vec<String> {
        let asked = self.door.asked.lock().unwrap();
        let last = asked.last().expect("the model was asked");
        let names = last
            .tools
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str());
        let names = names.filter(|name| ROUTINE_TOOLS.contains(name));
        names.map(str::to_string).collect()
    }

    async fn wait_for_ending(&self, run_id: &RunId) {
        for _ in 0..100 {
            let (run, _) = self.store.load_run(run_id).await.expect("run");
            if run.status.is_terminal() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("the run did not end in 10s");
    }
}

/// The epoch milliseconds `ms` as a time of day in UTC, and the weekday it falls on in `zone`.
fn utc_time_and_local_day(ms: i64, zone: &str) -> (String, chrono::Weekday) {
    use chrono::Datelike;
    let at = chrono::DateTime::from_timestamp_millis(ms).expect("a time");
    let zone = opengrok_core::schedule::zone(zone).expect("a zone");
    (
        at.format("%H:%M").to_string(),
        at.with_timezone(&zone).weekday(),
    )
}

/// A BOT MAKES A ROUTINE WHEN ITS PERSON ASKS, and the Routines pane lists it: on the Bot whose
/// turn it was, read in the person's own zone since the call named none, next waking at nine on a
/// weekday in Manila, which is one in the morning UTC. `list_routines` tells the model the same.
#[tokio::test]
async fn a_bot_makes_a_routine_in_its_persons_zone_and_the_pane_lists_it() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let zone = Some(json!({ "timeZone": "Asia/Manila" }));
    assert_eq!(
        h.send(&ada, reqwest::Method::PUT, "/account", zone).await.0,
        200
    );
    let luna = h.hire(&ada, "Luna").await;

    let arguments = json!({ "name": "Standup", "prompt": "post the standup",
        "when": "0 9 * * MON-FRI" });
    let (ok, said) = h.answer(&ada, &luna, "create_routine", arguments).await;
    assert!(ok, "{said}");
    let made: Value = serde_json::from_str(&said).expect("the routine, as JSON");
    assert_eq!(made["tz"], "Asia/Manila", "{made}");
    assert_eq!(made["when"], json!(["0 9 * * MON-FRI"]), "{made}");
    assert_eq!(made["bot"], "Luna", "{made}");
    assert!(
        made.get("note").is_none() && made.get("willNotRun").is_none(),
        "{made}"
    );

    let rows = h.routines(&ada).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = &rows[0];
    assert_eq!(row["id"], made["id"]);
    assert_eq!(row["coworkerId"], luna, "the Bot whose turn it was");
    assert_eq!(row["tz"], "Asia/Manila", "{row}");
    assert_eq!(row["cron"], "0 0 9 * * MON-FRI", "{row}");
    let next = row["nextDueMs"].as_i64().expect("nextDueMs");
    let (at, day) = utc_time_and_local_day(next, "Asia/Manila");
    assert_eq!(at, "01:00", "nine in Manila is one in the morning UTC");
    use chrono::Weekday;
    assert!(!matches!(day, Weekday::Sat | Weekday::Sun), "{day}");
    assert_eq!(made["nextDueMs"], row["nextDueMs"]);

    let (ok, listed) = h.answer(&ada, &luna, "list_routines", json!({})).await;
    assert!(ok, "{listed}");
    let listed: Value = serde_json::from_str(&listed).expect("a list");
    assert_eq!(listed, json!([made]), "the list says what the create said");
}

/// A call may name the zone, and another of the person's own Bots, by name.
#[tokio::test]
async fn a_bot_names_the_zone_and_another_of_its_persons_bots() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let luna = h.hire(&ada, "Luna").await;
    let sol = h.hire(&ada, "Sol").await;
    let arguments = json!({ "prompt": "water the plants", "when": "30 7 * * *",
        "tz": "Europe/London", "bot": "sol" });
    let (ok, said) = h.answer(&ada, &luna, "create_routine", arguments).await;
    assert!(ok, "{said}");
    let row = h.routines(&ada).await.pop().expect("a routine");
    assert_eq!(row["tz"], "Europe/London", "{row}");
    assert_eq!(row["coworkerId"], sol, "{row}");
    assert_eq!(row["name"], "water the plants", "its prompt's first words");
}

/// THE TIME AND DAYS ARE THE PERSON'S: a create with no `when` is sent back to ask them, in the
/// contract's words, and the offer says `when` is required and never guessed. Nothing is made.
#[tokio::test]
async fn a_routine_with_no_time_is_sent_back_to_ask_the_person() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let luna = h.hire(&ada, "Luna").await;
    let arguments = json!({ "prompt": "post the standup" });
    let (ok, said) = h.answer(&ada, &luna, "create_routine", arguments).await;
    assert!(!ok);
    assert_eq!(said, format!("refused: {NO_WHEN}"));
    assert!(h.routines(&ada).await.is_empty());
    let asked = h.door.asked.lock().unwrap().last().cloned().expect("asked");
    let create = asked
        .tools
        .iter()
        .find(|tool| tool["function"]["name"] == "create_routine");
    let create = create.expect("create_routine is offered");
    assert_eq!(
        create["function"]["parameters"]["required"],
        json!(["prompt", "when"])
    );
    let words = create["function"]["description"].as_str().unwrap();
    assert!(
        words.contains("never guessed") && words.contains("request_user_form"),
        "{words}"
    );
}

/// One cron for now, and no webhook from a Bot: each refused in the contract's words, and
/// nothing is made.
#[tokio::test]
async fn two_crons_or_a_webhook_are_refused_in_words() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let luna = h.hire(&ada, "Luna").await;
    let two = json!({ "prompt": "p", "when": ["0 9 * * 1", "0 17 * * 5"] });
    let (_, said) = h.answer(&ada, &luna, "create_routine", two).await;
    assert_eq!(said, format!("refused: {ONE_SCHEDULE}"));
    let hook = json!({ "prompt": "p", "kind": "webhook" });
    let (_, said) = h.answer(&ada, &luna, "create_routine", hook).await;
    assert_eq!(said, format!("refused: {NO_WEBHOOK}"));
    assert!(h.routines(&ada).await.is_empty());
}

/// THE ONE-MINUTE FLOOR: a cron that wakes more often is a 422 on the route, in the contract's
/// words, and the same words to a Bot. One call on `POST /schedules`, which the corpus keeps.
#[tokio::test]
async fn a_routine_that_wakes_more_often_than_once_a_minute_is_refused() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let luna = h.hire(&ada, "Luna").await;
    let body = json!({ "coworkerId": luna, "prompt": "tick", "cron": "*/30 * * * * *" });
    let (status, refused) = h
        .send(&ada, reqwest::Method::POST, "/schedules", Some(body))
        .await;
    assert_eq!(status, 422, "{refused}");
    assert_eq!(refused, json!({ "error": FLOOR }));
    let fast = json!({ "prompt": "tick", "when": "* * * * * *" });
    let (_, said) = h.answer(&ada, &luna, "create_routine", fast).await;
    assert_eq!(said, format!("refused: {FLOOR}"));
    assert!(h.routines(&ada).await.is_empty());
}

/// The floor holds on an edit that sets a cron, and an edit that leaves the cron alone is not
/// asked about it.
#[tokio::test]
async fn an_edit_to_a_cron_under_a_minute_is_refused() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let luna = h.hire(&ada, "Luna").await;
    let body = json!({ "coworkerId": luna, "prompt": "tick", "cron": "*/5 * * * *" });
    let id = h.rest_routine(&ada, body).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let path = format!("/schedules/{id}");
    let fast = Some(json!({ "cron": "15 * * * * *" }));
    let (status, refused) = h.send(&ada, reqwest::Method::PATCH, &path, fast).await;
    assert_eq!((status, refused), (422, json!({ "error": FLOOR })));
    let edit = json!({ "routine": id, "when": "*/10 * * * * *" });
    let (_, said) = h.answer(&ada, &luna, "update_routine", edit).await;
    assert_eq!(said, format!("refused: {FLOOR}"));
    let (status, _) = h
        .send(
            &ada,
            reqwest::Method::PATCH,
            &path,
            Some(json!({ "name": "Tick" })),
        )
        .await;
    assert_eq!(status, 200);
}

/// ONLY THE PERSON'S OWN BOTS: a call that aims a routine at somebody else's Bot, by its id or
/// its name, is refused with the person's own list, and nothing is made for anybody.
#[tokio::test]
async fn a_bot_cannot_aim_a_routine_at_a_bot_its_person_does_not_own() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let (ada, bea) = (h.person().await, h.person().await);
    let luna = h.hire(&ada, "Luna").await;
    let theirs = h.hire(&bea, "Orion").await;
    for named in [theirs.as_str(), "Orion"] {
        let arguments = json!({ "prompt": "p", "when": "0 9 * * 1", "bot": named });
        let (ok, said) = h.answer(&ada, &luna, "create_routine", arguments).await;
        assert!(!ok);
        let expected = format!("refused: no Bot of yours is called \"{named}\"; yours are: Luna");
        assert_eq!(said, expected);
    }
    assert!(h.routines(&ada).await.is_empty());
    assert!(h.routines(&bea).await.is_empty());
}

/// A ROUTINE THAT IS NOT THE PERSON'S IS UNKNOWN, whether it is somebody else's or nobody's: an
/// edit and a delete of it are refused in the same words, before any card, and it is untouched.
#[tokio::test]
async fn a_routine_that_is_not_yours_is_unknown() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let (ada, bea) = (h.person().await, h.person().await);
    let luna = h.hire(&ada, "Luna").await;
    let orion = h.hire(&bea, "Orion").await;
    let body = json!({ "coworkerId": orion, "name": "Bea's", "prompt": "p", "cron": "0 9 * * 1" });
    let theirs = h.rest_routine(&bea, body).await;
    let theirs = theirs["id"].as_str().unwrap().to_string();
    for id in [theirs.as_str(), "sched_nobody"] {
        let unknown = format!("refused: no routine {id} is yours; call list_routines.");
        let edit = json!({ "routine": id, "name": "Mine now" });
        let called = h.call(&ada, &luna, "update_routine", edit).await;
        assert_eq!(called.result.map(|(_, said)| said), Some(unknown.clone()));
        let called = h
            .call(&ada, &luna, "delete_routine", json!({ "routine": id }))
            .await;
        assert!(
            called.parked.is_none(),
            "no card for a routine that is not theirs"
        );
        assert_eq!(called.result.map(|(_, said)| said), Some(unknown));
    }
    let row = h.routines(&bea).await.pop().expect("theirs");
    assert_eq!(row["name"], "Bea's", "untouched: {row}");
}

/// An edit changes what it names and keeps the rest: the name, the cron, and the zone it is read
/// in, which moves its next wake with it.
#[tokio::test]
async fn an_update_changes_the_name_the_cron_and_the_zone() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let luna = h.hire(&ada, "Luna").await;
    let body = json!({ "coworkerId": luna, "name": "Report", "prompt": "write it",
        "cron": "0 9 * * *", "tz": "UTC" });
    let id = h.rest_routine(&ada, body).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let edit = json!({ "routine": id, "name": "Morning report", "when": "0 8 * * *",
        "tz": "Asia/Manila" });
    let (ok, said) = h.answer(&ada, &luna, "update_routine", edit).await;
    assert!(ok, "{said}");
    let row = h.routines(&ada).await.pop().expect("the routine");
    assert_eq!(row["name"], "Morning report");
    assert_eq!(row["cron"], "0 0 8 * * *");
    assert_eq!(row["tz"], "Asia/Manila");
    assert_eq!(row["prompt"], "write it", "kept");
    let next = row["nextDueMs"].as_i64().expect("nextDueMs");
    let (at, _) = utc_time_and_local_day(next, "Asia/Manila");
    assert_eq!(at, "00:00", "eight in Manila is midnight UTC");

    let paused = json!({ "routine": id, "active": false });
    let (ok, said) = h.answer(&ada, &luna, "update_routine", paused).await;
    assert!(ok, "{said}");
    assert_eq!(h.routines(&ada).await[0]["active"], false);
}

/// A DELETE ALWAYS ASKS: the card names the routine as it is stored, whatever name the call
/// wrote; a no leaves it, and a yes deletes it. The approvals queue names it the same way.
#[tokio::test]
async fn a_delete_asks_first_naming_the_routine_as_stored() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let luna = h.hire(&ada, "Luna").await;
    let body = json!({ "coworkerId": luna, "name": "Weekly report", "prompt": "write it",
        "cron": "0 9 * * 1" });
    let id = h.rest_routine(&ada, body).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let delete = json!({ "routine": id, "name": "A harmless test" });

    for (approved, kept) in [(false, true), (true, false)] {
        let called = h.call(&ada, &luna, "delete_routine", delete.clone()).await;
        let parked = called.parked.expect("a card first");
        assert_eq!(parked["reason"], "policy-approval", "{parked}");
        let why = parked["why"].as_str().unwrap();
        assert!(why.contains("\"Weekly report\""), "{why}");
        assert!(!why.contains("harmless"), "{why}");
        let waiting = called
            .result
            .map(|(ok, said)| (ok, said.starts_with("waiting for approval")));
        assert_eq!(waiting, Some((false, true)), "nothing ran yet");
        assert_eq!(h.routines(&ada).await.len(), 1, "nothing deleted yet");

        let (status, queue) = h
            .send(&ada, reqwest::Method::GET, "/ag-ui/approvals", None)
            .await;
        assert_eq!(status, 200, "{queue}");
        let waiting = queue.as_array().unwrap().last().cloned().expect("waiting");
        assert!(
            waiting["why"]
                .as_str()
                .unwrap()
                .contains("\"Weekly report\""),
            "{waiting}"
        );
        let run_id = RunId::from_stored(waiting["runId"].as_str().unwrap());
        let answer = json!({ "call_id": waiting["callId"], "approved": approved });
        let path = format!("/ag-ui/runs/{}/answer", run_id.as_str());
        let (status, answered) = h
            .send(&ada, reqwest::Method::POST, &path, Some(answer))
            .await;
        assert_eq!(status, 200, "{answered}");
        h.wait_for_ending(&run_id).await;
        assert_eq!(
            h.routines(&ada).await.len(),
            usize::from(kept),
            "approved: {approved}"
        );
    }
}

/// A RUN A ROUTINE STARTED MAY ONLY LIST: it is offered `list_routines` and none of the other
/// three, and a call to make one is refused, so a routine cannot make routines.
#[tokio::test]
async fn a_routines_own_run_is_offered_only_the_listing() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let luna = h.hire(&ada, "Luna").await;
    let breed = json!({ "tool": "create_routine",
        "arguments": { "prompt": "more", "when": "0 10 * * *" } });
    let body = json!({ "coworkerId": luna, "name": "Breeder", "prompt": breed.to_string(),
        "cron": "0 9 * * *" });
    let id = h.rest_routine(&ada, body).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let path = format!("/schedules/{id}/run");
    let (status, accepted) = h
        .send(&ada, reqwest::Method::POST, &path, Some(json!({})))
        .await;
    assert_eq!(status, 202, "{accepted}");
    let run_id = RunId::from_stored(accepted["runId"].as_str().unwrap());
    h.wait_for_ending(&run_id).await;
    let asked = h.door.asked.lock().unwrap().clone();
    let first = asked.first().expect("the routine's run asked the model");
    let offered: Vec<&str> = first
        .tools
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .filter(|name| ROUTINE_TOOLS.contains(name))
        .collect();
    assert_eq!(offered, ["list_routines"]);
    let said = asked
        .last()
        .unwrap()
        .messages
        .last()
        .unwrap()
        .content
        .clone();
    assert!(said.contains("may only list routines"), "{said}");
    assert_eq!(h.routines(&ada).await.len(), 1, "no routine made a routine");
}

/// A BOT'S NUMBERED DAYS ARE STANDARD CRON'S (#331, review of #334): `0 9 * * 1-5` from a Bot is
/// stored by name, Monday to Friday, and read back as five named fields; every waking of its week
/// is a weekday at nine in its zone, never a Sunday.
#[tokio::test]
async fn a_bots_numbered_weekdays_are_stored_by_name_and_wake_on_weekdays() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let luna = h.hire(&ada, "Luna").await;
    let arguments = json!({ "prompt": "post the standup", "when": "0 9 * * 1-5",
        "tz": "Asia/Manila" });
    let (ok, said) = h.answer(&ada, &luna, "create_routine", arguments).await;
    assert!(ok, "{said}");
    let made: Value = serde_json::from_str(&said).expect("the routine, as JSON");
    assert_eq!(made["when"], json!(["0 9 * * MON-FRI"]), "{made}");
    let rows = h.routines(&ada).await;
    let row = &rows[0];
    assert_eq!(row["cron"], "0 0 9 * * MON-FRI", "{row}");
    let mut next = row["nextDueMs"].as_i64().expect("nextDueMs");
    let mut days = Vec::new();
    for _ in 0..5 {
        let (at, day) = utc_time_and_local_day(next, "Asia/Manila");
        assert_eq!(at, "01:00", "nine in Manila");
        days.push(day);
        let cron = row["cron"].as_str().unwrap();
        next = opengrok_core::schedule::next_fire_ms(cron, "Asia/Manila", next).expect("next");
    }
    days.sort_by_key(chrono::Weekday::num_days_from_monday);
    use chrono::Weekday::{Fri, Mon, Thu, Tue, Wed};
    assert_eq!(days, [Mon, Tue, Wed, Thu, Fri], "a week of wakings");
}

/// A ROUTINE FOR A BOT ON ITS PERSON'S OWN PLAN IS MADE, and says when it runs: only while the
/// person's computer is on, or their proxy answers, by the way their plan goes (#316, the owner's
/// later rule). It is never told it will not run.
#[tokio::test]
async fn a_routine_for_a_bot_on_its_persons_plan_says_when_it_runs() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let luna = h.hire(&ada, "Luna").await;
    let sol = h.hire(&ada, "Sol").await;
    let plan = Some(json!({ "source": "local_proxy", "model": "gpt-5.5" }));
    let (status, row) = h
        .send(
            &ada,
            reqwest::Method::PATCH,
            &format!("/coworkers/{sol}"),
            plan,
        )
        .await;
    assert_eq!(status, 200, "{row}");
    let fallback = json!({ "model": "xai/grok-4.6" });
    for (via, (on, fallback), note) in [
        (
            "loopback",
            (true, Value::Null),
            "runs only while your plan's proxy answers",
        ),
        (
            "mac",
            (true, Value::Null),
            "runs only while your computer is on",
        ),
        // While the relay is off (#332): on the fallback, or skipped with none.
        (
            "mac",
            (false, fallback),
            "runs on your Server fallback model while Relay is off",
        ),
        (
            "mac",
            (false, Value::Null),
            "is skipped while Relay is off for your plan",
        ),
        // The switch is the Mac's alone: the loopback never reads it.
        (
            "loopback",
            (false, Value::Null),
            "runs only while your plan's proxy answers",
        ),
    ] {
        let setting = json!({ "kind": "local_proxy", "via": via,
            "baseUrl": "http://127.0.0.1:9", "localModel": "gpt-5.5",
            "relayEnabled": on, "planFallback": fallback });
        let path = "/account/inference-source";
        let (status, saved) = h
            .send(&ada, reqwest::Method::PUT, path, Some(setting))
            .await;
        assert_eq!(status, 200, "{saved}");
        // Asked of Luna, who answers on the gateway, for Sol, who answers on the plan.
        let arguments = json!({ "prompt": "check in", "when": "0 9 * * *", "bot": "Sol" });
        let (ok, said) = h.answer(&ada, &luna, "create_routine", arguments).await;
        assert!(ok, "{said}");
        let made: Value = serde_json::from_str(&said).unwrap();
        assert_eq!(made["note"], note, "{made}");
        assert!(made.get("willNotRun").is_none(), "{made}");
    }
}

/// THE ROUTINES ARE ONE ROW of a Bot's ceiling, its only row for them, on for a new hire. Off,
/// none of the four is offered or listed; on again, all four are, and the listing shows the row.
#[tokio::test]
async fn the_routines_are_one_row_that_switches_all_four() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let luna = h.hire(&ada, "Luna").await;
    let path = format!("/coworkers/{luna}/ceiling");
    let (status, ceiling) = h.send(&ada, reqwest::Method::GET, &path, None).await;
    assert_eq!(status, 200, "{ceiling}");
    let rows = ceiling["tools"].as_array().unwrap();
    let named = |name: &str| rows.iter().filter(|row| row["name"] == name).count();
    assert_eq!(named("routines"), 1, "{ceiling}");
    assert!(
        ROUTINE_TOOLS.iter().all(|tool| named(tool) == 0),
        "{ceiling}"
    );
    let row = rows.iter().find(|row| row["name"] == "routines").unwrap();
    assert_eq!(row["kind"], "builtin");
    assert_eq!(row["label"], "Routines");
    assert_eq!(row["enabled"], true, "on for a new hire");

    let tools_listed = |body: &Value| -> Vec<String> {
        let tools = body["tools"].as_array().cloned().unwrap_or_default();
        tools
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .map(str::to_string)
            .collect()
    };
    let others: Vec<Value> = rows
        .iter()
        .filter(|row| row["enabled"] == true && row["name"] != "routines")
        .map(|row| row["name"].clone())
        .collect();
    let off = json!({ "enabled": others, "version": ceiling["version"] });
    let (status, now) = h.send(&ada, reqwest::Method::PUT, &path, Some(off)).await;
    assert_eq!(status, 200, "{now}");
    let listing = format!("/coworkers/{luna}/tools");
    let (_, listed) = h.send(&ada, reqwest::Method::GET, &listing, None).await;
    assert!(
        !tools_listed(&listed).contains(&"routines".to_string()),
        "{listed}"
    );
    h.call(&ada, &luna, "list_routines", json!({})).await;
    assert!(h.offered().is_empty(), "none of the four is offered");

    let mut on = others.clone();
    on.push(json!("routines"));
    let on = json!({ "enabled": on, "version": now["version"] });
    let (status, now) = h.send(&ada, reqwest::Method::PUT, &path, Some(on)).await;
    assert_eq!(status, 200, "{now}");
    let (_, listed) = h.send(&ada, reqwest::Method::GET, &listing, None).await;
    let listed = tools_listed(&listed);
    assert_eq!(
        listed.iter().filter(|name| *name == "routines").count(),
        1,
        "{listed:?}"
    );
    assert!(
        ROUTINE_TOOLS
            .iter()
            .all(|tool| !listed.contains(&tool.to_string()))
    );
    h.call(&ada, &luna, "list_routines", json!({})).await;
    assert_eq!(h.offered(), ROUTINE_TOOLS);
}

/// The ceiling as a Bot's settings read it: one row for its routines. One call on the route, for
/// the corpus.
#[tokio::test]
async fn a_bots_ceiling_lists_its_routines_as_one_row() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let luna = h.hire(&ada, "Luna").await;
    let path = format!("/coworkers/{luna}/ceiling");
    let (status, ceiling) = h.send(&ada, reqwest::Method::GET, &path, None).await;
    assert_eq!(status, 200, "{ceiling}");
    let routines = ceiling["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "routines")
        .cloned();
    let expected = json!({ "name": "routines", "kind": "builtin", "label": "Routines",
        "enabled": true, "description": "List, make, edit, delete and run your routines when you \
        ask in chat. Deleting one always asks you first." });
    assert_eq!(routines, Some(expected));
}

/// ONE DESK: the route and the tool make the same routine from the same words — every field of
/// its row the same but its id.
#[tokio::test]
async fn the_route_and_the_tool_make_the_same_routine() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let luna = h.hire(&ada, "Luna").await;
    let body = json!({ "coworkerId": luna, "name": "Inbox", "prompt": "sort the inbox",
        "cron": "15 8 * * MON", "tz": "America/New_York" });
    h.rest_routine(&ada, body).await;
    let arguments = json!({ "name": "Inbox", "prompt": "sort the inbox", "when": "15 8 * * MON",
        "tz": "America/New_York" });
    let (ok, said) = h.answer(&ada, &luna, "create_routine", arguments).await;
    assert!(ok, "{said}");
    let mut rows = h.routines(&ada).await;
    assert_eq!(rows.len(), 2);
    for row in &mut rows {
        row.as_object_mut().unwrap().remove("id");
    }
    assert_eq!(rows[0], rows[1]);
}

impl Harness {
    /// A routine of `who`'s for `bot`, made the way the Routines pane makes one: its id.
    async fn made(&self, who: &Person, bot: &str, name: &str) -> String {
        let body = json!({ "coworkerId": bot, "name": name, "prompt": "post the standup",
            "cron": "0 9 * * MON-FRI" });
        let made = self.rest_routine(who, body).await;
        made["id"].as_str().unwrap().to_string()
    }

    /// `who`'s inference source saved as `body`, which must be taken.
    async fn source(&self, who: &Person, body: Value) {
        let path = "/account/inference-source";
        let (status, saved) = self.send(who, reqwest::Method::PUT, path, Some(body)).await;
        assert_eq!(status, 200, "{saved}");
    }

    /// `bot`'s own door, set by its owner.
    async fn door(&self, who: &Person, bot: &str, body: Value) {
        let path = format!("/coworkers/{bot}");
        let (status, row) = self
            .send(who, reqwest::Method::PATCH, &path, Some(body))
            .await;
        assert_eq!(status, 200, "{row}");
    }

    /// The routine's runs, read from the store, so a wait records no reply.
    async fn runs_of(&self, routine: &str) -> usize {
        let runs =
            sqlx::query_scalar::<_, i64>("select count(*) from run_view where thread_id = $1");
        let runs = runs
            .bind(routine)
            .fetch_one(self.store.pool())
            .await
            .expect("runs");
        usize::try_from(runs).unwrap_or_default()
    }
}

/// A BOT RUNS ITS PERSON'S ROUTINE BY ITS ID, AS THEIR RUN NOW DOES (#337): its own Bot is woken
/// with its prompt on its thread, the call is told `{runId, threadId}`, and the history and the
/// row's `lastRun` say a Bot pressed it, which, as it was called. The person's own press says
/// `manual`, `by` null. One read of the history, the list and the call's thread each, which the
/// corpus keeps.
#[tokio::test]
async fn a_bot_runs_a_routine_by_its_id_and_its_history_names_the_bot() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let (luna, sol) = (h.hire(&ada, "Luna").await, h.hire(&ada, "Sol").await);
    let id = h.made(&ada, &sol, "Standup").await;
    let path = format!("/schedules/{id}/run");
    let (status, pressed) = h
        .send(&ada, reqwest::Method::POST, &path, Some(json!({})))
        .await;
    assert_eq!(status, 202, "{pressed}");
    h.wait_for_ending(&RunId::from_stored(pressed["runId"].as_str().unwrap()))
        .await;

    let called = h
        .call(&ada, &luna, "run_routine", json!({ "routine": id }))
        .await;
    assert!(called.parked.is_none(), "no card");
    let (ok, said) = called.result.expect("a result");
    assert!(ok, "{said}");
    let ran: Value = serde_json::from_str(&said).expect("JSON");
    let run_id = ran["runId"].as_str().expect("runId").to_string();
    assert_eq!(ran, json!({ "runId": run_id, "threadId": id }));
    h.wait_for_ending(&RunId::from_stored(run_id.clone())).await;
    let (run, _) = h
        .store
        .load_run(&RunId::from_stored(run_id.clone()))
        .await
        .unwrap();
    let woke = run.coworker_id.as_ref().map(|bot| bot.as_str());
    assert_eq!(
        (woke, run.thread_id.as_str()),
        (Some(sol.as_str()), id.as_str())
    );

    let by = json!({ "coworkerId": luna, "name": "Luna" });
    let (status, history) = h
        .send(&ada, reqwest::Method::GET, &format!("{path}s"), None)
        .await;
    assert_eq!(status, 200, "{history}");
    let causes: Vec<(&Value, &Value, &Value)> = history
        .as_array()
        .unwrap()
        .iter()
        .map(|row| (&row["runId"], &row["cause"], &row["by"]))
        .collect();
    let (bot, manual) = (json!("bot"), json!("manual"));
    let expected = vec![
        (&ran["runId"], &bot, &by),
        (&pressed["runId"], &manual, &Value::Null),
    ];
    assert_eq!(causes, expected, "{history}");
    let row = h.routines(&ada).await.pop().expect("the routine");
    let last = &row["lastRun"];
    let said = (&last["runId"], &last["cause"], &last["by"]);
    assert_eq!(said, (&ran["runId"], &bot, &by), "{last}");
    let replay = format!("/ag-ui/threads/{}", called.thread);
    let (status, thread) = h.send(&ada, reqwest::Method::GET, &replay, None).await;
    assert_eq!(status, 200, "{thread}");
    assert!(
        thread.to_string().contains(&run_id),
        "the call's result is replayed"
    );
}

/// BY ITS NAME TOO, as `list_routines` gives it (#337); a name two of the person's routines share
/// is refused naming both, and runs neither.
#[tokio::test]
async fn a_bot_runs_a_routine_by_its_name_and_a_shared_name_is_refused_naming_both() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let luna = h.hire(&ada, "Luna").await;
    let standup = h.made(&ada, &luna, "Standup").await;
    let (one, two) = (
        h.made(&ada, &luna, "Report").await,
        h.made(&ada, &luna, "Report").await,
    );
    let (ok, said) = h
        .answer(&ada, &luna, "run_routine", json!({ "routine": "Standup" }))
        .await;
    assert!(ok, "{said}");
    let ran: Value = serde_json::from_str(&said).expect("JSON");
    assert_eq!(ran["threadId"], standup.as_str(), "{ran}");
    h.wait_for_ending(&RunId::from_stored(ran["runId"].as_str().unwrap()))
        .await;

    let (ok, said) = h
        .answer(&ada, &luna, "run_routine", json!({ "routine": "Report" }))
        .await;
    assert!(!ok, "{said}");
    let start = "refused: more than one of your routines is called \"Report\" (";
    assert!(said.starts_with(start), "{said}");
    assert!(said.contains(&one) && said.contains(&two), "{said}");
    assert!(said.ends_with("); run one by its id."), "{said}");
    assert_eq!((h.runs_of(&one).await, h.runs_of(&two).await), (0, 0));
}

/// A ROUTINE THAT IS NOT THE PERSON'S IS UNKNOWN to `run_routine` too, by id or by name, whoever's
/// it is (#337), and nothing runs; a paused one of theirs is refused in the pause's words, which
/// the person's own press is not. One read of a refused call's thread, which the corpus keeps.
#[tokio::test]
async fn a_bot_cannot_run_a_routine_that_is_not_its_persons_or_is_paused() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let (ada, bea) = (h.person().await, h.person().await);
    let luna = h.hire(&ada, "Luna").await;
    let orion = h.hire(&bea, "Orion").await;
    let theirs = h.made(&bea, &orion, "Bea's").await;
    let mut refusal_thread = String::new();
    for asked in [theirs.as_str(), "Bea's", "sched_nobody"] {
        let called = h
            .call(&ada, &luna, "run_routine", json!({ "routine": asked }))
            .await;
        let unknown = format!("refused: no routine {asked} is yours; call list_routines.");
        assert_eq!(called.result, Some((false, unknown)), "{asked}");
        refusal_thread = called.thread;
    }
    assert_eq!(h.runs_of(&theirs).await, 0, "nothing ran");
    let replay = format!("/ag-ui/threads/{refusal_thread}");
    let (status, thread) = h.send(&ada, reqwest::Method::GET, &replay, None).await;
    assert_eq!(status, 200, "{thread}");

    let mine = h.made(&ada, &luna, "Standup").await;
    let pause = format!("/schedules/{mine}/pause");
    let (status, _) = h
        .send(&ada, reqwest::Method::POST, &pause, Some(json!({})))
        .await;
    assert_eq!(status, 204);
    let (ok, said) = h
        .answer(&ada, &luna, "run_routine", json!({ "routine": mine }))
        .await;
    assert_eq!(
        (ok, said.as_str()),
        (false, "refused: that schedule is paused")
    );
    assert_eq!(h.runs_of(&mine).await, 0, "a pause holds a Bot's press");
}

/// ITS PLAN'S RULES HOLD (#316, #332, #337): a routine of a Bot on its person's own plan is
/// skipped in the run now's words while the person's computer is off, or their proxy does not
/// answer, and its history says a Bot asked; with Relay off it runs on the fallback, or with none
/// is skipped as Relay off. The calling Bot is on the server, so its own turn is answered.
#[tokio::test]
async fn a_plan_bots_routine_run_by_a_bot_keeps_its_plans_rules() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let (luna, sol) = (h.hire(&ada, "Luna").await, h.hire(&ada, "Sol").await);
    h.door(&ada, &luna, json!({ "source": "gateway" })).await;
    h.door(
        &ada,
        &sol,
        json!({ "source": "local_proxy", "model": "gpt-6-luna" }),
    )
    .await;
    let id = h.made(&ada, &sol, "Standup").await;
    let mac = json!({ "kind": "local_proxy", "via": "mac", "relay": { "localModel": "gpt-5.5" } });
    let down = json!({ "kind": "local_proxy", "via": "loopback", "baseUrl": "http://127.0.0.1:1",
        "localModel": "gpt-5.5" });
    for (setting, words) in [
        (
            mac.clone(),
            "Skipped: your computer was off, so your plan couldn't answer",
        ),
        (down, "Skipped: your plan's proxy didn't answer"),
    ] {
        h.source(&ada, setting).await;
        let (ok, said) = h
            .answer(&ada, &luna, "run_routine", json!({ "routine": id }))
            .await;
        assert_eq!((ok, said), (false, format!("refused: {words}")));
    }
    assert_eq!(h.runs_of(&id).await, 0, "skipped, never run");
    let (status, history) = h
        .send(
            &ada,
            reqwest::Method::GET,
            &format!("/schedules/{id}/runs"),
            None,
        )
        .await;
    assert_eq!(status, 200, "{history}");
    let by = json!({ "coworkerId": luna, "name": "Luna" });
    for (row, code) in history
        .as_array()
        .unwrap()
        .iter()
        .zip(["proxy_down", "relay_offline"])
    {
        let said = (&row["cause"], &row["skipped"], &row["by"]);
        assert_eq!(said, (&json!("bot"), &json!(code), &by), "{row}");
    }

    let mut off = mac;
    off["relayEnabled"] = json!(false);
    off["planFallback"] = json!(null);
    h.source(&ada, off.clone()).await;
    let (ok, said) = h
        .answer(&ada, &luna, "run_routine", json!({ "routine": id }))
        .await;
    let words = "refused: Skipped: Relay is off for your plan";
    assert_eq!((ok, said.as_str()), (false, words));
    off["planFallback"] = json!({ "model": "oag/fallback", "effort": "low" });
    h.source(&ada, off).await;
    let (ok, said) = h
        .answer(&ada, &luna, "run_routine", json!({ "routine": id }))
        .await;
    assert!(ok, "{said}");
    let ran: Value = serde_json::from_str(&said).expect("JSON");
    let run_id = RunId::from_stored(ran["runId"].as_str().unwrap());
    h.wait_for_ending(&run_id).await;
    let (run, _) = h.store.load_run(&run_id).await.unwrap();
    let asked = (run.model.as_deref(), run.inference_source);
    use opengrok_core::inference::SourceKind;
    assert_eq!(
        asked,
        (Some("oag/fallback"), SourceKind::Gateway),
        "on the fallback"
    );
}

/// A ROUTINE'S OWN RUN CANNOT RUN A ROUTINE (#337's loop guard): `run_routine` is not offered to it,
/// and a call it makes anyway is refused, so no chain of runs can form.
#[tokio::test]
async fn a_routines_own_run_cannot_run_a_routine() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let luna = h.hire(&ada, "Luna").await;
    let id = h.made(&ada, &luna, "Loop").await;
    let again = json!({ "tool": "run_routine", "arguments": { "routine": id } });
    let path = format!("/schedules/{id}");
    let edit = Some(json!({ "prompt": again.to_string() }));
    let (status, row) = h.send(&ada, reqwest::Method::PATCH, &path, edit).await;
    assert_eq!(status, 200, "{row}");
    let (status, accepted) = h
        .send(
            &ada,
            reqwest::Method::POST,
            &format!("{path}/run"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, 202, "{accepted}");
    h.wait_for_ending(&RunId::from_stored(accepted["runId"].as_str().unwrap()))
        .await;
    let asked = h.door.asked.lock().unwrap().clone();
    let first = asked.first().expect("the routine's run asked the model");
    let offered = first
        .tools
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str());
    assert!(
        !offered.collect::<Vec<_>>().contains(&"run_routine"),
        "not offered"
    );
    let said = asked
        .last()
        .unwrap()
        .messages
        .last()
        .unwrap()
        .content
        .clone();
    assert!(said.contains("may only list routines"), "{said}");
    assert_eq!(h.runs_of(&id).await, 1, "no chain");
}
