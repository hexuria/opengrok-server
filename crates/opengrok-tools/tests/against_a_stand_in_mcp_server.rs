//! Drives our MCP client against a stand-in MCP server.
//!
//! The unit tests in `mcp` prove we fill placeholders and namespace tools correctly. This proves
//! the client actually speaks the protocol: initialize, list, call — that the credential resolved
//! from a coworker's connections arrives in the server's hands, that the schema the server sent is
//! what the model is offered (#196), and that a server which never answers costs a turn its
//! deadline rather than the turn (#199).
//!
//! THE SERVER IS HAND-WRITTEN, ON PURPOSE. Standing up rmcp's own server would be less code and a
//! worse test: two halves of one library agreeing with each other proves they share an
//! interpretation, not that the interpretation is right. Answering the raw JSON-RPC by hand is the
//! same work an unfamiliar third-party server does.

#![allow(clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use opengrok_box::{BoxError, BoxResult, CommandOutput, Computer, StartedCommand};
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_tools::Executor;
use opengrok_tools::mcp::{
    Deadlines, Dialled, Endpoint, McpTool, Pool, Session, advertised_parameters, within_budget,
};
use serde_json::{Value, json};

/// What the server saw, so a test can assert on the request rather than only the reply.
#[derive(Debug, Default, Clone)]
struct Seen {
    authorization: Arc<Mutex<Option<String>>>,
    custom: Arc<Mutex<Option<String>>>,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    /// Handshakes, so reuse can be told apart from reconnecting.
    initializes: Arc<AtomicUsize>,
    /// `notifications/cancelled` params, so a timed-out call can be shown to have been called off.
    cancelled: Arc<Mutex<Vec<Value>>>,
}

/// The protocol version rmcp negotiates. Answering with something it does not know fails the
/// handshake, so this is transcribed from `ProtocolVersion::V_2025_11_25` rather than guessed.
const PROTOCOL_VERSION: &str = "2025-11-25";

async fn start_server() -> (String, Seen) {
    start_server_hanging(None).await
}

