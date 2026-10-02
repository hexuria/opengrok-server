//! A coworker's tool ceiling, read and set by its owner (#268).
//!
//! `GET /coworkers/{id}/ceiling` answers every built-in, the person's own machine and every
//! plugin as a row its owner can switch; `PUT` replaces the ceiling with exactly the rows named.
//! A SWITCH IS ONLY TRUE IF A TURN OBEYS IT, so these read the run path back — what
//! `GET /coworkers/{id}/tools` lists and what the model door is actually offered — rather than
//! trusting the PUT's 200.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::response::IntoResponse;
use opengrok_box::{BoxError, BoxResult, CommandOutput, Computer, StartedCommand};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_harness::{DeltaStream, MockDoor, ModelDoor, ModelError, ModelRequest};
use opengrok_plugins::{McpConfig, McpServer, Plugin, Trust};
use opengrok_policy::ToolSet;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::host_state::HostState;
use opengrok_store::PgStore;
use opengrok_tools::{Executor, USER_MACHINE_SHELL};
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

/// A box that is always up and never asked to do anything: these turns only look at the tools.
struct IdleBox;

#[async_trait]
impl Computer for IdleBox {
    async fn create(&self, _ttl: Option<u64>) -> BoxResult<String> {
        Ok(format!("bx_idle_{}", uuid::Uuid::now_v7().simple()))
    }
    async fn run(&self, _b: &str, _c: &str, _t: u32) -> BoxResult<CommandOutput> {
        Err(BoxError::NoSuchBox)
    }
    async fn start(&self, _b: &str, _c: &str) -> BoxResult<StartedCommand> {
        Err(BoxError::NoSuchBox)
    }
    async fn watch(&self, _b: &str, _p: &str) -> BoxResult<StartedCommand> {
        Err(BoxError::NoSuchBox)
    }
    async fn read_file(&self, _b: &str, _p: &str) -> BoxResult<String> {
        Err(BoxError::NoSuchBox)
    }
    async fn write_file(&self, _b: &str, _p: &str, _c: &str) -> BoxResult<()> {
        Err(BoxError::NoSuchBox)
    }
    async fn expose_port(&self, _b: &str, _p: u16, _t: &str) -> BoxResult<String> {
        Err(BoxError::NoSuchBox)
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

/// The echoing mock, keeping every request so a test can read the tools a turn was offered.
#[derive(Default)]
struct RecordingDoor {
    inner: MockDoor,
    asked: Mutex<Vec<ModelRequest>>,
}

#[async_trait]
impl ModelDoor for RecordingDoor {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
        self.asked.lock().expect("asked").push(request.clone());
        self.inner.stream(request).await
    }
}

/// The smallest MCP server that lists one tool, `jot`: hand-written JSON-RPC, like any third
/// party's.
async fn start_notes_server() -> String {
    let app = axum::Router::new().route(
        "/mcp",
        axum::routing::post(|body: String| async move {
            let request: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let Some(id) = request.get("id").cloned() else {
                return axum::http::StatusCode::ACCEPTED.into_response();
            };
            let result = match request["method"].as_str().unwrap_or_default() {
                "initialize" => json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "notes", "version": "0.1.0" }
                }),
                "tools/list" => json!({ "tools": [{
                    "name": "jot",
                    "description": "Jot a note down",
                    "inputSchema": {
                        "type": "object",
                        "properties": { "text": { "type": "string" } },
                        "required": ["text"]
                    }
                }] }),
                _ => json!({}),
            };
            axum::Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}/mcp")
}

fn plugin(name: &str, description: &str, url: &str, headers: &[(&str, &str)]) -> Plugin {
    Plugin {
        root: std::env::temp_dir(),
        manifest: serde_json::from_value(json!({ "name": name, "description": description }))
            .expect("manifest"),
        mcp: McpConfig {
            schema: None,
            servers: BTreeMap::from([(
                "api".to_string(),
                McpServer::StreamableHttp {
                    url: url.to_string(),
                    headers: headers
                        .iter()
                        .map(|(name, value)| (name.to_string(), value.to_string()))
                        .collect(),
                },
            )]),
        },
        trust: Trust::Verified,
    }
}

