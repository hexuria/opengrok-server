//! OpenGrok — the server the coworkers live on.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
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

    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&database_url)
        .await
        .context("could not connect to Postgres")?;

    opengrok_store::migrations::run(&pool)
        .await
        .map_err(|error| anyhow::anyhow!("migrations failed: {error}"))?;

    let state = AuthState {
        store: PgStore::new(pool),
        minter: Arc::new(TokenMinter::new(token_secret.as_bytes())),
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

async fn shutdown_signal() {
    // A failure to install the handler must not take the process down; it only means Ctrl-C will
    // not be graceful, which is strictly better than refusing to run.
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::warn!(%error, "could not listen for shutdown signal");
    }
}