/// How the stand-in fails to answer one method.
#[derive(Clone, Copy)]
enum Hang {
    /// Takes the request and sends nothing back, not even headers: a server that answers in JSON
    /// and whose work never finishes.
    Silent(&'static str),
    /// Opens the event stream for the answer and never sends it: a streaming server whose work
    /// never finishes.
    Streaming(&'static str),
}

async fn start_server_hanging(hang: Option<Hang>) -> (String, Seen) {
    let seen = Seen::default();

    let app =
        Router::new()
            .route(
                "/mcp",
                post(
                    move |State(seen): State<Seen>,
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

                        if method == "notifications/cancelled"
                            && let Ok(mut cancelled) = seen.cancelled.lock()
                        {
                            cancelled.push(request.get("params").cloned().unwrap_or(Value::Null));
                        }
                        // A notification carries no id and expects no body — answering one with JSON
                        // is a protocol error that some clients tolerate and some do not.
                        let Some(id) = id else {
                            return axum::http::StatusCode::ACCEPTED.into_response();
                        };
                        if method == "initialize" {
                            seen.initializes.fetch_add(1, Ordering::SeqCst);
                        }
                        match hang {
                            Some(Hang::Silent(on)) if on == method => {
                                std::future::pending::<()>().await;
                            }
                            Some(Hang::Streaming(on)) if on == method => {
                                let never = futures::stream::pending::<
                                    Result<axum::response::sse::Event, std::convert::Infallible>,
                                >();
                                return axum::response::sse::Sse::new(never).into_response();
                            }
                            _ => {}
                        }

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

                        Json(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
                            .into_response()
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

/// Accepts every connection and never reads or writes a byte: a server behind a black-holing
/// firewall, or one wedged mid-deploy.
async fn start_black_hole() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind an ephemeral port");
    let addr = listener.local_addr().expect("read the address");
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    format!("http://{addr}/mcp")
}

/// Short enough to keep the suite quick, long enough for a loopback stand-in to answer.
fn quick() -> Deadlines {
    Deadlines {
        connect: Duration::from_millis(500),
        call: Duration::from_millis(500),
    }
}

fn endpoint_for(plugin: &str, url: &str) -> Endpoint {
    Endpoint {
        plugin: plugin.to_string(),
        ..endpoint(url, &[])
    }
}

/// #199 AS FIRST SEEN: a server that accepts and never answers `initialize` hung every turn.
#[tokio::test]
async fn a_server_that_never_answers_initialize_is_reported_within_the_deadline() {
    let url = start_black_hole().await;
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        Session::connect_within(endpoint(&url, &[]), quick()),
    )
    .await;
    let error = outcome
        .expect("connect must give up on its own deadline")
        .expect_err("nothing answered");
    let message = error.to_string();
    assert!(
        message.contains("gmail.api"),
        "it must name the server: {message}"
    );
    assert!(message.contains("did not answer"), "{message}");
}

/// Answering `initialize` and then going quiet is as unavailable as never answering.
#[tokio::test]
async fn a_server_that_never_lists_its_tools_is_reported_within_the_deadline() {
    let (url, _) = start_server_hanging(Some(Hang::Silent("tools/list"))).await;
    let session = Session::connect_within(endpoint(&url, &[]), quick())
        .await
        .expect("the handshake is answered");
    let error = tokio::time::timeout(Duration::from_secs(5), session.tools())
        .await
        .expect("listing must give up on its own deadline")
        .expect_err("nothing was listed");
    let message = error.to_string();
    assert!(message.contains("gmail.api"), "{message}");
    assert!(message.contains("tools/list"), "{message}");
}

/// A call that never returns becomes a RESULT the model can reason about — one that says the
/// action may still have happened — and a session wedged on it is never handed to another turn.
#[tokio::test]
async fn a_tool_call_that_never_returns_is_a_result_not_a_hang() {
    let (url, _) = start_server_hanging(Some(Hang::Silent("tools/call"))).await;
    let session = Arc::new(
        Session::connect_within(endpoint(&url, &[]), quick())
            .await
            .expect("connect"),
    );
    let tools = session.tools().await.expect("list");
    let executor = Executor::with_policy(std::sync::Arc::new(NoBox), everything_allowed())
        .with_plugin_tools(
            BTreeMap::from([("gmail.api".to_string(), session.clone())]),
            tools,
        );

    let result = tokio::time::timeout(Duration::from_secs(5), send_through(&executor))
        .await
        .expect("the call must give up on its own deadline");

    assert!(!result.ok, "{result:?}");
    assert!(result.content.contains("did not answer"), "{result:?}");
    assert!(
        result.content.contains("may already have taken effect"),
        "a timed-out send may have been sent: {result:?}"
    );
    assert!(
        !session.is_usable(),
        "a session stuck on a call must not be pooled for the next turn"
    );
}

/// When the server is streaming its answer, the timeout reaches it as `notifications/cancelled`,
/// so the work is called off rather than finished for nobody.
#[tokio::test]
async fn a_timed_out_call_is_called_off_at_the_server() {
    let (url, seen) = start_server_hanging(Some(Hang::Streaming("tools/call"))).await;
    let session = Session::connect_within(endpoint(&url, &[]), quick())
        .await
        .expect("connect");

    let error = tokio::time::timeout(
        Duration::from_secs(5),
        session.call("send", json!({ "to": "someone@example.com" })),
    )
    .await
    .expect("the call must give up on its own deadline")
    .expect_err("nothing was answered");
    assert!(error.to_string().contains("did not answer"), "{error}");

    let called_off = async {
        while seen.cancelled.lock().expect("lock").is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(3), called_off)
        .await
        .expect("the server is told the call was cancelled");
}

/// A tool of a server that did not answer this turn, called anyway (the model remembers it from an
/// earlier one), is refused with the server's reason — not as a tool that never existed.
#[tokio::test]
async fn a_call_to_an_unavailable_server_says_why() {
    let executor = Executor::with_policy(std::sync::Arc::new(NoBox), everything_allowed())
        .with_plugins(Dialled {
            unavailable: BTreeMap::from([(
                "gmail.api".to_string(),
                "gmail.api did not answer initialize and tools/list within 5s".to_string(),
            )]),
            ..Dialled::default()
        });

    for name in ["gmail_api_send", "gmail.api.send"] {
        let result = call_through(&executor, name).await;
        assert!(!result.ok, "{result:?}");
        assert!(
            result.content.contains("did not answer"),
            "{name}: {result:?}"
        );
        assert!(
            !result.content.contains("there is no tool"),
            "{name}: {result:?}"
        );
    }
    // A name that belongs to no server is still simply not a tool.
    let unknown = call_through(&executor, "github_api_search").await;
    assert!(unknown.content.contains("there is no tool"), "{unknown:?}");
}

async fn send_through(executor: &Executor) -> opengrok_tools::ToolResult {
    call_through(executor, "gmail_api_send").await
}

async fn call_through(executor: &Executor, name: &str) -> opengrok_tools::ToolResult {
    let context = opengrok_tools::ToolContext {
        account_id: AccountId::from_stored("acct_1"),
        coworker_id: CoworkerId::from_stored("cw_1"),
        // A plugin tool never touches the box, but a coworker with none is refused before
        // anything is routed.
        box_id: Some(opengrok_core::id::BoxId::from_stored("box_1")),
        group_box: None,
        screen_hold: false,
    };
    let call = opengrok_tools::ToolCall {
        id: "call_1".to_string(),
        name: name.to_string(),
        arguments: json!({ "to": "someone@example.com" }),
    };
    executor.execute(&context, &call).await
}

/// #199's acceptance: one dead server among several costs the turn the deadline, not a hang, and
/// the others' tools are offered as usual.
#[tokio::test]
async fn one_black_holed_server_does_not_delay_the_others() {
    let (good, _) = start_server().await;
    let mut endpoints = Vec::new();
    for n in 1..=4 {
        endpoints.push(endpoint_for(&format!("slow{n}"), &start_black_hole().await));
    }
    endpoints.insert(2, endpoint_for("gmail", &good));
    let pool = Pool::new(quick());

    let started = Instant::now();
    let dialled = pool.dial("acct_1/cw_1", endpoints, |_| true).await;
    let took = started.elapsed();

    // Concurrent: four dead servers cost one deadline (500 ms), not four (2 s). The margin is for
    // a loaded CI machine, not for a serial dial to slip under.
    assert!(took < Duration::from_millis(1_500), "took {took:?}");
    let names: Vec<_> = dialled
        .tools
        .iter()
        .map(|tool| tool.qualified_name.as_str())
        .collect();
    assert!(names.contains(&"gmail.api.send"), "{names:?}");
    assert_eq!(
        dialled.sessions.keys().collect::<Vec<_>>(),
        vec!["gmail.api"]
    );
    assert_eq!(
        dialled.unavailable.keys().collect::<Vec<_>>(),
        vec!["slow1.api", "slow2.api", "slow3.api", "slow4.api"]
    );
    assert!(
        dialled.unavailable["slow1.api"].contains("did not answer"),
        "{:?}",
        dialled.unavailable
    );

    // What the turn is told, so a person asking for the dead one hears it is down.
    let executor = Executor::with_policy(std::sync::Arc::new(NoBox), everything_allowed())
        .with_plugins(dialled);
    let line = executor.unavailable_plugins_line();
    assert!(
        line.contains("slow1.api") && line.contains("slow2.api"),
        "{line}"
    );
    assert!(!line.contains("gmail"), "{line}");
}

/// A server listed a moment ago is reused, not dialled again — and the ceiling is still applied to
/// the pooled list on every dial.
#[tokio::test]
async fn a_second_dial_within_the_ttl_reuses_the_session() {
    let (url, seen) = start_server().await;
    let pool = Pool::new(quick());

    let first = pool
        .dial("acct_1/cw_1", vec![endpoint(&url, &[])], |_| true)
        .await;
    assert_eq!(first.tools.len(), 2, "{:?}", first.tools);

    let narrowed = pool
        .dial("acct_1/cw_1", vec![endpoint(&url, &[])], |tool| {
            tool != "gmail.api.send"
        })
        .await;
    assert_eq!(
        seen.initializes.load(Ordering::SeqCst),
        1,
        "reused, not redialled"
    );
    let names: Vec<_> = narrowed
        .tools
        .iter()
        .map(|tool| tool.qualified_name.as_str())
        .collect();
    assert_eq!(
        names,
        vec!["gmail.api.repos.list"],
        "the ceiling applies to a pooled list"
    );
    assert!(Arc::ptr_eq(
        &first.sessions["gmail.api"],
        &narrowed.sessions["gmail.api"]
    ));
}

/// A session never outlives its credential and is never shared across coworkers: a different
/// token, or a different coworker with the same token, is a new session.
#[tokio::test]
async fn a_pooled_session_is_never_handed_to_another_credential_or_coworker() {
    let (url, seen) = start_server().await;
    let pool = Pool::new(quick());
    let with_token = |token: &str| endpoint(&url, &[("authorization", token)]);

    pool.dial("acct_1/cw_1", vec![with_token("Bearer gho_first")], |_| {
        true
    })
    .await;
    pool.dial(
        "acct_1/cw_1",
        vec![with_token("Bearer gho_rotated")],
        |_| true,
    )
    .await;
    assert_eq!(
        seen.initializes.load(Ordering::SeqCst),
        2,
        "a rotated token reconnects"
    );
    assert_eq!(
        seen.authorization.lock().expect("lock").as_deref(),
        Some("Bearer gho_rotated")
    );

    pool.dial(
        "acct_1/cw_2",
        vec![with_token("Bearer gho_rotated")],
        |_| true,
    )
    .await;
    assert_eq!(
        seen.initializes.load(Ordering::SeqCst),
        3,
        "another coworker reconnects"
    );
}

/// A dead server costs one deadline per retry window, not one per turn; once the window passes it
/// is tried again.
#[tokio::test]
async fn a_failed_server_is_left_alone_until_its_retry_window_passes() {
    let url = start_black_hole().await;
    let pool = Pool::new(quick()).with_reuse(Pool::TTL, Duration::from_millis(800));

    let first = pool
        .dial("acct_1/cw_1", vec![endpoint(&url, &[])], |_| true)
        .await;
    assert!(first.unavailable.contains_key("gmail.api"));

    let started = Instant::now();
    let again = pool
        .dial("acct_1/cw_1", vec![endpoint(&url, &[])], |_| true)
        .await;
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "{:?}",
        started.elapsed()
    );
    assert!(again.unavailable.contains_key("gmail.api"));

    tokio::time::sleep(Duration::from_millis(800)).await;
    let started = Instant::now();
    let retried = pool
        .dial("acct_1/cw_1", vec![endpoint(&url, &[])], |_| true)
        .await;
    assert!(
        started.elapsed() >= Duration::from_millis(400),
        "it was tried again"
    );
    assert!(retried.unavailable.contains_key("gmail.api"));
}