async fn seed_account(store: &PgStore, email: &str) -> AccountId {
    let id = AccountId::new();
    let at_ms = chrono::Utc::now().timestamp_millis();
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.to_string(),
            password_hash: "x".to_string(),
            first_name: "Ada".to_string(),
            last_name: String::new(),
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
        password_hash: Some("x".to_string()),
        first_name: "Ada".to_string(),
        last_name: String::new(),
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

/// Every built-in row the ceiling answers, in order: the box's tools, the person's machine and
/// `message_bot` (#314), then the one row of the routine tools (#316) in place of all four.
fn builtins() -> Vec<&'static str> {
    let routine = |name: &&str| opengrok_tools::routine::is_routine_tool(name);
    let rows = Executor::every_builtin().filter(|name| !routine(name));
    rows.chain([opengrok_tools::routine::ROW]).collect()
}

struct Harness {
    base: String,
    agui: AgUiState,
    store: PgStore,
    owner: AccountId,
    token: String,
    door: Arc<RecordingDoor>,
    client: reqwest::Client,
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
    let email = format!("ceiling-{}@og.local", uuid::Uuid::now_v7().simple());
    let owner = seed_account(&store, &email).await;
    // `gmail` needs a connector nobody has connected, so it is never dialled; `notes` is a live
    // server with nothing to connect, so its tool is offered the moment the ceiling admits it.
    // `notes.extra` is a live server too, under a name the spec allows and this server cannot
    // route: its tool would be `notes.extra.api.jot`, which `notes.*` admits by name.
    let plugins = BTreeMap::from([
        (
            "gmail".to_string(),
            plugin(
                "gmail",
                "Read and send mail.",
                "http://127.0.0.1:9/mcp",
                &[("authorization", "Bearer ${GMAIL_TOKEN}")],
            ),
        ),
        (
            "notes".to_string(),
            plugin("notes", "Keep notes.", &start_notes_server().await, &[]),
        ),
        (
            "notes.extra".to_string(),
            plugin(
                "notes.extra",
                "Not notes.",
                &start_notes_server().await,
                &[],
            ),
        ),
    ]);
    let door = Arc::new(RecordingDoor::default());
    let agui = AgUiState {
        auth: AuthState::new(
            store.clone(),
            Arc::new(TokenMinter::new(b"a-tool-ceiling-a-tool-ceiling!!!")),
            email.clone(),
        ),
        door: door.clone(),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer: Some(Arc::new(IdleBox)),
        vault: None,
        connectors: Connectors {
            providers: Arc::new(BTreeMap::new()),
            redirect_uri: "http://127.0.0.1/callback".to_string(),
        },
        plugins: Arc::new(plugins),
        host_settings: None,
    };
    let host = HostState::new(agui.clone(), Some("http://opengrok.lan:1447".to_string()));
    let app = opengrok_server::router(agui.clone(), host);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    let token = mint(&agui, &owner, &email);
    Harness {
        base,
        agui,
        store,
        owner,
        token,
        door,
        client: reqwest::Client::new(),
    }
}

fn mint(agui: &AgUiState, account: &AccountId, email: &str) -> String {
    agui.auth
        .minter
        .mint_access(
            account.as_str(),
            "sess-ceiling",
            email,
            "ultra",
            chrono::Utc::now().timestamp(),
            3600,
        )
        .expect("mint access")
}

impl Harness {
    /// Somebody else entirely, signed in.
    async fn outsider(&self) -> String {
        let email = format!("ceiling-other-{}@og.local", uuid::Uuid::now_v7().simple());
        let account = seed_account(&self.store, &email).await;
        mint(&self.agui, &account, &email)
    }

