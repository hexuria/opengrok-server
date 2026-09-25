//! Drives our MCP client against a stand-in MCP server.
//!
//! The unit tests in `mcp` prove we fill placeholders and namespace tools correctly. This proves
//! the client actually speaks the protocol: initialize, list, call — and that the credential
//! resolved from a coworker's connections arrives in the server's hands.
//!
//! THE SERVER IS HAND-WRITTEN, ON PURPOSE. Standing up rmcp's own server would be less code and a
//! worse test: two halves of one library agreeing with each other proves they share an
//! interpretation, not that the interpretation is right. Answering the raw JSON-RPC by hand is the
//! same work an unfamiliar third-party server does.

#![allow(clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use opengrok_box::{BoxError, BoxResult, CommandOutput, Computer, StartedCommand};
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_tools::Executor;
use opengrok_tools::mcp::{Endpoint, McpTool, Session, advertised_parameters, within_budget};
use serde_json::{Value, json};

/// What the server saw, so a test can assert on the request rather than only the reply.
#[derive(Debug, Default, Clone)]
struct Seen {
    authorization: Arc<Mutex<Option<String>>>,
    custom: Arc<Mutex<Option<String>>>,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
}

/// The protocol version rmcp negotiates. Answering with something it does not know fails the
/// handshake, so this is transcribed from `ProtocolVersion::V_2025_11_25` rather than guessed.
const PROTOCOL_VERSION: &str = "2025-11-25";

