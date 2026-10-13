//! A form-mode card (#220, Tier 1): minted from a stand-in BIR plugin's card tool without parking
//! the turn, pressed through `/ag-ui/user-form/action`, submitted on the person's yes, and kept as
//! a read-only view or a dismissed draft with Resume.
//!
//! THE PLUGIN IS A HAND-WRITTEN STAND-IN, as in `opengrok-tools`'s `against_a_stand_in_mcp_server`:
//! raw JSON-RPC answering the contract's tool names with the contract's reply shapes
//! (`CardReply`, `SubmitReply`), so what is proved is that the server speaks the contract, not
//! that two halves of one library agree.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL. Its frames and bodies are the form-mode
//! fixtures of the wire corpus (`scripts/record-wire.sh`).

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::response::IntoResponse;
use futures::stream;
use opengrok_box::{BoxError, BoxResult, CommandOutput, Computer, StartedCommand};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_harness::{DeltaStream, ModelDelta, ModelDoor, ModelError, ModelRequest};
use opengrok_plugins::{McpConfig, McpServer, Plugin, Trust};
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

const TAX14: &str = "frm0619E:txtTax14";
const TAX16: &str = "frm0619E:txtTax16";
const WITHHELD: &str = "frm0619E:rdoTaxWithheld";

/// What the stand-in BIR holds: the draft's values, its queue, and every call it was asked.
#[derive(Default)]
struct Bir {
    values: Mutex<BTreeMap<String, String>>,
    saves: Mutex<u32>,
    queued: Mutex<Option<String>>,
    calls: Mutex<Vec<(String, Value)>>,
}

impl Bir {
    fn fingerprint(&self) -> String {
        format!("sha256:draft-{}", self.saves.lock().unwrap())
    }

    fn calls_to(&self, tool: &str) -> Vec<Value> {
        let calls = self.calls.lock().unwrap();
        calls
            .iter()
            .filter(|(name, _)| name == tool)
            .map(|(_, arguments)| arguments.clone())
            .collect()
    }

    /// A CardReply, as the contract spells it.
    fn card(&self, form: &Value) -> Value {
        let values = self.values.lock().unwrap().clone();
        let tax14 = values.get(TAX14).cloned().unwrap_or_default();
        let first_error = if tax14.is_empty() {
            json!({ "key": TAX14, "message": "Please fill up Part II - Tax Remittance if item 4 is set to Yes." })
        } else {
            Value::Null
        };
        json!({
            "form": form,
            "title": "Form 0619-E",
            "fingerprint": self.fingerprint(),
            "pages": [{ "n": 1, "title": "Part II · Tax Remittance" }],
            "fields": [
                { "key": WITHHELD, "label": "Any Taxes Withheld?", "kind": "radio", "page": 1,
                  "part": "I", "line": "4",
                  "options": [{ "value": "Y", "label": "Yes" }, { "value": "N", "label": "No" }],
                  "value": values.get(WITHHELD).cloned().unwrap_or_else(|| "Y".to_string()),
                  "required": true, "read_only": false },
                { "key": TAX14, "label": "Amount of Remittance", "kind": "text", "page": 1,
                  "part": "II", "line": "14", "value": tax14, "required": true, "read_only": false },
                { "key": TAX16, "label": "Net Amount of Remittance", "kind": "text", "page": 1,
                  "part": "II", "line": "16", "value": values.get(TAX14).cloned().unwrap_or_else(|| "0.00".to_string()),
                  "required": false, "read_only": true }
            ],
            "first_error": first_error,
            "notes": [],
            "status": if self.queued.lock().unwrap().is_some() { "queued" } else { "draft" },
        })
    }