    /// Any request, answered as status and JSON body.
    async fn call(
        &self,
        token: &str,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let method = reqwest::Method::from_bytes(method.as_bytes()).expect("method");
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base))
            .header("authorization", format!("Bearer {token}"));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.expect("request");
        let status = response.status().as_u16();
        let text = response.text().await.expect("body");
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn hire(&self) -> String {
        let (status, hired) = self
            .call(
                &self.token,
                "POST",
                "/coworkers",
                Some(json!({ "name": "Ada" })),
            )
            .await;
        assert_eq!(status, 201, "{hired}");
        hired["id"].as_str().expect("coworker id").to_string()
    }

    async fn ceiling(&self, agent: &str) -> Value {
        let (status, body) = self
            .call(
                &self.token,
                "GET",
                &format!("/coworkers/{agent}/ceiling"),
                None,
            )
            .await;
        assert_eq!(status, 200, "{body}");
        assert!(
            body["tools"].is_array(),
            "the rows are always an array: {body}"
        );
        assert!(body["version"].is_i64(), "a version is a number: {body}");
        body
    }

    async fn put(&self, agent: &str, body: Value) -> (u16, Value) {
        self.call(
            &self.token,
            "PUT",
            &format!("/coworkers/{agent}/ceiling"),
            Some(body),
        )
        .await
    }

    /// What `GET /coworkers/{id}/tools` says a turn would be offered.
    async fn listed(&self, agent: &str) -> Vec<String> {
        let (status, body) = self
            .call(
                &self.token,
                "GET",
                &format!("/coworkers/{agent}/tools"),
                None,
            )
            .await;
        assert_eq!(status, 200, "{body}");
        names(&body["tools"], |tool| tool["name"].as_str())
    }

    /// What a real turn put in front of the model.
    async fn offered_on_a_turn(&self, agent: &str) -> Vec<String> {
        let (status, _) = self
            .call(
                &self.token,
                "POST",
                "/ag-ui",
                Some(json!({
                    "threadId": format!("thr-{}", uuid::Uuid::now_v7()),
                    "runId": uuid::Uuid::now_v7().to_string(),
                    "messages": [{ "id": "m1", "role": "user", "content": "hello" }],
                    "forwardedProps": { "coworkerId": agent },
                })),
            )
            .await;
        assert_eq!(status, 200, "ag-ui turn");
        let asked = self.door.asked.lock().expect("asked");
        let request = asked.last().expect("the turn asked the model");
        names(&json!(request.tools), |tool| {
            tool["function"]["name"].as_str()
        })
    }
}

fn names(list: &Value, name: impl Fn(&Value) -> Option<&str>) -> Vec<String> {
    list.as_array()
        .expect("an array")
        .iter()
        .filter_map(|item| name(item).map(str::to_string))
        .collect()
}

/// The row named `name`, which must be there exactly once.
fn row<'a>(body: &'a Value, name: &str) -> &'a Value {
    let rows: Vec<&Value> = body["tools"]
        .as_array()
        .expect("rows")
        .iter()
        .filter(|row| row["name"] == name)
        .collect();
    assert_eq!(rows.len(), 1, "exactly one row named {name}: {body}");
    rows[0]
}

fn enabled(body: &Value) -> Vec<String> {
    names(&body["tools"], |row| {
        (row["enabled"] == true)
            .then(|| row["name"].as_str())
            .flatten()
    })
}

#[tokio::test]
async fn the_owner_reads_every_builtin_every_plugin_and_no_machine_to_reach() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let agent = h.hire().await;
    let body = h.ceiling(&agent).await;

    let rows = body["tools"].as_array().expect("rows");
    let every: Vec<&str> = rows.iter().filter_map(|row| row["name"].as_str()).collect();
    let mut expected = builtins();
    expected.extend(["gmail", "notes"]);
    assert_eq!(
        every, expected,
        "built-ins first, then the plugins by name: {body}"
    );

    for name in builtins() {
        let row = row(&body, name);
        assert_eq!(row["kind"], "builtin", "{row}");
        assert!(
            !row["description"].as_str().unwrap_or_default().is_empty(),
            "{row}"
        );
        // A new hire starts where every coworker stood before its ceiling had switches.
        assert_eq!(row["enabled"], true, "{row}");
        let only_the_machine_says = name == USER_MACHINE_SHELL;
        assert_eq!(
            row.get("available").is_some(),
            only_the_machine_says,
            "{row}"
        );
    }
    // No machine enrolled, so there is nothing to reach — said, not left for the client to guess.
    assert_eq!(row(&body, USER_MACHINE_SHELL)["available"], false);

    // The words are the ones a turn gives the model for the same tool, not a second copy.
    let (_, listed) = h
        .call(&h.token, "GET", &format!("/coworkers/{agent}/tools"), None)
        .await;
    let listed = listed["tools"].as_array().expect("tools").clone();
    assert!(!listed.is_empty());
    for tool in listed.iter().filter(|tool| tool["kind"] == "builtin") {
        let name = tool["name"].as_str().expect("name");
        assert_eq!(
            row(&body, name)["description"],
            tool["description"],
            "{name}"
        );
    }

    let gmail = row(&body, "gmail");
    assert_eq!(
        gmail,
        &json!({ "name": "gmail", "kind": "plugin", "label": "gmail",
            "description": "Read and send mail.", "connector": "gmail", "enabled": false })
    );
    // Nothing to connect, so no connector — omitted, not an empty string.
    let notes = row(&body, "notes");
    assert_eq!(
        notes,
        &json!({ "name": "notes", "kind": "plugin", "label": "notes",
            "description": "Keep notes.", "enabled": false })
    );
}

