//! A plugin server that accepts and never answers must not hold the turn (#199).
//!
//! THE BUG: every turn, resume, autonomy run, MCP-door call and tools listing awaits the plugin
//! listing before anything else, and that listing dialled every installed plugin server one after
//! another with nothing bounding it. One server behind a black-holing firewall hung every chat on
//! the deployment, with no error anywhere.
//!
//! Driven through `GET /coworkers/{id}/tools`, which builds exactly the toolbox a turn does and
//! answers with what the model would be offered. The dead server costs the listing the connect
//! deadline once (5 s by default — this suite cannot shorten it, the process-wide pool reads it
//! from the environment), the live one's tools are still offered, and the next listing is served
//! from the pool without waiting again.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::response::IntoResponse;
use opengrok_box::{BoxError, BoxResult, CommandOutput, Computer, StartedCommand};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_harness::MockDoor;
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

/// A box that is always up and never asked to do anything: the listing only looks at it.
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

/// The smallest MCP server that lists one tool: hand-written JSON-RPC, like any third party's.
async fn start_live_server() -> String {
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
                    "serverInfo": { "name": "live", "version": "0.1.0" }
                }),
                "tools/list" => json!({ "tools": [{
                    "name": "send",
                    "description": "Send a message",
                    "inputSchema": {
                        "type": "object",
                        "properties": { "to": { "type": "string" } },
                        "required": ["to"]
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

/// Accepts every connection and never reads or writes a byte.
async fn start_black_hole() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    format!("http://{addr}/mcp")
}

fn plugin(name: &str, url: &str) -> Plugin {
    Plugin {
        root: std::env::temp_dir(),
        manifest: serde_json::from_value(json!({ "name": name })).expect("manifest"),
        mcp: McpConfig {
            schema: None,
            servers: BTreeMap::from([(
                "api".to_string(),
                McpServer::StreamableHttp {
                    url: url.to_string(),
                    headers: BTreeMap::new(),
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
            first_name: "Host".to_string(),
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
        first_name: "Host".to_string(),
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

async fn listed(client: &reqwest::Client, base: &str, token: &str, agent: &str) -> Vec<String> {
    let body: Value = client
        .get(format!("{base}/coworkers/{agent}/tools"))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("list tools")
        .json()
        .await
        .expect("tools json");
    body["tools"]
        .as_array()
        .expect("a tools array")
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test]
async fn a_dead_plugin_server_costs_the_deadline_once_and_the_live_ones_tools_are_offered() {
    let database_url = database_or_skip!();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let email = format!("dead-plugin-{}@og.local", uuid::Uuid::now_v7().simple());
    let account = seed_account(&store, &email).await;

    let plugins = BTreeMap::from([
        (
            "dead".to_string(),
            plugin("dead", &start_black_hole().await),
        ),
        (
            "live".to_string(),
            plugin("live", &start_live_server().await),
        ),
    ]);
    let auth = AuthState::new(
        store.clone(),
        Arc::new(TokenMinter::new(b"dead-plugin-secret-dead-plugin!!")),
        email.clone(),
    );
    let agui = AgUiState {
        auth,
        door: Arc::new(MockDoor::echoing()),
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
    // Both plugins' tools inside the ceiling, so both servers are worth dialling.
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

    let started = Instant::now();
    let first = tokio::time::timeout(
        Duration::from_secs(20),
        listed(&client, &base, &token, &agent),
    )
    .await
    .expect("the listing must not wait on a server that never answers");
    let took = started.elapsed();
    assert!(took < Duration::from_secs(9), "took {took:?}");
    assert!(first.contains(&"live_api_send".to_string()), "{first:?}");
    assert!(first.contains(&"shell".to_string()), "{first:?}");
    assert!(
        !first.iter().any(|name| name.starts_with("dead_")),
        "{first:?}"
    );

    // The dead server is not waited on again inside its retry window, and the live one is reused.
    let started = Instant::now();
    let second = listed(&client, &base, &token, &agent).await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "took {:?}",
        started.elapsed()
    );
    assert_eq!(first, second);
}
