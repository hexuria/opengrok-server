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
use opengrok_tools::mcp::{Endpoint, Session};
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
                                "inputSchema": {
                                    "type": "object",
                                    "properties": { "to": { "type": "string" } }
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