/// The recording NativeChat asked for: one plugin on, the machine not there to reach.
#[tokio::test]
async fn a_ceiling_with_one_plugin_on_reads_it_on_and_the_machine_unavailable() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let agent = h.hire().await;
    let mut on: Vec<&str> = builtins();
    on.push("notes");
    let (status, put) = h.put(&agent, json!({ "enabled": on })).await;
    assert_eq!(status, 200, "{put}");

    let body = h.ceiling(&agent).await;
    assert_eq!(put, body, "a PUT answers what a GET then reads");
    assert_eq!(row(&body, "notes")["enabled"], true);
    assert_eq!(row(&body, "gmail")["enabled"], false);
    assert_eq!(row(&body, USER_MACHINE_SHELL)["available"], false);
    let mut expected: Vec<String> = on.iter().map(|name| name.to_string()).collect();
    expected.sort();
    let mut got = enabled(&body);
    got.sort();
    assert_eq!(got, expected);
}

#[tokio::test]
async fn switching_shell_off_takes_it_from_the_next_turn_and_a_plugin_on_offers_its_tool() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let agent = h.hire().await;
    let before = h.listed(&agent).await;
    assert!(before.contains(&"shell".to_string()), "{before:?}");
    assert!(!before.contains(&"notes_api_jot".to_string()), "{before:?}");

    let (status, put) = h
        .put(
            &agent,
            json!({ "enabled": ["read_file", "write_file", "notes"] }),
        )
        .await;
    assert_eq!(status, 200, "{put}");
    assert_eq!(put, h.ceiling(&agent).await);
    assert_eq!(row(&put, "shell")["enabled"], false);
    assert_eq!(row(&put, "notes")["enabled"], true);

    // The run path reads the same row: shell is gone from the listing and from a real turn, and
    // the plugin's tool — named by nobody until its server was dialled — is offered.
    let listed = h.listed(&agent).await;
    assert!(!listed.contains(&"shell".to_string()), "{listed:?}");
    assert!(listed.contains(&"read_file".to_string()), "{listed:?}");
    assert!(listed.contains(&"notes_api_jot".to_string()), "{listed:?}");
    // `notes.*` admits `notes.extra.api.jot` by name, but `notes.extra` is not `notes`: a plugin
    // with a dot in its name is never dialled, so nothing is offered in another plugin's name.
    assert!(
        !listed.iter().any(|name| name.starts_with("notes_extra")),
        "{listed:?}"
    );
    let offered = h.offered_on_a_turn(&agent).await;
    assert!(!offered.contains(&"shell".to_string()), "{offered:?}");
    assert!(
        offered.contains(&"notes_api_jot".to_string()),
        "{offered:?}"
    );

    // Switched back, it comes back.
    let (status, _) = h.put(&agent, json!({ "enabled": ["shell"] })).await;
    assert_eq!(status, 200);
    let listed = h.listed(&agent).await;
    assert!(listed.contains(&"shell".to_string()), "{listed:?}");
    assert!(!listed.contains(&"notes_api_jot".to_string()), "{listed:?}");
}