    fn answer(&self, tool: &str, arguments: &Value) -> Value {
        self.calls
            .lock()
            .unwrap()
            .push((tool.to_string(), arguments.clone()));
        let form = arguments.get("form").cloned().unwrap_or(Value::Null);
        match tool {
            "bir_form_card" => self.card(&form),
            "bir_form_save" => {
                if let Some(values) = arguments.get("values").and_then(Value::as_object) {
                    let mut held = self.values.lock().unwrap();
                    for (key, value) in values {
                        held.insert(key.clone(), value.as_str().unwrap_or_default().to_string());
                    }
                }
                *self.saves.lock().unwrap() += 1;
                self.card(&form)
            }
            "bir_form_submit" => {
                let seen = arguments.get("fingerprint").and_then(Value::as_str);
                if seen != Some(self.fingerprint().as_str()) {
                    return json!({ "queued": false, "fingerprint": self.fingerprint(),
                        "first_error": { "key": "", "message": "The draft changed since you saw it." } });
                }
                let card = self.card(&form);
                if !card["first_error"].is_null() {
                    return json!({ "queued": false, "fingerprint": self.fingerprint(),
                        "first_error": card["first_error"] });
                }
                *self.queued.lock().unwrap() = Some("q_0619E_2026-09".to_string());
                json!({ "queued": true, "queue_ref": "q_0619E_2026-09",
                    "fingerprint": self.fingerprint(), "values": self.values.lock().unwrap().clone() })
            }
            "bir_form_preview" => {
                json!({ "html": "<!doctype html><title>0619-E</title><p>12,500.00</p>" })
            }
            "bir_form_focus" => json!({ "ok": true, "scrolled": false }),
            _ => json!({ "ok": true }),
        }
    }
}

fn tool(name: &str) -> Value {
    json!({ "name": name, "description": name, "inputSchema": { "type": "object" } })
}

