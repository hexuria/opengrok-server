//! `GET /ag-ui/events`: the account's change notes (#348), over HTTP as the app reads them.
//!
//! What a person is told about what the server did on its own (a routine pressed, a hook, a Bot's
//! `run_routine`) and about their own turns, in ids and the history's words and nothing else; whose
//! it is told to; and how a connection that went away is brought back. The notes themselves, their
//! numbering and the stream's clocks are tested where they live (`opengrok-events`); these drive
//! the real server end to end.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use opengrok_box::{BoxResult, CommandOutput, Computer, StartedCommand};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::AccountId;
use opengrok_events::Tuning;
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

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::now_v7().simple())
}

/// A model that makes the one call its person's message asks for, then says the result back; any
/// other message gets a plain answer, which is how a routine's run and a chat turn end.
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
        Ok(format!("bx_events_{}", uuid::Uuid::now_v7().simple()))
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
    minter: Arc<TokenMinter>,
}

async fn harness(database_url: &str) -> Harness {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    // Short clocks, so a test waits on the server and not on a window: a stream is read as soon as
    // it is woken, and a ping is far off unless a test brings it close.
    store.events.tune(Tuning {
        ping: Duration::from_secs(60),
        poll: Duration::from_secs(60),
        window: Duration::from_millis(20),
        room: 64,
    });
    let minter = Arc::new(TokenMinter::new(b"events-stream-test-secret"));
    let auth = AuthState::new(store.clone(), minter.clone(), "owner@og.local".to_string());
    let agui = AgUiState {
        auth,
        door: Arc::new(Caller::default()),
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
    }
}

/// One block of the stream, taken apart.
#[derive(Debug, Clone, PartialEq)]
struct Block {
    id: i64,
    event: String,
    data: Value,
    /// As it came, for the checks that look at words and not at fields.
    text: String,
}

fn parse(text: &str) -> Block {
    let lines: Vec<&str> = text.trim_end_matches('\n').split('\n').collect();
    assert_eq!(lines.len(), 3, "id, event, one line of data: {text:?}");
    let field = |at: usize, name: &str| {
        let line = lines[at];
        line.strip_prefix(&format!("{name}: "))
            .unwrap_or_else(|| panic!("line {at} of {text:?} is not {name}"))
            .to_string()
    };
    Block {
        id: field(0, "id").parse().expect("a number"),
        event: field(1, "event"),
        data: serde_json::from_str(&field(2, "data")).expect("JSON"),
        text: text.to_string(),
    }
}

/// An open `GET /ag-ui/events`, read piece by piece.
struct Sse {
    response: reqwest::Response,
    pending: String,
}

impl Sse {
    async fn piece(&mut self, ms: u64) -> Option<String> {
        loop {
            if let Some(end) = self.pending.find("\n\n") {
                return Some(self.pending.drain(..end + 2).collect());
            }
            let next = tokio::time::timeout(Duration::from_millis(ms), self.response.chunk());
            let chunk = next.await.ok()?.expect("the stream held")?;
            self.pending.push_str(&String::from_utf8_lossy(&chunk));
        }
    }

    /// The next block within `ms`, pings skipped.
    async fn block(&mut self, ms: u64) -> Option<Block> {
        loop {
            let piece = self.piece(ms).await?;
            if !piece.starts_with(':') {
                return Some(parse(&piece));
            }
        }
    }

    async fn must(&mut self) -> Block {
        self.block(10_000).await.expect("a block within 10 s")
    }

    /// Blocks up to and including the first that `done` says is the last.
    async fn until(&mut self, done: impl Fn(&Block) -> bool) -> Vec<Block> {
        let mut told = Vec::new();
        loop {
            let block = self.must().await;
            let last = done(&block);
            told.push(block);
            if last {
                return told;
            }
        }
    }

    async fn nothing_for(&mut self, ms: u64) {
        let heard = self.block(ms).await;
        assert!(
            heard.is_none(),
            "told something it should not be: {heard:?}"
        );
    }
}

fn finished(run: &str) -> impl Fn(&Block) -> bool + '_ {
    move |block| block.event == "run.finished" && block.data["runId"] == run
}