#[tokio::test]
async fn an_empty_ceiling_switches_every_row_off_and_offers_nothing() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let agent = h.hire().await;
    let (status, put) = h.put(&agent, json!({ "enabled": [] })).await;
    assert_eq!(status, 200, "{put}");
    assert!(enabled(&put).is_empty(), "{put}");
    assert_eq!(put, h.ceiling(&agent).await);
    assert!(h.listed(&agent).await.is_empty());
    let policy = h
        .store
        .policy_for(&h.owner, &CoworkerId::from_stored(agent.clone()))
        .await
        .expect("policy");
    assert_eq!(
        policy.ceiling.map(|ceiling| ceiling.tools),
        Some(ToolSet::None)
    );
}

#[tokio::test]
async fn a_name_no_row_shows_is_refused_and_nothing_changes() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let agent = h.hire().await;
    let before = h.ceiling(&agent).await;

    let (status, refused) = h
        .put(&agent, json!({ "enabled": ["read_file", "gmial"] }))
        .await;
    assert_eq!(status, 422, "{refused}");
    assert_eq!(refused, json!({ "error": "no tool or plugin named gmial" }));
    // A plugin's tool is not a row: only the plugin is. Nor is a plugin whose name has a dot.
    for name in ["notes.api.jot", "notes.extra"] {
        let (status, refused) = h.put(&agent, json!({ "enabled": [name] })).await;
        assert_eq!(status, 422, "{name}: {refused}");
    }

    for malformed in [
        json!({}),
        json!({ "enable": ["shell"] }),
        json!({ "enabled": "shell" }),
    ] {
        let (status, refused) = h.put(&agent, malformed.clone()).await;
        assert_eq!(status, 422, "{malformed}: {refused}");
        assert!(
            !refused["error"].as_str().unwrap_or_default().is_empty(),
            "{malformed}: {refused}"
        );
    }
    let response = h
        .client
        .put(format!("{}/coworkers/{agent}/ceiling", h.base))
        .header("authorization", format!("Bearer {}", h.token))
        .header("content-type", "application/json")
        .body("not json")
        .send()
        .await
        .expect("put");
    assert_eq!(response.status().as_u16(), 422);

    assert_eq!(h.ceiling(&agent).await, before, "nothing changed");
}

#[tokio::test]
async fn another_account_is_told_no_such_coworker_on_both_verbs() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let agent = h.hire().await;
    let before = h.ceiling(&agent).await;
    let outsider = h.outsider().await;
    let path = format!("/coworkers/{agent}/ceiling");
    let no_such = json!({ "error": "no such coworker" });

    let (status, body) = h.call(&outsider, "GET", &path, None).await;
    assert_eq!((status, body), (404, no_such.clone()));
    let (status, body) = h
        .call(&outsider, "PUT", &path, Some(json!({ "enabled": [] })))
        .await;
    assert_eq!((status, body), (404, no_such.clone()));
    // Not even a malformed body tells it the coworker exists.
    let (status, body) = h.call(&outsider, "PUT", &path, Some(json!({}))).await;
    assert_eq!((status, body), (404, no_such.clone()));
    // An id nobody has reads the same.
    let (status, body) = h
        .call(&h.token, "GET", "/coworkers/cw_nobody/ceiling", None)
        .await;
    assert_eq!((status, body), (404, no_such));

    assert_eq!(h.ceiling(&agent).await, before);
}