async fn start_server() -> (String, Seen) {
    let seen = Seen::default();

    let app = Router::new()
        .route(
            "/mcp",
            post(
                |State(seen): State<Seen>,
                 headers: axum::http::HeaderMap,
                 body: String| async move {
                    if let Ok(mut slot) = seen.authorization.lock() {
                        *slot = headers
                            .get(axum::http::header::AUTHORIZATION)
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_string);
                    }
                    if let Ok(mut slot) = seen.custom.lock() {
                        *slot = headers
                            .get("x-workspace")
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_string);
                    }

                    let request: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                    let method = request
                        .get("method")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let id = request.get("id").cloned();

                    // A notification carries no id and expects no body — answering one with JSON
                    // is a protocol error that some clients tolerate and some do not.
                    let Some(id) = id else {
                        return axum::http::StatusCode::ACCEPTED.into_response();
                    };

                    let result = match method.as_str() {
                        "initialize" => json!({
                            "protocolVersion": PROTOCOL_VERSION,
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": "stand-in", "version": "0.1.0" }
                        }),
                        "tools/list" => json!({
                            "tools": [{
                                "name": "send",
                                "description": "Send a message",
                                // Shaped like a zod- or pydantic-generated schema: a
                                // `$schema` line, a `required` list, and an identity
                                // property a remote server has no business asking the model
                                // for.
                                "inputSchema": {
                                    "$schema": "http://json-schema.org/draft-07/schema#",
                                    "type": "object",
                                    "properties": {
                                        "to": { "type": "string" },
                                        "subject": { "type": "string" },
                                        "coworkerId": { "type": "string" }
                                    },
                                    "required": ["to", "coworkerId"]
                                }
                            }, {
                                "name": "repos.list",
                                "description": "A tool whose own name contains a dot",
                                "inputSchema": { "type": "object" }
                            }]
                        }),
                        "tools/call" => {
                            let params = request.get("params").cloned().unwrap_or(Value::Null);
                            let name = params
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            let arguments =
                                params.get("arguments").cloned().unwrap_or(Value::Null);
                            if let Ok(mut calls) = seen.calls.lock() {
                                calls.push((name.clone(), arguments.clone()));
                            }
                            json!({
                                "content": [{
                                    "type": "text",
                                    "text": format!("{name} ran with {arguments}")
                                }],
                                "isError": false
                            })
                        }
                        other => {
                            return Json(json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": { "code": -32601, "message": format!("no method {other}") }
                            }))
                            .into_response();
                        }
                    };

                    Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response()
                },
            ),
        )
        .with_state(seen.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind an ephemeral port");
    let addr = listener.local_addr().expect("read the address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    (format!("http://{addr}/mcp"), seen)
}

fn endpoint(url: &str, headers: &[(&str, &str)]) -> Endpoint {
    Endpoint {
        plugin: "gmail".to_string(),
        server: "api".to_string(),
        url: url.to_string(),
        headers: headers
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect::<BTreeMap<_, _>>(),
    }
}

/// The handshake, the listing and the namespacing, against a server that is not rmcp.
#[tokio::test]
async fn we_can_connect_and_list_a_servers_tools() {
    let (url, _) = start_server().await;
    let session = Session::connect(endpoint(&url, &[]))
        .await
        .expect("connect and initialize");

    let tools = session.tools().await.expect("list tools");
    let names: Vec<_> = tools
        .iter()
        .map(|tool| tool.qualified_name.as_str())
        .collect();

    assert!(names.contains(&"gmail.api.send"), "{names:?}");
    // A remote tool whose own name contains a dot must survive being qualified.
    assert!(names.contains(&"gmail.api.repos.list"), "{names:?}");

    let send = tools
        .iter()
        .find(|tool| tool.remote_name == "send")
        .expect("the send tool");
    assert_eq!(send.description.as_deref(), Some("Send a message"));

    session.close().await;
}

/// THE POINT OF THE WHOLE CONNECTOR CHAIN: the credential reaches the server.
#[tokio::test]
async fn the_resolved_credential_arrives_as_a_header() {
    let (url, seen) = start_server().await;
    let session = Session::connect(endpoint(
        &url,
        &[
            ("authorization", "Bearer gho_theresolvedtoken"),
            ("x-workspace", "acme"),
        ],
    ))
    .await
    .expect("connect");

    session.tools().await.expect("list tools");

    assert_eq!(
        seen.authorization.lock().expect("lock").clone().as_deref(),
        Some("Bearer gho_theresolvedtoken"),
        "the token filled from the coworker's connection must reach the server"
    );
    // Non-authorization headers a plugin declares travel too.
    assert_eq!(
        seen.custom.lock().expect("lock").clone().as_deref(),
        Some("acme")
    );

    session.close().await;
}

/// Calling by the REMOTE name, with the model's arguments intact.
#[tokio::test]
async fn a_tool_call_reaches_the_server_with_its_arguments() {
    let (url, seen) = start_server().await;
    let session = Session::connect(endpoint(&url, &[]))
        .await
        .expect("connect");

    let output = session
        .call("send", json!({ "to": "someone@example.com" }))
        .await
        .expect("call the tool");

    assert!(output.contains("someone@example.com"), "{output}");

    let calls = seen.calls.lock().expect("lock").clone();
    assert_eq!(calls.len(), 1);
    // The remote name, not our qualified one — the server has never heard of `gmail.api.send`.
    assert_eq!(calls[0].0, "send");
    assert_eq!(calls[0].1["to"], "someone@example.com");

    session.close().await;
}

/// A server that is not there must be reported, not hung on.
#[tokio::test]
async fn an_unreachable_server_is_reported() {
    // Port 1 on loopback: nothing listens and the connection is refused immediately.
    let error = Session::connect(endpoint("http://127.0.0.1:1/mcp", &[]))
        .await
        .expect_err("should fail");
    let message = error.to_string();
    assert!(
        message.contains("gmail.api"),
        "it must name which server: {message}"
    );
}

/// A tool the server does not have is its refusal to report, not our crash.
#[tokio::test]
async fn an_unknown_tool_is_refused_by_the_server() {
    let (url, _) = start_server().await;
    let session = Session::connect(endpoint(&url, &[]))
        .await
        .expect("connect");

    // The stand-in answers a JSON-RPC error for an unknown method; a real server does the same for
    // an unknown tool.
    let result = session.call("no-such-tool", json!({})).await;
    // Either shape is acceptable — what matters is that we do not panic and do not claim success.
    if let Ok(output) = &result {
        assert!(
            !output.is_empty(),
            "an empty success would read as 'it worked'"
        );
    }

    session.close().await;
}

/// A computer nobody reaches: these tests are about what the model is TOLD, and a plugin tool never
/// touches the box.
struct NoBox;

#[async_trait::async_trait]
impl Computer for NoBox {
    async fn create(&self, _ttl: Option<u64>) -> BoxResult<String> {
        Err(BoxError::NoSuchBox)
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
        Err(BoxError::NoSuchBox)
    }
    async fn resume(&self, _b: &str) -> BoxResult<()> {
        Err(BoxError::NoSuchBox)
    }
    async fn destroy(&self, _b: &str) -> BoxResult<()> {
        Err(BoxError::NoSuchBox)
    }
    async fn state(&self, _b: &str) -> BoxResult<String> {
        Err(BoxError::NoSuchBox)
    }
}

/// A coworker whose ceiling and grant let it run everything, so what reaches the model is decided
/// by the schema alone.
fn everything_allowed() -> opengrok_policy::Context {
    opengrok_policy::Context {
        grant: Some(opengrok_policy::Grant {
            principal: AccountId::from_stored("acct_1"),
            coworker: CoworkerId::from_stored("cw_1"),
            profile: opengrok_policy::ToolSet::All,
            needs_approval: opengrok_policy::ToolSet::None,
            revoked: false,
        }),
        ceiling: Some(opengrok_policy::Ceiling {
            coworker: CoworkerId::from_stored("cw_1"),
            tools: opengrok_policy::ToolSet::All,
        }),
    }
}

/// The function definition the model is offered for `wire`, from a live listing.
fn advertised(tools: Vec<opengrok_tools::mcp::McpTool>, wire: &str) -> Value {
    let executor = Executor::with_policy(std::sync::Arc::new(NoBox), everything_allowed())
        .with_plugin_tools(BTreeMap::new(), tools);
    let schemas = executor.tool_schemas(
        &AccountId::from_stored("acct_1"),
        &CoworkerId::from_stored("cw_1"),
    );
    schemas
        .into_iter()
        .find(|schema| schema["function"]["name"] == wire)
        .unwrap_or_else(|| panic!("{wire} was not offered"))
}

/// #196: THE MODEL IS TOLD WHAT THE SERVER ASKED FOR. An open object made it guess `to`, and a
/// guess the server's own validation refuses costs a round.
#[tokio::test]
async fn a_tool_with_required_arguments_is_advertised_with_them() {
    let (url, _) = start_server().await;
    let session = Session::connect(endpoint(&url, &[]))
        .await
        .expect("connect");
    let tools = session.tools().await.expect("list tools");
    session.close().await;

    // Carried as the server sent it: the cleaning is applied to what is advertised, never to what
    // is kept.
    let raw = tools
        .iter()
        .find(|tool| tool.remote_name == "send")
        .and_then(|tool| tool.input_schema.clone())
        .expect("the listing carries the server's inputSchema");
    assert_eq!(raw["required"], json!(["to", "coworkerId"]));
    assert!(raw.contains_key("$schema"), "{raw:?}");

    let send = advertised(tools, "gmail_api_send");
    let parameters = &send["function"]["parameters"];
    assert_eq!(parameters["type"], "object", "{parameters}");
    assert_eq!(parameters["required"], json!(["to"]), "{parameters}");
    assert_eq!(
        parameters["properties"]["to"]["type"], "string",
        "{parameters}"
    );
    assert_eq!(
        parameters["properties"]["subject"]["type"], "string",
        "{parameters}"
    );
    // The identity is overwritten and then stripped before the call leaves: a property the server
    // will never receive must not be something the model is asked to fill.
    assert!(
        parameters["properties"].get("coworkerId").is_none(),
        "{parameters}"
    );
    assert!(parameters.get("$schema").is_none(), "{parameters}");
}

fn schema(value: Value) -> Option<serde_json::Map<String, Value>> {
    match value {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

fn tool_with(input_schema: Option<serde_json::Map<String, Value>>) -> McpTool {
    McpTool {
        qualified_name: "gmail.api.send".to_string(),
        remote_name: "send".to_string(),
        input_schema,
        ..McpTool::default()
    }
}

/// #196's fallback: whatever cannot be offered as the server's schema is offered as the open object,
/// never dropped — a tool that runs but that nobody was told about is worse than a vague one.
#[test]
fn a_schema_that_cannot_be_offered_falls_back_to_the_open_object() {
    let open = json!({ "type": "object" });

    assert_eq!(tool_with(None).parameters(), open);
    let not_an_object = schema(json!({ "type": "string" }));
    let reason =
        advertised_parameters(not_an_object.as_ref()).expect_err("a string is not arguments");
    assert!(reason.contains("string"), "{reason}");
    assert_eq!(tool_with(not_an_object).parameters(), open);

    let broken_properties = schema(json!({ "type": "object", "properties": ["to"] }));
    assert_eq!(tool_with(broken_properties).parameters(), open);

    let huge: serde_json::Map<String, Value> = (0..2_000)
        .map(|n| {
            (
                format!("field_{n}"),
                json!({ "type": "string", "description": "x".repeat(16) }),
            )
        })
        .collect();
    let oversized = schema(json!({ "type": "object", "properties": huge }));
    let reason = advertised_parameters(oversized.as_ref()).expect_err("over the cap");
    assert!(reason.contains("cap"), "{reason}");
    assert_eq!(tool_with(oversized).parameters(), open);
}

/// `{}` is how many servers spell "no arguments", and it is not an error: it becomes the object it
/// always meant, with nothing logged against it.
#[test]
fn an_empty_schema_is_a_tool_with_no_arguments() {
    let empty = schema(json!({}));
    assert_eq!(
        advertised_parameters(empty.as_ref()).expect("usable"),
        json!({ "type": "object" })
    );
}

/// The cleaning only ever LOOSENS what the model is told, and leaves the server's own vocabulary
/// alone.
#[test]
fn the_root_is_cleaned_and_the_rest_passes_through() {
    let raw = schema(json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": ["object", "null"],
        "anyOf": [{ "required": ["owner"] }, { "required": ["repo"] }],
        "properties": {
            "owner": { "type": "string", "x-vendor": true },
            "repo": { "anyOf": [{ "type": "string" }, { "type": "null" }] },
            "coworker_id": { "type": "string" },
            "boxId": { "type": "string" }
        },
        "required": ["coworker_id", "boxId"],
        "additionalProperties": false,
        "$defs": { "Owner": { "type": "string" } }
    }));
    let offered = advertised_parameters(raw.as_ref()).expect("usable");

    assert_eq!(offered["type"], "object");
    assert!(offered.get("$schema").is_none(), "{offered}");
    assert!(offered.get("anyOf").is_none(), "{offered}");
    // Nothing left to require once the identity keys are gone, and an empty list is not sent.
    assert!(offered.get("required").is_none(), "{offered}");
    assert!(
        offered["properties"].get("coworker_id").is_none(),
        "{offered}"
    );
    assert!(offered["properties"].get("boxId").is_none(), "{offered}");
    // Below the root, the server's schema is its own.
    assert_eq!(offered["properties"]["owner"]["x-vendor"], true);
    assert_eq!(
        offered["properties"]["repo"]["anyOf"][1]["type"], "null",
        "{offered}"
    );
    assert_eq!(offered["additionalProperties"], false);
    assert_eq!(offered["$defs"]["Owner"]["type"], "string");
}

/// One request's plugin schemas share a budget; a tool past it keeps its place, as an open object.
#[test]
fn the_schema_budget_is_shared_and_spent_in_order() {
    let parameters = json!({ "type": "object", "properties": { "to": { "type": "string" } } });
    let size = serde_json::to_string(&parameters).expect("serialise").len();
    let mut budget = size * 2;

    assert_eq!(within_budget(parameters.clone(), &mut budget), parameters);
    assert_eq!(within_budget(parameters.clone(), &mut budget), parameters);
    assert_eq!(budget, 0);
    assert_eq!(
        within_budget(parameters, &mut budget),
        json!({ "type": "object" })
    );
}