/// The stand-in `bir-mcp`: hand-written JSON-RPC over streamable HTTP.
async fn start_bir(bir: Arc<Bir>) -> String {
    let app = axum::Router::new().route(
        "/mcp",
        axum::routing::post(move |body: String| {
            let bir = bir.clone();
            async move {
                let request: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                let Some(id) = request.get("id").cloned() else {
                    return axum::http::StatusCode::ACCEPTED.into_response();
                };
                let result = match request["method"].as_str().unwrap_or_default() {
                    "initialize" => json!({
                        "protocolVersion": "2025-11-25",
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "bir-mcp", "version": "0.1.0" }
                    }),
                    "tools/list" => json!({ "tools": [
                        tool("bir_form_card"), tool("bir_form_save"), tool("bir_form_submit"),
                        tool("bir_form_preview"), tool("bir_form_focus"), tool("bir_queue_list"),
                        tool("bir_queue_cancel"), tool("bir_queue_send_now")
                    ] }),
                    "tools/call" => {
                        let name = request["params"]["name"].as_str().unwrap_or_default();
                        let arguments = request["params"]["arguments"].clone();
                        let reply = bir.answer(name, &arguments);
                        json!({
                            "content": [{ "type": "text", "text": reply.to_string() }],
                            "isError": false
                        })
                    }
                    _ => json!({}),
                };
                axum::Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response()
            }
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

/// The BIR plugin as the contract ships it: `extensions.formMode` on `bir-mcp`.
fn bir_plugin(url: &str) -> Plugin {
    Plugin {
        root: std::env::temp_dir(),
        manifest: serde_json::from_value(json!({
            "name": "bir",
            "extensions": { "formMode": {
                "server": "bir-mcp",
                "card": "bir_form_card",
                "actions": { "save": "bir_form_save", "submit": "bir_form_submit",
                             "view": "bir_form_preview", "focus": "bir_form_focus",
                             "refresh": "bir_form_card" },
                "alwaysAsk": ["bir_form_submit", "bir_queue_send_now"]
            } }
        }))
        .expect("manifest"),
        mcp: McpConfig {
            schema: None,
            servers: BTreeMap::from([(
                "bir-mcp".to_string(),
                McpServer::StreamableHttp {
                    url: url.to_string(),
                    headers: BTreeMap::new(),
                },
            )]),
        },
        skills: BTreeMap::new(),
        files: BTreeMap::new(),
        trust: Trust::Verified,
    }
}

/// A model that opens the form when asked, submits it itself when told to, and otherwise ends its
/// turn. Keyed off the conversation, never a counter: the door is shared by every turn.
struct FormModeModel;

#[async_trait]
impl ModelDoor for FormModeModel {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
        let last = request.messages.last();
        let asked = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == "user")
            .map(|message| message.content.clone())
            .unwrap_or_default();
        let script = if last.is_some_and(|message| message.role == "tool") {
            vec![ModelDelta::Text("The form is open.".to_string())]
        } else if asked.contains("submit it yourself") {
            call(
                "bir_bir-mcp_bir_form_submit",
                json!({ "form": form_key(), "fingerprint": "sha256:draft-1" }),
            )
        } else if asked.contains("0619") {
            call(
                "request_user_form",
                json!({ "formMode": true, "title": "BIR 0619-E", "form": form_key(),
                        "values": { TAX14: "999" }, "fingerprint": "sha256:forged" }),
            )
        } else {
            vec![ModelDelta::Text("Nothing to do.".to_string())]
        };
        Ok(Box::pin(stream::iter(script.into_iter().map(Ok))))
    }
}

fn form_key() -> Value {
    json!({ "form": "0619E", "tin": "123456789000", "period": "2026-09" })
}

fn call(name: &str, arguments: Value) -> Vec<ModelDelta> {
    let id = format!("call-{}", uuid::Uuid::now_v7().simple());
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
}

/// A box that is up and never asked for anything: form mode does not touch it.
struct IdleBox;

#[async_trait]
impl Computer for IdleBox {
    async fn create(&self, _ttl: Option<u64>) -> BoxResult<String> {
        Ok(format!("bx_form_mode_{}", uuid::Uuid::now_v7().simple()))
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

async fn seed_account(store: &PgStore, email: &str) -> AccountId {
    let id = AccountId::new();
    let at_ms = chrono::Utc::now().timestamp_millis();
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.to_string(),
            password_hash: "x".to_string(),
            first_name: "Juan".to_string(),
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
        first_name: "Juan".to_string(),
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

struct Harness {
    base: String,
    store: PgStore,
    account: AccountId,
    agent: String,
    token: String,
    client: reqwest::Client,
    bir: Arc<Bir>,
}

async fn harness(database_url: &str) -> Harness {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let email = format!("form-mode-{}@og.local", uuid::Uuid::now_v7().simple());
    let account = seed_account(&store, &email).await;
    let bir = Arc::new(Bir::default());
    let url = start_bir(bir.clone()).await;
    let auth = AuthState::new(
        store.clone(),
        Arc::new(TokenMinter::new(b"form-mode-secret-form-mode-secret")),
        email.clone(),
    );
    let agui = AgUiState {
        auth,
        door: Arc::new(FormModeModel),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer: Some(Arc::new(IdleBox)),
        vault: None,
        connectors: Connectors {
            providers: Arc::new(BTreeMap::new()),
            redirect_uri: "http://127.0.0.1/callback".to_string(),
        },
        plugins: Arc::new(BTreeMap::from([("bir".to_string(), bir_plugin(&url))])),
        host_settings: None,
    };
    let gateway = HostState::new(agui.clone(), Some("http://opengrok.lan:1447".to_string()));
    let app = opengrok_server::router(agui.clone(), gateway);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    let token = agui
        .auth
        .minter
        .mint_access(
            account.as_str(),
            "sess-test",
            &email,
            "ultra",
            chrono::Utc::now().timestamp(),
            3600,
        )
        .expect("mint access");
    let client = reqwest::Client::new();
    let hired: Value = client
        .post(format!("{base}/coworkers"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({ "name": "Ada" }))
        .send()
        .await
        .expect("hire")
        .json()
        .await
        .expect("hire json");
    let agent = hired["id"].as_str().expect("coworker id").to_string();
    // Everything allowed and nothing marked for approval: what asks below asks because the
    // plugin's alwaysAsk says so, and for no other reason.
    store
        .grant_access(
            &account,
            &CoworkerId::from_stored(agent.clone()),
            &opengrok_policy::ToolSet::All,
            &opengrok_policy::ToolSet::All,
            &opengrok_policy::ToolSet::None,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .expect("grant");
    Harness {
        base,
        store,
        account,
        agent,
        token,
        client,
        bir,
    }
}

impl Harness {
    async fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        let response = self
            .client
            .post(format!("{}{path}", self.base))
            .header("authorization", format!("Bearer {}", self.token))
            .json(body)
            .send()
            .await
            .expect("post");
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    /// One AG-UI turn, its frames parsed.
    async fn turn(&self, thread: &str, text: &str, form_mode: bool) -> Vec<Value> {
        let mut props = json!({ "coworkerId": self.agent });
        if form_mode {
            props["formMode"] = json!(true);
        }
        let body = self
            .client
            .post(format!("{}/ag-ui", self.base))
            .header("authorization", format!("Bearer {}", self.token))
            .json(&json!({
                "threadId": thread,
                "runId": uuid::Uuid::now_v7().to_string(),
                "messages": [{ "id": format!("m-{}", uuid::Uuid::now_v7().simple()), "role": "user", "content": text }],
                "forwardedProps": props,
            }))
            .send()
            .await
            .expect("turn")
            .text()
            .await
            .expect("frames");
        body.lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str(data).ok())
            .collect()
    }

    async fn entry(&self, entry_id: &str) -> Value {
        let coworker = CoworkerId::from_stored(self.agent.clone());
        self.store
            .find_gateway_entry(&coworker, &self.account, entry_id)
            .await
            .expect("entry")
            .expect("the card is in the transcript")
            .1
    }

    async fn act(&self, entry_id: &str, action: &str, extra: Value) -> (u16, Value) {
        let mut body = json!({ "entryId": entry_id, "agentId": self.agent, "action": action });
        if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
            body.extend(extra.clone());
        }
        self.post("/ag-ui/user-form/action", &body).await
    }

    /// Open a card in a turn of its own and return its entry id.
    async fn open_card(&self, thread: &str) -> (String, Vec<Value>) {
        let frames = self
            .turn(thread, "open my 0619-E for September", true)
            .await;
        let card = frames
            .iter()
            .find(|frame| frame["type"] == "CUSTOM" && frame["name"] == "user-form")
            .unwrap_or_else(|| panic!("no form-mode card was streamed: {frames:#?}"));
        let id = card["entryId"].as_str().expect("entry id").to_string();
        (id, frames)
    }
}

/// Ids and clocks that differ on every run, by key: placeholders in the fixtures, so the bytes
/// the three repos hash are the same whenever the shape is.
const VOLATILE: [&str; 10] = [
    "entryId",
    "threadId",
    "runId",
    "toolCallId",
    "requestCallId",
    "callId",
    "messageId",
    "timestamp",
    "timestampMs",
    "atMs",
];

fn normalised(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| {
                    // An entry's own id is minted per run; a field's id is the plugin's key for
                    // its box, which is the contract and stays.
                    let minted =
                        key == "id" && value.as_str().is_some_and(|id| id.starts_with("e_"));
                    let kept = match (VOLATILE.contains(&key.as_str()) || minted, value) {
                        (true, Value::Number(_)) => json!(0),
                        (true, Value::String(_)) => json!(format!("<{key}>")),
                        _ => normalised(value),
                    };
                    (key.clone(), kept)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(normalised).collect()),
        other => other.clone(),
    }
}

/// The form-mode fixtures (`tests/fixtures/form-mode/`): the shapes the app and the BIR plugin
/// vendor, and the bytes `scripts/tier-gate.sh` hashes across the three repos. Written with
/// `OG_RECORD_FORM_MODE=1`, otherwise compared: a change to the card's wire is a change to the
/// contract, and it has to be recorded here on purpose.
fn fixture(name: &str, value: &Value) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/form-mode")
        .join(format!("{name}.json"));
    let text = serde_json::to_string_pretty(&normalised(value)).expect("json") + "\n";
    if std::env::var_os("OG_RECORD_FORM_MODE").is_some_and(|on| !on.is_empty()) {
        std::fs::create_dir_all(path.parent().expect("dir")).expect("fixtures dir");
        std::fs::write(&path, text).expect("write fixture");
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "no fixture {}: record it with OG_RECORD_FORM_MODE=1",
            path.display()
        )
    });
    assert_eq!(
        committed, text,
        "the form-mode wire changed; if on purpose, record it with OG_RECORD_FORM_MODE=1"
    );
}