#[tokio::test]
async fn a_plugin_no_longer_loaded_stays_listed_unavailable_and_can_be_switched_off() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let agent = h.hire().await;
    let coworker = CoworkerId::from_stored(agent.clone());
    // Switched on while a plugin called `gone` was installed; this server no longer loads it.
    let ceiling = ToolSet::only(["shell".to_string(), opengrok_policy::every_tool_of("gone")]);
    h.store
        .grant_access(&h.owner, &coworker, &ceiling, &ceiling, &ToolSet::None, 1)
        .await
        .expect("an older ceiling");

    let body = h.ceiling(&agent).await;
    assert_eq!(
        row(&body, "gone"),
        &json!({ "name": "gone", "kind": "plugin", "enabled": true, "available": false }),
        "kept by name, with nothing the server no longer knows"
    );

    // It may be named to keep it...
    let (status, kept) = h.put(&agent, json!({ "enabled": ["shell", "gone"] })).await;
    assert_eq!(status, 200, "{kept}");
    assert_eq!(row(&kept, "gone")["enabled"], true);
    // ...but a plugin this server does not load cannot be added by name.
    let (status, refused) = h
        .put(&agent, json!({ "enabled": ["shell", "gone", "vanished"] }))
        .await;
    assert_eq!(status, 422, "{refused}");
    assert_eq!(refused["error"], "no tool or plugin named vanished");

    let (status, off) = h.put(&agent, json!({ "enabled": ["shell"] })).await;
    assert_eq!(status, 200, "{off}");
    assert!(
        !off["tools"]
            .as_array()
            .expect("rows")
            .iter()
            .any(|row| row["name"] == "gone"),
        "switched off, it has nothing left to show: {off}"
    );
    let policy = h
        .store
        .policy_for(&h.owner, &coworker)
        .await
        .expect("policy");
    assert_eq!(
        policy.ceiling.map(|ceiling| ceiling.tools),
        Some(ToolSet::only(["shell"]))
    );
}

/// The person's machine is the one tool a ceiling did not govern before #268: its row is only
/// true if switching it off takes it away from the turn.
#[tokio::test]
async fn the_machine_row_is_the_switch_a_turn_reads() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let agent = h.hire().await;
    let (status, enrolled) = h
        .call(
            &h.token,
            "POST",
            "/local-exec/daemon",
            Some(json!({ "label": "Ada's Mac" })),
        )
        .await;
    assert!((200..300).contains(&status), "{enrolled}");
    let machine = enrolled["machineId"].as_str().expect("machine id");
    let (status, body) = h
        .call(
            &h.token,
            "PUT",
            "/local-exec/policy",
            Some(json!({ "machineId": machine, "mode": "ask" })),
        )
        .await;
    assert!((200..300).contains(&status), "{body}");

    let body = h.ceiling(&agent).await;
    let row_of = |body: &Value| row(body, USER_MACHINE_SHELL).clone();
    assert_eq!(row_of(&body)["available"], true);
    assert_eq!(row_of(&body)["enabled"], true);
    assert!(
        h.listed(&agent)
            .await
            .contains(&USER_MACHINE_SHELL.to_string())
    );

    let off: Vec<&str> = builtins()
        .into_iter()
        .filter(|name| *name != USER_MACHINE_SHELL)
        .collect();
    let (status, put) = h.put(&agent, json!({ "enabled": off })).await;
    assert_eq!(status, 200, "{put}");
    assert_eq!(row_of(&put)["enabled"], false);
    assert_eq!(
        row_of(&put)["available"],
        true,
        "still there, just not this coworker's"
    );
    let listed = h.listed(&agent).await;
    assert!(
        !listed.contains(&USER_MACHINE_SHELL.to_string()),
        "{listed:?}"
    );
    assert!(listed.contains(&"shell".to_string()), "{listed:?}");
    let offered = h.offered_on_a_turn(&agent).await;
    assert!(
        !offered.contains(&USER_MACHINE_SHELL.to_string()),
        "{offered:?}"
    );

    let (status, _) = h
        .put(&agent, json!({ "enabled": [USER_MACHINE_SHELL] }))
        .await;
    assert_eq!(status, 200);
    assert!(
        h.listed(&agent)
            .await
            .contains(&USER_MACHINE_SHELL.to_string())
    );
}

