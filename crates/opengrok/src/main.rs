//! OpenGrok — the server the coworkers live on.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use opengrok_harness::{GatewayDoor, MockDoor, ModelDoor};
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_store::PgStore;
use sqlx::postgres::PgPoolOptions;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "opengrok=debug,opengrok_server=debug".into()),
        )
        .init();

    let bind: SocketAddr = std::env::var("OG_BIND")
        .unwrap_or_else(|_| "0.0.0.0:1337".to_string())
        .parse()
        .context("OG_BIND is not a socket address")?;

    let database_url = std::env::var("OG_DATABASE_URL")
        .context("OG_DATABASE_URL is required; see .env.example")?;

    // The secret that signs access tokens. Required rather than defaulted: a default would mean
    // every deployment that forgot to set one shares a signing key, and tokens minted by anybody's
    // laptop would verify here.
    let token_secret = std::env::var("OG_TOKEN_SECRET")
        .context("OG_TOKEN_SECRET is required; generate one with `openssl rand -hex 32`")?;

    // A bounded wait, because the unbounded one is worse than a failure: with Postgres unreachable
    // the process sits alive, silent, and never binds — no log line, no port, nothing to debug.
    // Ten seconds is longer than a healthy connect and shorter than a person's patience.
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(&database_url)
        .await
        .with_context(|| format!("could not connect to Postgres at {}", redact(&database_url)))?;

    opengrok_store::migrations::run(&pool)
        .await
        .map_err(|error| anyhow::anyhow!("migrations failed: {error}"))?;

    let auth = AuthState {
        store: PgStore::new(pool),
        minter: Arc::new(TokenMinter::new(token_secret.as_bytes())),
    };

    // OG_MODEL_DOOR=mock runs the whole stack with no provider, no key and no spend. It is also
    // what CI uses, so the streaming path is exercised on every push rather than only by hand.
    let door: Arc<dyn ModelDoor> = match std::env::var("OG_MODEL_DOOR").as_deref() {
        Ok("mock") => {
            tracing::warn!("OG_MODEL_DOOR=mock — no model will be called");
            Arc::new(MockDoor::echoing())
        }
        _ => {
            let url = std::env::var("OG_GATEWAY_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:29080".to_string());
            // An oag_live_ key, never a provider key: a coworker's pin is a route (CLAUDE.md #4).
            let key = std::env::var("OG_GATEWAY_TOKEN").context(
                "OG_GATEWAY_TOKEN is required unless OG_MODEL_DOOR=mock; see .env.example",
            )?;
            Arc::new(GatewayDoor::new(url, key))
        }
    };

    // The computer provider. Without a box key there are no computers, and therefore no tools —
    // the honest state, since a tool the model is told about but that cannot run is a dead end it
    // keeps trying.
    let computer: Option<Arc<dyn opengrok_box::Computer>> = match std::env::var("OG_BOX_API_KEY") {
        Ok(key) if !key.is_empty() => Some(Arc::new(opengrok_box::AsciiBoxes::new(key))),
        _ => {
            tracing::warn!(
                "OG_BOX_API_KEY is unset — coworkers will have no computer and no tools"
            );
            None
        }
    };

    let state = AgUiState {
        auth,
        door,
        model: std::env::var("OG_MODEL").unwrap_or_else(|_| "oag/cheap".to_string()),
        computer,
    };

    let app = opengrok_server::router(state);
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("could not bind {bind}"))?;

    tracing::info!(%bind, "opengrok listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server stopped unexpectedly")?;
    Ok(())
}

/// A connection string without its password, for an error a person may paste into a chat.
fn redact(database_url: &str) -> String {
    match (database_url.find("://"), database_url.find('@')) {
        (Some(scheme), Some(at)) if at > scheme => {
            format!(
                "{}://<redacted>{}",
                &database_url[..scheme],
                &database_url[at..]
            )
        }
        _ => database_url.to_string(),
    }
}

/// Wait for either signal that means "stop".
///
/// SIGTERM MATTERS MORE THAN CTRL-C HERE. It is how Docker, systemd and Kubernetes ask a process
/// to stop; a server that only listens for SIGINT keeps serving until the grace period runs out and
/// it is SIGKILLed, which drops whatever was in flight. Handling both is the difference between a
/// deploy that finishes its runs and one that truncates them.
///
/// A failure to install a handler must not take the process down: it means shutdown will be abrupt,
/// which is strictly better than refusing to run.
async fn shutdown_signal() {
    let interrupt = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::warn!(%error, "could not listen for Ctrl-C");
            // Never resolve, so this arm cannot win the select and end the process early.
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                tracing::warn!(%error, "could not listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = interrupt => tracing::info!("interrupted; shutting down"),
        () = terminate => tracing::info!("terminated; shutting down"),
    }
}