/// The card is minted from the plugin, not the model, without parking the turn; its frames carry
/// its id; the model's smuggled values and fingerprint never reach the plugin or the entry.
#[tokio::test]
async fn a_form_mode_card_is_minted_from_the_plugin_without_parking_the_turn() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let (entry_id, frames) = h.open_card(&h.agent.clone()).await;

    let result = frames
        .iter()
        .find(|frame| frame["type"] == "TOOL_CALL_RESULT")
        .expect("the call answered at once");
    assert_eq!(
        result["content"],
        json!(opengrok_tools::user_form::form_mode::FORM_OPEN)
    );
    assert_eq!(result["entryId"], json!(entry_id), "{result}");
    assert!(
        frames
            .iter()
            .filter(|frame| frame["type"] == "TOOL_CALL_START")
            .all(|frame| frame["entryId"] == json!(entry_id)),
        "the call's frames carry the card's id: {frames:#?}"
    );
    assert!(
        !frames
            .iter()
            .any(|frame| frame["name"] == "run-awaiting-approval"),
        "a form-mode card parks nothing: {frames:#?}"
    );
    assert!(
        frames.iter().any(|frame| frame["type"] == "RUN_FINISHED"),
        "the turn ends"
    );
    let awaiting = h
        .store
        .awaiting_approval(&h.account)
        .await
        .expect("awaiting");
    assert!(awaiting.is_empty(), "no run is parked: {awaiting:?}");

    // A cold load draws what it drew live: the card once, and the call's frames joined to it.
    let run_id = frames
        .iter()
        .find(|frame| frame["type"] == "RUN_STARTED")
        .and_then(|frame| frame["runId"].as_str())
        .expect("run id")
        .to_string();
    let replay: Value = h
        .client
        .get(format!("{}/ag-ui/runs/{run_id}", h.base))
        .header("authorization", format!("Bearer {}", h.token))
        .send()
        .await
        .expect("replay")
        .json()
        .await
        .expect("replay json");
    let events = replay["events"].as_array().expect("events");
    let cards: Vec<&Value> = events
        .iter()
        .filter(|event| event["name"] == "user-form")
        .collect();
    assert_eq!(cards.len(), 1, "{events:#?}");
    assert_eq!(cards[0]["entryId"], json!(entry_id));
    assert!(
        events
            .iter()
            .filter(|event| event["type"] == "TOOL_CALL_START")
            .all(|event| event["entryId"] == json!(entry_id)),
        "the replayed call's frames carry the card's id: {events:#?}"
    );
    fixture("replay-card-frame", cards[0]);

    let entry = h.entry(&entry_id).await;
    assert!(entry.get("callId").is_none(), "{entry}");
    let card = &entry["message"]["formRequest"];
    assert_eq!(card["formMode"], json!(true), "{entry}");
    assert_eq!(card["plugin"], json!("bir"));
    assert_eq!(
        card["fingerprint"],
        json!("sha256:draft-0"),
        "the plugin's, not the model's"
    );
    assert_eq!(card["focus"], json!(TAX14));
    assert_eq!(card["fields"][1]["line"], json!("14"));
    assert!(card["fields"][1]["error"].is_string());
    assert_eq!(card["fields"][2]["readOnly"], json!(true));
    fixture(
        "card-frame",
        frames
            .iter()
            .find(|f| f["name"] == "user-form")
            .expect("card"),
    );
    fixture("card-entry", &entry);
    let opened = h.bir.calls_to("bir_form_card");
    assert_eq!(opened, vec![json!({ "form": form_key(), "mode": "index" })]);
    assert!(
        h.bir.calls_to("bir_form_save").is_empty(),
        "the model's values were never saved"
    );
}