/// Two screens read the same ceiling and both save. The second was looking at a ceiling that no
/// longer exists, so it is told so — and the first one's choice is what stands.
#[tokio::test]
async fn two_puts_from_the_same_read_land_once_and_the_second_is_told_the_tools_changed() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let agent = h.hire().await;
    let read = h.ceiling(&agent).await["version"]
        .as_i64()
        .expect("version");

    let (status, first) = h
        .put(
            &agent,
            json!({ "enabled": ["read_file", "shell"], "version": read }),
        )
        .await;
    assert_eq!(status, 200, "{first}");
    assert_eq!(first["version"], read + 1, "a change moves the version on");

    let (status, second) = h
        .put(
            &agent,
            json!({ "enabled": ["write_file"], "version": read }),
        )
        .await;
    assert_eq!(status, 409, "{second}");
    assert_eq!(
        second,
        json!({ "error": "the tools changed since you looked", "code": "ceiling-changed" })
    );
    let now = h.ceiling(&agent).await;
    assert_eq!(
        now, first,
        "the first PUT stands, and nothing of the second landed"
    );

    // The current version is accepted, and moves on again.
    let (status, third) = h
        .put(
            &agent,
            json!({ "enabled": ["read_file"], "version": read + 1 }),
        )
        .await;
    assert_eq!(status, 200, "{third}");
    assert_eq!(third["version"], read + 2);
    // Saving what is already there is not a change, so nobody else's version goes stale.
    let (status, same) = h
        .put(
            &agent,
            json!({ "enabled": ["read_file"], "version": read + 2 }),
        )
        .await;
    assert_eq!(
        (status, &same["version"]),
        (200, &json!(read + 2)),
        "{same}"
    );
    // Without a version the write is unconditional, as before there was one.
    let (status, unversioned) = h.put(&agent, json!({ "enabled": ["shell"] })).await;
    assert_eq!(status, 200, "{unversioned}");
    assert_eq!(unversioned["version"], read + 3);
    let (status, refused) = h
        .put(&agent, json!({ "enabled": ["shell"], "version": "latest" }))
        .await;
    assert_eq!(status, 422, "{refused}");
}

/// A 403 from these routes is a sentence the app can show, like every other refusal on them, and
/// both verbs give it: a withdrawn grant is not its owner's to read the switches of, nor to widen.
#[tokio::test]
async fn a_withdrawn_grant_is_refused_in_words_and_nothing_changes() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let agent = h.hire().await;
    let coworker = CoworkerId::from_stored(agent.clone());
    let before = h.store.ceiling_at(&coworker).await.expect("before");
    h.store
        .revoke_access(&h.owner, &coworker, 1)
        .await
        .expect("withdraw the grant");

    let path = format!("/coworkers/{agent}/ceiling");
    for (method, body) in [
        ("GET", None),
        ("PUT", Some(json!({ "enabled": ["shell"] }))),
    ] {
        let (status, refused) = h.call(&h.token, method, &path, body).await;
        assert_eq!(status, 403, "{method}: {refused}");
        assert!(
            refused["error"]
                .as_str()
                .is_some_and(|why| why.contains("revoked")),
            "{method}: {refused}"
        );
    }
    let after = h.store.ceiling_at(&coworker).await.expect("after");
    assert_eq!(after, before, "nothing changed");
}

/// Saving what is already there is not a choice: the ceiling goes on following the built-ins, so a
/// later built-in still reaches it at boot. Only a save that changes something is the owner's.
#[tokio::test]
async fn saving_the_ceiling_unchanged_does_not_make_it_a_choice() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let agent = h.hire().await;
    let (status, same) = h.put(&agent, json!({ "enabled": builtins() })).await;
    assert_eq!(status, 200, "{same}");
    assert!(!chosen(&h.store, &agent).await, "nothing changed");
    let (status, changed) = h.put(&agent, json!({ "enabled": ["shell"] })).await;
    assert_eq!(status, 200, "{changed}");
    assert!(chosen(&h.store, &agent).await, "a change is a choice");
}

async fn chosen(store: &PgStore, agent: &str) -> bool {
    sqlx::query_scalar("select chosen from ceiling_view where coworker_id = $1")
        .bind(agent)
        .fetch_one(store.pool())
        .await
        .expect("chosen")
}

/// Intended: a built-in that is not there yet can still be switched on. The ceiling records the
/// intent; a turn offers the tool only once there is a machine to reach.
#[tokio::test]
async fn the_machine_can_be_switched_on_before_there_is_one_to_reach() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let agent = h.hire().await;
    let (status, _) = h.put(&agent, json!({ "enabled": ["shell"] })).await;
    assert_eq!(status, 200);

    let (status, on) = h
        .put(&agent, json!({ "enabled": ["shell", USER_MACHINE_SHELL] }))
        .await;
    assert_eq!(status, 200, "{on}");
    let machine = row(&on, USER_MACHINE_SHELL);
    assert_eq!(machine["enabled"], true, "{machine}");
    assert_eq!(machine["available"], false, "{machine}");
    let listed = h.listed(&agent).await;
    assert!(
        !listed.contains(&USER_MACHINE_SHELL.to_string()),
        "{listed:?}"
    );
    assert!(listed.contains(&"shell".to_string()), "{listed:?}");
}