impl Harness {
    async fn person(&self, first: &str, org: Option<&str>) -> Person {
        let id = AccountId::new();
        let email = format!("{}@og.local", unique(&first.to_lowercase()));
        let at_ms = chrono::Utc::now().timestamp_millis();
        let events = Account::default()
            .decide(AccountCommand::Register {
                email: email.clone(),
                password_hash: "x".to_string(),
                first_name: first.to_string(),
                last_name: "Test".to_string(),
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
            last_name: "Test".to_string(),
            org_id: org.map(str::to_string),
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
            .mint_access(id.as_str(), "sess-events", &email, "ultra", now, 3600)
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

    /// `GET /ag-ui/events`, as `who` holds it open, resuming from `last` when given.
    async fn listen(&self, who: &Person, last: Option<&str>) -> Sse {
        let mut request = self
            .client
            .get(format!("{}/ag-ui/events", self.base))
            .bearer_auth(&who.token);
        if let Some(last) = last {
            request = request.header("last-event-id", last);
        }
        let response = request.send().await.expect("events");
        assert_eq!(response.status().as_u16(), 200);
        Sse {
            response,
            pending: String::new(),
        }
    }

    /// A routine, as the app's pane makes it.
    async fn routine(&self, who: &Person, bot: &str, name: &str) -> String {
        let body = json!({ "coworkerId": bot, "name": name, "prompt": "summarise the quarter",
            "cron": "0 9 * * MON-FRI" });
        let (status, made) = self
            .send(who, reqwest::Method::POST, "/schedules", Some(body))
            .await;
        assert_eq!(status, 201, "{made}");
        made["id"].as_str().expect("id").to_string()
    }

    /// One turn on the AG-UI door, its stream read to the end. The thread, the run and the status.
    async fn turn(&self, who: &Person, bot: &str, thread: &str, said: &str) -> (String, u16) {
        let run = uuid::Uuid::now_v7().to_string();
        let res = self
            .client
            .post(format!("{}/ag-ui", self.base))
            .bearer_auth(&who.token)
            .json(&json!({
                "threadId": thread,
                "runId": run,
                "messages": [{ "id": unique("m"), "role": "user", "content": said }],
                "forwardedProps": { "coworkerId": bot },
            }))
            .send()
            .await
            .expect("ag-ui turn");
        let status = res.status().as_u16();
        res.text().await.expect("the turn's stream");
        (run, status)
    }

    /// A Bot's one tool call, as the model makes it, and what it was told back.
    async fn call(&self, who: &Person, bot: &str, tool: &str, arguments: Value) -> (String, Value) {
        let said = json!({ "tool": tool, "arguments": arguments }).to_string();
        let thread = unique("thr");
        let (run, status) = self.turn(who, bot, &thread, &said).await;
        assert_eq!(status, 200);
        (run, json!({ "thread": thread }))
    }
}

/// The notes a block sequence is, as `(event, data)`.
fn words(blocks: &[Block]) -> Vec<(&str, &Value)> {
    blocks
        .iter()
        .map(|block| (block.event.as_str(), &block.data))
        .collect()
}

// ---- who may listen ------------------------------------------------------------------------

/// A MISSING OR BAD TOKEN IS A 401 IN WORDS, like the other account routes; the console's cookie
/// opens the stream as the header does.
#[tokio::test]
async fn a_stream_is_for_a_signed_in_person_by_header_or_by_the_consoles_cookie() {
    let url = database_or_skip!();
    let h = harness(&url).await;
    let ada = h.person("Ada", None).await;
    let events = format!("{}/ag-ui/events", h.base);

    let res = h.client.get(&events).send().await.unwrap();
    assert_eq!(res.status().as_u16(), 401);
    assert_eq!(
        res.json::<Value>().await.unwrap(),
        json!({ "error": "sign in first" })
    );
    let res = h
        .client
        .get(&events)
        .bearer_auth("not-a-token")
        .send()
        .await;
    let res = res.unwrap();
    assert_eq!(res.status().as_u16(), 401);
    assert_eq!(
        res.json::<Value>().await.unwrap(),
        json!({ "error": "sign in first" })
    );

    let cookie = format!("og_access={}", ada.token);
    let res = h.client.get(&events).header("cookie", cookie).send().await;
    let res = res.unwrap();
    assert_eq!(res.status().as_u16(), 200);
    let kind = res.headers()["content-type"].to_str().unwrap();
    assert!(kind.starts_with("text/event-stream"), "{kind}");
    assert_eq!(res.headers()["cache-control"], "no-cache");
}

/// A fresh connection is told to read everything first: `reset`, with the head for its id.
#[tokio::test]
async fn a_new_connection_begins_with_a_reset() {
    let url = database_or_skip!();
    let h = harness(&url).await;
    let ada = h.person("Ada", None).await;
    let mut sse = h.listen(&ada, None).await;
    let first = sse.must().await;
    assert_eq!(first.text, "id: 0\nevent: reset\ndata: {}\n\n");
}

// ---- what is told ------------------------------------------------------------------------

/// A PERSON'S OWN TURN is told, so their other devices update: it begins as a `chat`, goes on a
/// round at a time, and ends `ok`; and not one word of the conversation, the Bot's name or the
/// routine's is on the wire.
#[tokio::test]
async fn a_chat_turn_is_told_in_ids_and_the_history_words_and_nothing_it_said() {
    let url = database_or_skip!();
    let h = harness(&url).await;
    let ada = h.person("Ada", None).await;
    let luna = h.hire(&ada, "Lunaaa-the-bot").await;
    let mut sse = h.listen(&ada, None).await;
    assert_eq!(sse.must().await.event, "reset");

    let said = "my private words 8f3a";
    let (run, status) = h.turn(&ada, &luna, "thread-chat", said).await;
    assert_eq!(status, 200);
    let told = sse.until(finished(&run)).await;

    let first = &told[0];
    assert_eq!(first.event, "run.started");
    assert_eq!(
        first.data,
        json!({ "runId": run, "threadId": "thread-chat", "coworkerId": luna, "cause": "chat" })
    );
    let ending = told.last().unwrap();
    assert_eq!(
        ending.data,
        json!({ "runId": run, "threadId": "thread-chat", "coworkerId": luna, "state": "ok" })
    );
    let middle = &told[1..told.len() - 1];
    assert!(!middle.is_empty(), "the rounds of the turn");
    for block in middle {
        assert_eq!(block.event, "thread.changed", "{block:?}");
        assert_eq!(
            block.data,
            json!({ "threadId": "thread-chat", "coworkerId": luna, "runId": run }),
            "every round of a run's own turn names the run"
        );
    }
    let ids: Vec<i64> = told.iter().map(|block| block.id).collect();
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]), "{ids:?}");