/// Without `forwardedProps.formMode` an old app gets no card, and the model is told so.
#[tokio::test]
async fn an_app_that_cannot_draw_the_card_gets_none() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let frames = h.turn(&h.agent.clone(), "open my 0619-E", false).await;
    let result = frames
        .iter()
        .find(|frame| frame["type"] == "TOOL_CALL_RESULT")
        .expect("the call was answered");
    assert!(
        result["content"]
            .as_str()
            .is_some_and(|said| said.contains("Form mode is not available")),
        "{result}"
    );
    assert!(!frames.iter().any(|frame| frame["name"] == "user-form"));
    assert!(h.bir.calls_to("bir_form_card").is_empty());
}

/// Save on a blur, Submit on one press, then a read-only view built from the snapshot.
#[tokio::test]
async fn a_card_saves_submits_on_one_press_and_then_only_shows() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let (entry_id, _) = h.open_card(&h.agent.clone()).await;

    // A blur saves the card's own boxes; a key it does not have and a computed box are dropped.
    let (status, saved) = h
        .act(
            &entry_id,
            "save",
            json!({ "values": { TAX14: "12,500.00", TAX16: "1", "frm0619E:other": "x" } }),
        )
        .await;
    assert_eq!(status, 200, "{saved}");
    assert_eq!(
        h.bir.calls_to("bir_form_save"),
        vec![json!({ "form": form_key(), "values": { TAX14: "12,500.00" } })]
    );
    assert_eq!(saved["formRequest"]["fingerprint"], json!("sha256:draft-1"));
    fixture("action-save", &saved);
    assert!(saved["reply"]["first_error"].is_null(), "{saved}");

    let (status, viewed) = h.act(&entry_id, "view", json!({})).await;
    assert_eq!(status, 200, "{viewed}");
    assert!(
        viewed["reply"]["html"]
            .as_str()
            .is_some_and(|html| html.starts_with("<!doctype"))
    );
    fixture("action-view", &viewed);
    let (status, focused) = h.act(&entry_id, "focus", json!({ "key": TAX14 })).await;
    assert_eq!(
        (status, focused["reply"]["ok"].clone()),
        (200, json!(true)),
        "{focused}"
    );

    // One press sends: the press is the person's yes for exactly this call, so the plugin's
    // alwaysAsk lets it through without a card.
    let (status, submitted) = h
        .act(
            &entry_id,
            "submit",
            json!({ "fingerprint": "sha256:draft-1" }),
        )
        .await;
    assert_eq!(status, 200, "{submitted}");
    assert_eq!(submitted["reply"]["queued"], json!(true), "{submitted}");
    fixture("action-submit", &submitted);
    assert_eq!(
        h.bir.calls_to("bir_form_submit"),
        vec![json!({ "form": form_key(), "fingerprint": "sha256:draft-1" })]
    );

    let entry = h.entry(&entry_id).await;
    assert_eq!(entry["formResolution"], json!("submitted"), "{entry}");
    assert_eq!(entry["formOutcome"]["queueRef"], json!("q_0619E_2026-09"));
    assert_eq!(entry["formOutcome"]["values"][TAX14], json!("12,500.00"));
    let line = opengrok_tools::user_form::history_line(&entry).expect("a line for the Bot");
    assert!(
        line.contains("q_0619E_2026-09") && line.contains("12,500.00"),
        "{line}"
    );

    // A submitted card only shows, and shows from its snapshot, never by reopening BIR.
    let cards_before = h.bir.calls_to("bir_form_card").len();
    let previews_before = h.bir.calls_to("bir_form_preview").len();
    let (status, shown) = h.act(&entry_id, "view", json!({})).await;
    assert_eq!(status, 200, "{shown}");
    assert_eq!(shown["values"][TAX14], json!("12,500.00"), "{shown}");
    fixture("action-view-submitted", &shown);
    assert_eq!(h.bir.calls_to("bir_form_card").len(), cards_before);
    assert_eq!(h.bir.calls_to("bir_form_preview").len(), previews_before);
    for action in ["save", "submit", "refresh", "focus"] {
        let (status, refused) = h
            .act(
                &entry_id,
                action,
                json!({ "values": { TAX14: "1" }, "key": TAX14 }),
            )
            .await;
        assert_eq!(status, 409, "{action}: {refused}");
    }
    assert_eq!(h.bir.calls_to("bir_form_submit").len(), 1, "sent once");

    // Every press left an audit row, as a door call does.
    let calls = h
        .client
        .get(format!("{}/coworkers/{}/mcp-calls", h.base, h.agent))
        .header("authorization", format!("Bearer {}", h.token))
        .send()
        .await
        .expect("mcp calls")
        .text()
        .await
        .expect("body");
    assert!(calls.contains("bir.bir-mcp.bir_form_submit"), "{calls}");
}