/// A choice that happens to equal an older built-in set is still a choice: the boot-time pass that
/// brings old default rows up to date must not switch back on what its owner switched off.
#[tokio::test]
async fn a_chosen_ceiling_equal_to_an_older_builtin_set_is_not_widened_at_boot() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let agent = h.hire().await;
    let older = ["read_file", "shell", "write_file"];
    let (status, chosen) = h.put(&agent, json!({ "enabled": older })).await;
    assert_eq!(status, 200, "{chosen}");

    opengrok_store::migrations::run(h.store.pool())
        .await
        .expect("a boot");
    assert_eq!(
        h.ceiling(&agent).await,
        chosen,
        "the boot left the choice alone"
    );
    // And the owner's profile, set equal to it, is not widened past it either.
    let coworker = CoworkerId::from_stored(agent.clone());
    let policy = h
        .store
        .policy_for(&h.owner, &coworker)
        .await
        .expect("policy");
    let profile = policy.grant.map(|grant| grant.profile);
    assert_eq!(profile, Some(ToolSet::only(older)), "nor its profile");
}

/// THE RACE THIS PINS: `POST /coworkers/{id}/approvals` read the grant and the ceiling, then wrote
/// them back beside what needs a human yes. When a ceiling save landed between its read and its
/// write, the write put the old ceiling back: `shell`, switched off, came back on. Here the save
/// is held open on the owner's grant row, written as `set_ceiling` writes it; the approvals save
/// reads while it is open, and its write waits on that row until the save commits.
#[tokio::test]
async fn an_approvals_save_that_read_before_a_ceiling_save_does_not_undo_it() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let agent = h.hire().await;
    assert!(h.listed(&agent).await.contains(&"shell".to_string()));

    let narrow = serde_json::to_value(ToolSet::only(["read_file"])).expect("narrow");
    let mut save = h.store.pool().begin().await.expect("begin the save");
    sqlx::query("update grant_view set profile = $3 where principal_id = $1 and coworker_id = $2")
        .bind(h.owner.as_str())
        .bind(agent.as_str())
        .bind(&narrow)
        .execute(&mut *save)
        .await
        .expect("the owner's profile");
    sqlx::query(
        "update ceiling_view set tools = $2, version = version + 1, chosen = true
         where coworker_id = $1",
    )
    .bind(agent.as_str())
    .bind(&narrow)
    .execute(&mut *save)
    .await
    .expect("the ceiling");

    let approvals = tokio::spawn({
        let request = h
            .client
            .post(format!("{}/coworkers/{agent}/approvals", h.base))
            .bearer_auth(&h.token)
            .json(&json!({ "tools": ["read_file"] }));
        async move {
            request
                .send()
                .await
                .map(|response| response.status().as_u16())
        }
    });
    // Its write is waiting on the row the save holds, so its read has been made.
    let mut polls = 0;
    loop {
        let waiting: i64 = sqlx::query_scalar(
            "select count(*) from pg_stat_activity
             where datname = current_database() and wait_event_type = 'Lock'
               and query like '%grant_view%'",
        )
        .fetch_one(h.store.pool())
        .await
        .expect("who is waiting");
        if waiting > 0 {
            break;
        }
        polls += 1;
        assert!(polls < 500, "the approvals save never reached its write");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    save.commit().await.expect("the ceiling save lands");
    let status = approvals.await.expect("join").expect("approvals");
    assert_eq!(status, 200);

    let body = h.ceiling(&agent).await;
    assert_eq!(enabled(&body), vec!["read_file".to_string()], "{body}");
    assert!(!h.listed(&agent).await.contains(&"shell".to_string()));
    let offered = h.offered_on_a_turn(&agent).await;
    assert!(!offered.contains(&"shell".to_string()), "{offered:?}");
}