    // The bytes: three fields and a blank line, the data on one line with its keys in the order
    // the contract lists them.
    let block = |at: usize, event: &str, data: String| {
        format!("id: {}\nevent: {event}\ndata: {data}\n\n", told[at].id)
    };
    let started = format!(
        r#"{{"runId":"{run}","threadId":"thread-chat","coworkerId":"{luna}","cause":"chat"}}"#
    );
    assert_eq!(told[0].text, block(0, "run.started", started));
    let changed = format!(r#"{{"threadId":"thread-chat","coworkerId":"{luna}","runId":"{run}"}}"#);
    assert_eq!(told[1].text, block(1, "thread.changed", changed));
    let ended = format!(
        r#"{{"runId":"{run}","threadId":"thread-chat","coworkerId":"{luna}","state":"ok"}}"#
    );
    assert_eq!(ending.text, block(told.len() - 1, "run.finished", ended));

    for block in &told {
        for secret in [said, "Lunaaa", "nothing to do"] {
            assert!(
                !block.text.contains(secret),
                "{secret:?} is on the wire: {block:?}"
            );
        }
    }
}

/// A TEST RUN — a routine the server starts on a person's press — is told as the history says it:
/// the routine changed (its firing), the run started `manual` and naming the routine, the thread
/// changed, and the run finished naming the routine. None of the routine's name or prompt.
#[tokio::test]
async fn a_routine_run_pressed_by_its_person_is_told_manual_and_names_its_routine() {
    let url = database_or_skip!();
    let h = harness(&url).await;
    let ada = h.person("Ada", None).await;
    let sol = h.hire(&ada, "Sol").await;
    let mut sse = h.listen(&ada, None).await;
    assert_eq!(sse.must().await.event, "reset");

    let id = h.routine(&ada, &sol, "Quarterly secrets 51c9").await;
    let made = sse.must().await;
    assert_eq!(made.event, "routine.changed");
    assert_eq!(
        made.data,
        json!({ "routineId": id, "coworkerId": sol, "change": "created" })
    );

    let path = format!("/schedules/{id}/run");
    let (status, pressed) = h.send(&ada, reqwest::Method::POST, &path, None).await;
    assert_eq!(status, 202, "{pressed}");
    let run = pressed["runId"].as_str().unwrap().to_string();
    let told = sse.until(finished(&run)).await;

    assert_eq!(
        told[0].data,
        json!({ "routineId": id, "coworkerId": sol, "change": "updated" }),
        "the firing is a change to the routine's row"
    );
    assert_eq!(told[0].event, "routine.changed");
    assert_eq!(told[1].event, "run.started");
    assert_eq!(
        told[1].data,
        json!({ "runId": run, "threadId": id, "coworkerId": sol, "routineId": id,
                "cause": "manual" })
    );
    let ending = told.last().unwrap();
    assert_eq!(
        ending.data,
        json!({ "runId": run, "threadId": id, "coworkerId": sol, "routineId": id,
                "state": "ok" })
    );
    assert!(told.iter().any(|block| block.event == "thread.changed"));
    for block in &told {
        for secret in ["Quarterly secrets", "summarise the quarter"] {
            assert!(
                !block.text.contains(secret),
                "{secret:?} is on the wire: {block:?}"
            );
        }
    }
}

/// A CARD THE PERSON SETTLES IS NOT THE RUN'S COMMIT. A Bot's delete always asks: its turn parks
/// (told as the run's own rounds are, and not as an ending); the person's answer is a
/// `thread.changed` that names no run, `runId` present and `null`, so the app that is still
/// streaming the run reads the thread anyway; and the run goes on under its own id to the end.
#[tokio::test]
async fn a_card_the_person_settles_is_told_without_a_run_and_the_run_goes_on_under_its_own() {
    let url = database_or_skip!();
    let h = harness(&url).await;
    let ada = h.person("Ada", None).await;
    let luna = h.hire(&ada, "Luna").await;
    let id = h.routine(&ada, &luna, "Weekly").await;
    let mut sse = h.listen(&ada, None).await;
    assert_eq!(sse.must().await.event, "reset");

    let (turn, _) = h
        .call(&ada, &luna, "delete_routine", json!({ "routine": id }))
        .await;
    let started = sse.must().await;
    assert_eq!(started.event, "run.started");
    assert_eq!(started.data["runId"], turn.as_str());
    // Its rounds up to the card, and then quiet: the run is waiting and has not ended.
    let mut rounds = vec![sse.must().await];
    while let Some(more) = sse.block(700).await {
        rounds.push(more);
    }
    for round in &rounds {
        assert_eq!(round.event, "thread.changed", "{round:?}");
        assert_eq!(round.data["runId"], turn.as_str(), "the run's own round");
    }

    let (status, queue) = h
        .send(&ada, reqwest::Method::GET, "/ag-ui/approvals", None)
        .await;
    assert_eq!(status, 200, "{queue}");
    let cards = queue.as_array().unwrap();
    let waiting = cards.iter().find(|card| card["runId"] == turn.as_str());
    let call = waiting.expect("the card")["callId"].clone();
    let answer = json!({ "call_id": call, "approved": true });
    let path = format!("/ag-ui/runs/{turn}/answer");
    let (status, answered) = h
        .send(&ada, reqwest::Method::POST, &path, Some(answer))
        .await;
    assert_eq!(status, 200, "{answered}");

    let told = sse.until(finished(&turn)).await;
    let settled = told.first().unwrap();
    assert_eq!(settled.event, "thread.changed");
    assert_eq!(
        settled.data,
        json!({ "threadId": started.data["threadId"], "coworkerId": luna, "runId": null }),
        "the person's answer names no run"
    );
    for block in told.iter().filter(|block| block.event == "thread.changed") {
        let run = &block.data["runId"];
        assert!(
            run.is_null() || run == turn.as_str(),
            "no other run: {block:?}"
        );
    }
    let deleted = told.iter().find(|block| block.event == "routine.changed");
    assert_eq!(deleted.unwrap().data["change"], "deleted");
    assert_eq!(told.last().unwrap().data["state"], "ok");
}

/// A HOOK's run is a `webhook`, as its history says.
#[tokio::test]
async fn a_hooks_run_is_told_webhook() {
    let url = database_or_skip!();
    let h = harness(&url).await;
    let ada = h.person("Ada", None).await;
    let sol = h.hire(&ada, "Sol").await;
    let body = json!({ "coworkerId": sol, "name": "Inbox", "prompt": "triage", "kind": "webhook" });
    let (status, made) = h
        .send(&ada, reqwest::Method::POST, "/schedules", Some(body))
        .await;
    assert_eq!(status, 201, "{made}");
    let id = made["id"].as_str().unwrap().to_string();
    let hook = made["webhook"]["url"].as_str().unwrap();
    let hook = hook.rsplit('/').next().unwrap();
    let key = made["webhook"]["key"].as_str().unwrap();

    let mut sse = h.listen(&ada, None).await;
    assert_eq!(sse.must().await.event, "reset");
    let posted = h
        .client
        .post(format!("{}/hooks/{hook}", h.base))
        .bearer_auth(key)
        .send()
        .await
        .unwrap();
    assert_eq!(posted.status().as_u16(), 202);
    let run = posted.json::<Value>().await.unwrap()["runId"]
        .as_str()
        .unwrap()
        .to_string();
    let told = sse.until(finished(&run)).await;
    let started = told.iter().find(|block| block.event == "run.started");
    assert_eq!(
        started.unwrap().data,
        json!({ "runId": run, "threadId": id, "coworkerId": sol, "routineId": id,
                "cause": "webhook" })
    );
}

/// A BOT'S `run_routine` IS A `bot`: the routine's run names the routine and the cause the history
/// gives it, beside the Bot's own chat turn, which is a `chat`.
#[tokio::test]
async fn a_bots_run_routine_is_told_bot() {
    let url = database_or_skip!();
    let h = harness(&url).await;
    let ada = h.person("Ada", None).await;
    let (luna, sol) = (h.hire(&ada, "Luna").await, h.hire(&ada, "Sol").await);
    let id = h.routine(&ada, &sol, "Standup").await;
    let mut sse = h.listen(&ada, None).await;
    assert_eq!(sse.must().await.event, "reset");

    let (turn, called) = h
        .call(&ada, &luna, "run_routine", json!({ "routine": id }))
        .await;
    let thread = called["thread"].as_str().unwrap();
    // The turn's own end, then the routine's run, which a Bot's call starts and does not wait on.
    let mut told = sse.until(finished(&turn)).await;
    let routine_run = loop {
        if let Some(started) = told
            .iter()
            .find(|block| block.event == "run.started" && block.data["cause"] == "bot")
        {
            break started.data["runId"].as_str().unwrap().to_string();
        }
        told.push(sse.must().await);
    };
    if !told.iter().any(finished(&routine_run)) {
        told.extend(sse.until(finished(&routine_run)).await);
    }

    let own = told
        .iter()
        .find(|block| block.event == "run.started" && block.data["runId"] == turn.as_str());
    assert_eq!(
        own.unwrap().data,
        json!({ "runId": turn, "threadId": thread, "coworkerId": luna, "cause": "chat" })
    );
    let started = told
        .iter()
        .find(|block| block.event == "run.started" && block.data["runId"] == routine_run.as_str());
    assert_eq!(
        started.unwrap().data,
        json!({ "runId": routine_run, "threadId": id, "coworkerId": sol, "routineId": id,
                "cause": "bot" })
    );
    let ended = told.iter().find(|block| finished(&routine_run)(block));
    assert_eq!(ended.unwrap().data["state"], "ok");
    assert_eq!(ended.unwrap().data["routineId"], id.as_str());
}

/// EVERY WRITE TO A ROUTINE IS TOLD, whoever made it: the app's pane (create, edit, pause, resume,
/// delete) and a Bot's tools (`create_routine`, `update_routine`).
#[tokio::test]
async fn every_routine_write_is_told_whether_the_pane_or_a_bot_made_it() {
    let url = database_or_skip!();
    let h = harness(&url).await;
    let ada = h.person("Ada", None).await;
    let luna = h.hire(&ada, "Luna").await;
    let mut sse = h.listen(&ada, None).await;
    assert_eq!(sse.must().await.event, "reset");

    let id = h.routine(&ada, &luna, "Weekly").await;
    let path = format!("/schedules/{id}");
    let edit = Some(json!({ "name": "Weekly, edited" }));
    assert_eq!(
        h.send(&ada, reqwest::Method::PATCH, &path, edit).await.0,
        200
    );
    let pause = h
        .send(&ada, reqwest::Method::POST, &format!("{path}/pause"), None)
        .await;
    assert_eq!(pause.0, 204);
    let resume = h
        .send(&ada, reqwest::Method::POST, &format!("{path}/resume"), None)
        .await;
    assert_eq!(resume.0, 204);
    assert_eq!(
        h.send(&ada, reqwest::Method::DELETE, &path, None).await.0,
        204
    );

    let mut told = Vec::new();
    for _ in 0..5 {
        told.push(sse.must().await);
    }
    let said: Vec<(&str, &str)> = told
        .iter()
        .map(|block| {
            assert_eq!(block.event, "routine.changed");
            assert_eq!(block.data["routineId"], id.as_str());
            assert_eq!(block.data["coworkerId"], luna.as_str());
            (block.event.as_str(), block.data["change"].as_str().unwrap())
        })
        .collect();
    let changes: Vec<&str> = said.iter().map(|(_, change)| *change).collect();
    assert_eq!(
        changes,
        ["created", "updated", "paused", "resumed", "deleted"]
    );

    // A Bot's tools: make one, then change it. The turn that makes the call is told as any turn.
    let made =
        json!({ "name": "Standup", "prompt": "post the standup", "when": "0 9 * * MON-FRI" });
    let (turn, _) = h.call(&ada, &luna, "create_routine", made).await;
    let told = sse.until(finished(&turn)).await;
    let created: Vec<&Block> = told
        .iter()
        .filter(|block| block.event == "routine.changed")
        .collect();
    assert_eq!(created.len(), 1, "{:?}", words(&told));
    assert_eq!(created[0].data["change"], "created");
    assert_eq!(created[0].data["coworkerId"], luna.as_str());
    let routine = created[0].data["routineId"].as_str().unwrap().to_string();

    let edit = json!({ "routine": routine, "name": "Morning standup" });
    let (turn, _) = h.call(&ada, &luna, "update_routine", edit).await;
    let told = sse.until(finished(&turn)).await;
    let updated: Vec<&Block> = told
        .iter()
        .filter(|block| block.event == "routine.changed")
        .collect();
    assert_eq!(updated.len(), 1, "{:?}", words(&told));
    assert_eq!(updated[0].data["change"], "updated");
    assert_eq!(updated[0].data["routineId"], routine.as_str());
}

// ---- whose it is ---------------------------------------------------------------------------

/// NOTES FOLLOW THE OWNER OF THE THING. On a Bot shared with the org, the owner is told of their own
/// turns and routines and not a word of an org-mate's, and the mate of theirs and not the owner's;
/// the two share a thread id (the app names a Bot's chat after the Bot), and it makes no
/// difference. Somebody outside the org is told nothing at all.
#[tokio::test]
async fn on_a_shared_bot_each_person_is_told_only_what_is_theirs() {
    let url = database_or_skip!();
    let h = harness(&url).await;
    let org = unique("org");
    let (ann, ben) = (
        h.person("Ann", Some(&org)).await,
        h.person("Ben", Some(&org)).await,
    );
    let cy = h.person("Cy", Some(&unique("elsewhere"))).await;
    let ada = h.hire(&ann, "Ada").await;
    let shared = json!({ "visibility": "org" });
    let (status, _) = h
        .send(
            &ann,
            reqwest::Method::PATCH,
            &format!("/coworkers/{ada}"),
            Some(shared),
        )
        .await;
    assert_eq!(status, 200);
    let (mut for_ann, mut for_ben, mut for_cy) = (
        h.listen(&ann, None).await,
        h.listen(&ben, None).await,
        h.listen(&cy, None).await,
    );
    for sse in [&mut for_ann, &mut for_ben, &mut for_cy] {
        assert_eq!(sse.must().await.event, "reset");
    }

    // The member's turn: the member is told, the owner and the stranger are not.
    let thread = format!("gateway-{ada}");
    let (bens, _) = h.turn(&ben, &ada, &thread, "hello from ben").await;
    let told = for_ben.until(finished(&bens)).await;
    assert!(
        told.iter()
            .all(|block| block.data["threadId"] == thread.as_str())
    );
    for sse in [&mut for_ann, &mut for_cy] {
        sse.nothing_for(700).await;
    }

    // The owner's, on the same thread id: the owner is told, and the member is not.
    let (anns, _) = h.turn(&ann, &ada, &thread, "hello from ann").await;
    let told = for_ann.until(finished(&anns)).await;
    assert_eq!(told[0].data["runId"], anns.as_str());
    for sse in [&mut for_ben, &mut for_cy] {
        sse.nothing_for(700).await;
    }

    // A routine the owner makes on the shared Bot, and the run they press on it, are the owner's:
    // the member who can chat with the Bot is told nothing of either.
    let routine = h.routine(&ann, &ada, "Ann's own").await;
    let made = for_ann.must().await;
    assert_eq!(made.event, "routine.changed");
    assert_eq!(made.data["routineId"], routine.as_str());
    let path = format!("/schedules/{routine}/run");
    let (status, pressed) = h.send(&ann, reqwest::Method::POST, &path, None).await;
    assert_eq!(status, 202, "{pressed}");
    let run = pressed["runId"].as_str().unwrap();
    let told = for_ann.until(finished(run)).await;
    assert!(told.iter().any(|block| block.event == "run.started"));
    for sse in [&mut for_ben, &mut for_cy] {
        sse.nothing_for(700).await;
    }
}

// ---- coming back ---------------------------------------------------------------------------

/// A connection that went away is replayed what it missed, in order, from the id it last saw, and
/// then followed; and an id that cannot be resumed from (not a number, from the future, or none)
/// begins with `reset`, carrying the head.
#[tokio::test]
async fn a_connection_that_comes_back_hears_what_it_missed_and_nothing_twice() {
    let url = database_or_skip!();
    let h = harness(&url).await;
    let ada = h.person("Ada", None).await;
    let luna = h.hire(&ada, "Luna").await;
    let mut sse = h.listen(&ada, None).await;
    assert_eq!(sse.must().await.event, "reset");

    let (first, _) = h.turn(&ada, &luna, "thread-away", "one").await;
    let seen = sse.until(finished(&first)).await;
    let last = seen.last().unwrap().id;
    drop(sse);

    // While away: another turn, and a routine.
    let (second, _) = h.turn(&ada, &luna, "thread-away", "two").await;
    let routine = h.routine(&ada, &luna, "While away").await;
    let select = "select id, kind from account_event where account_id = $1 and id > $2 order by id";
    let missed: Vec<(i64, String)> = loop {
        let rows: Vec<(i64, String)> = sqlx::query_as(select)
            .bind(ada.id.as_str())
            .bind(last)
            .fetch_all(h.store.pool())
            .await
            .unwrap();
        if rows.iter().any(|(_, kind)| kind == "routine.changed") {
            break rows;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let mut back = h.listen(&ada, Some(&last.to_string())).await;
    let mut replayed = Vec::new();
    for _ in 0..missed.len() {
        replayed.push(back.must().await);
    }
    let heard: Vec<(i64, String)> = replayed
        .iter()
        .map(|block| (block.id, block.event.clone()))
        .collect();
    assert_eq!(
        heard, missed,
        "every note after the last seen, in order, once"
    );
    assert!(replayed.iter().any(finished(&second)));
    assert!(
        replayed
            .iter()
            .any(|b| b.data["routineId"] == routine.as_str())
    );
    back.nothing_for(500).await;

    // Followed on from there.
    let (third, _) = h.turn(&ada, &luna, "thread-away", "three").await;
    let more = back.until(finished(&third)).await;
    assert!(more[0].id > replayed.last().unwrap().id);

    let head = more.last().unwrap().id;
    for refused in ["not-a-number", &(head + 1000).to_string(), "-3"] {
        let mut again = h.listen(&ada, Some(refused)).await;
        let first = again.must().await;
        assert_eq!(
            first.text,
            format!("id: {head}\nevent: reset\ndata: {{}}\n\n"),
            "Last-Event-ID: {refused}"
        );
    }
}

/// A quiet stream pings, on the clock it is tuned to.
#[tokio::test]
async fn a_quiet_stream_pings() {
    let url = database_or_skip!();
    let h = harness(&url).await;
    h.store.events.tune(Tuning {
        ping: Duration::from_millis(100),
        poll: Duration::from_secs(60),
        window: Duration::from_millis(20),
        room: 64,
    });
    let ada = h.person("Ada", None).await;
    let mut sse = h.listen(&ada, None).await;
    assert_eq!(sse.must().await.event, "reset");
    for _ in 0..3 {
        let piece = sse.piece(3_000).await.expect("a ping");
        assert_eq!(piece, ": ping\n\n");
    }
}