/// A model that calls the submit tool itself parks on a policy card: the plugin's alwaysAsk is
/// the lock the model cannot pass, while the card's press is the person's own yes.
#[tokio::test]
async fn a_model_that_submits_by_itself_waits_for_a_yes() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let frames = h.turn(&h.agent.clone(), "submit it yourself", true).await;
    assert!(
        frames
            .iter()
            .any(|frame| frame["name"] == "run-awaiting-approval"),
        "the model's submit asks first: {frames:#?}"
    );
    assert!(
        h.bir.calls_to("bir_form_submit").is_empty(),
        "nothing was sent"
    );
}

/// Dismiss keeps the draft and Resume reopens it: save and refresh still work on a dismissed
/// card, and a refresh is the card open again. The Bot is told where the person left off.
#[tokio::test]
async fn a_dismissed_card_keeps_its_draft_and_resumes() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let (entry_id, _) = h.open_card(&h.agent.clone()).await;

    let (status, dismissed) = h
        .post(
            "/ag-ui/user-form/dismiss",
            &json!({ "entryId": entry_id, "agentId": h.agent, "mode": "dismissed", "focus": TAX14 }),
        )
        .await;
    assert_eq!(status, 200, "{dismissed}");
    let entry = h.entry(&entry_id).await;
    assert_eq!(entry["formResolution"], json!("dismissed"), "{entry}");
    assert_eq!(entry["formOutcome"]["lastFocus"], json!(TAX14));
    fixture("dismissed-entry", &entry);
    let line = opengrok_tools::user_form::history_line(&entry).expect("a line for the Bot");
    assert!(
        line.contains("closed the form") && line.contains(TAX14),
        "{line}"
    );
    let awaiting = h
        .store
        .awaiting_approval(&h.account)
        .await
        .expect("awaiting");
    assert!(
        awaiting.is_empty(),
        "a dismiss resumes nothing: {awaiting:?}"
    );

    // Submit waits for Resume; save does not.
    let (status, _) = h
        .act(
            &entry_id,
            "submit",
            json!({ "fingerprint": "sha256:draft-0" }),
        )
        .await;
    assert_eq!(status, 409);
    let (status, saved) = h
        .act(&entry_id, "save", json!({ "values": { TAX14: "100.00" } }))
        .await;
    assert_eq!(status, 200, "{saved}");

    let (status, resumed) = h.act(&entry_id, "refresh", json!({})).await;
    assert_eq!(status, 200, "{resumed}");
    let entry = h.entry(&entry_id).await;
    assert!(entry.get("formResolution").is_none(), "{entry}");
    assert!(entry.get("widgetDismissed").is_none(), "{entry}");
    assert_eq!(
        entry["message"]["formRequest"]["fields"][1]["value"],
        json!("100.00")
    );
}
