//! Drives the tonic listener with a real gRPC client.
//!
//! The Connect routes are proven by `slice13-seamb-smoke.sh` with curl; HTTP/2 gRPC cannot be
//! curl'd, so this is the one place the tonic half of the transcribed contract is exercised —
//! client and server both generated from OUR `proto/opengrok_seamb.proto`, over a real socket.
//!
//! Needs Postgres (the state carries the store), so it skips — loudly — when OG_DATABASE_URL is
//! absent. The gate exports it; `cargo test` on a bare laptop stays green.

#![allow(clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::Arc;

use opengrok_proto::aiserver::v1 as pb;
use opengrok_server::auth::TokenMinter;
use opengrok_server::gateway::GatewayState;

macro_rules! database_or_skip {
    () => {
        match std::env::var("OG_DATABASE_URL") {
            Ok(url) => url,
            Err(_) => {
                eprintln!("skipping: OG_DATABASE_URL is not set");
                return;
            }
        }
    };
}

async fn state_with(secret: &[u8], database_url: &str) -> GatewayState {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = opengrok_store::PgStore::new(pool);
    let auth = opengrok_server::auth::AuthState {
        store,
        minter: Arc::new(TokenMinter::new(secret)),
    };
    let agui = opengrok_server::agui::AgUiState {
        auth,
        door: Arc::new(opengrok_harness::MockDoor::echoing()),
        model: "oag/cheap".to_string(),
        computer: None,
        vault: None,
        connectors: opengrok_server::connections::routes::Connectors {
            providers: Arc::new(BTreeMap::new()),
            redirect_uri: "http://127.0.0.1/callback".to_string(),
        },
        plugins: Arc::new(BTreeMap::new()),
    };
    GatewayState::new(
        agui,
        Some("grpc-test-bearer".to_string()),
        "grpc@og.local".to_string(),
        Some("http://opengrok.lan:1447".to_string()),
    )
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    port
}

fn bearer(
    minter: &TokenMinter,
    account: &str,
) -> tonic::metadata::MetadataValue<tonic::metadata::Ascii> {
    let now = chrono::Utc::now().timestamp();
    let token = minter
        .mint_access(account, "sess_grpc", "grpc@og.local", "pro", now, 3600)
        .expect("mint");
    format!("Bearer {token}").parse().expect("metadata")
}

#[tokio::test]
async fn the_transcribed_contract_round_trips_over_real_grpc() {
    let database_url = database_or_skip!();
    let secret = b"grpc-test-secret-grpc-test-secret";
    let state = state_with(secret, &database_url).await;
    let minter = TokenMinter::new(secret);

    let port = free_port();
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().expect("addr");
    tokio::spawn(opengrok_server::grpc::serve(state, addr));

    let endpoint = format!("http://127.0.0.1:{port}");
    let mut dashboard = None;
    for _ in 0..20 {
        match pb::dashboard_service_client::DashboardServiceClient::connect(endpoint.clone()).await
        {
            Ok(client) => {
                dashboard = Some(client);
                break;
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(200)).await,
        }
    }
    let mut dashboard = dashboard.expect("the tonic listener never came up");

    // Unauthenticated is a gRPC status, not a mystery.
    let refused = dashboard.get_me(pb::GetMeRequest {}).await;
    assert_eq!(
        refused.expect_err("no token must refuse").code(),
        tonic::Code::Unauthenticated
    );

    // With the same signed access token the HTTP edge takes.
    let token = bearer(&minter, "acct_grpc_test");
    let mut request = tonic::Request::new(pb::GetMeRequest {});
    request
        .metadata_mut()
        .insert("authorization", token.clone());
    let me = dashboard.get_me(request).await.expect("GetMe").into_inner();
    assert_eq!(me.auth_id, "acct_grpc_test");

    let mut request = tonic::Request::new(pb::GetUserPrivacyModeRequest {});
    request
        .metadata_mut()
        .insert("authorization", token.clone());
    let privacy = dashboard
        .get_user_privacy_mode(request)
        .await
        .expect("privacy")
        .into_inner();
    assert_eq!(privacy.privacy_mode(), pb::PrivacyMode::NoTraining);

    // The mint, over gRPC: the same address and bearer the Connect edge hands out.
    let mut grok_bot = pb::grok_bot_service_client::GrokBotServiceClient::connect(endpoint.clone())
        .await
        .expect("connect grok bot");
    let mut request = tonic::Request::new(pb::EnsureSandBoxRequest { window_index: 0 });
    request
        .metadata_mut()
        .insert("authorization", token.clone());
    let mint = grok_bot
        .ensure_sand_box(request)
        .await
        .expect("EnsureSandBox")
        .into_inner();
    assert_eq!(mint.gateway_url, "http://opengrok.lan:1447");
    assert_eq!(mint.gateway_token, "grpc-test-bearer");

    let mut request = tonic::Request::new(pb::ListGrokBotAgentsRequest { role: None });
    request.metadata_mut().insert("authorization", token);
    let agents = grok_bot
        .list_grok_bot_agents(request)
        .await
        .expect("ListGrokBotAgents")
        .into_inner();
    // A fresh account has an empty roster — the shape holds, the list is honest.
    assert!(agents.agents.is_empty());
}
