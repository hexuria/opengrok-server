//! OpenGrok — the server the coworkers live on.

mod admin;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use opengrok_harness::{GatewayDoor, MockDoor, ModelDoor, RigDoor};
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_store::PgStore;
use sqlx::postgres::PgPoolOptions;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // `opengrok admin …` runs a CLI command and exits before any listener starts.
    if let Some(code) = admin::maybe_run().await {
        std::process::exit(code);
    }

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

    // Who a browser login signs in as — the single user of a self-hosted OpenGrok. Defaults to
    // the gateway's own account so the desktop's roster and its sign-in are the same person.
    let login_email = std::env::var("OG_LOGIN_EMAIL")
        .ok()
        .or_else(|| std::env::var("OG_GATEWAY_EMAIL").ok())
        .unwrap_or_else(|| "host@opengrok.local".to_string());
    let public_url = std::env::var("OG_PUBLIC_GATEWAY_URL")
        .ok()
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| format!("http://{bind}"));
    let auth = AuthState::new(
        PgStore::new(pool),
        Arc::new(TokenMinter::new(token_secret.as_bytes())),
        login_email,
    )
    .with_resend(
        std::env::var("OG_RESEND_API_KEY")
            .ok()
            .or_else(|| std::env::var("RESEND_API").ok()),
        public_url,
    );

    // OG_MODEL_DOOR=mock runs the whole stack with no provider, no key and no spend. It is also
    // what CI uses, so the streaming path is exercised on every push rather than only by hand.
    let door: Arc<dyn ModelDoor> = match std::env::var("OG_MODEL_DOOR").as_deref() {
        Ok("mock") => {
            tracing::warn!("OG_MODEL_DOOR=mock — no model will be called");
            Arc::new(MockDoor::echoing())
        }
        // The tool path, without a model: the echoing door never reaches for a tool, so a suite
        // built only on it exercises talking and never doing.
        Ok("mock-tools") => {
            tracing::warn!("OG_MODEL_DOOR=mock-tools — no model, and every turn asks for a tool");
            Arc::new(MockDoor::asking_for_a_tool())
        }
        door => {
            let url = std::env::var("OG_GATEWAY_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:29080".to_string());
            // An oag_live_ key, never a provider key: a coworker's pin is a route (CLAUDE.md #4).
            // The same key opens either door — Rig is pointed at our gateway, not at a provider.
            let key = std::env::var("OG_GATEWAY_TOKEN").context(
                "OG_GATEWAY_TOKEN is required unless OG_MODEL_DOOR=mock; see .env.example",
            )?;
            if door == Ok("rig") {
                tracing::info!("OG_MODEL_DOOR=rig — reaching the gateway through rig-core");
                Arc::new(RigDoor::new(url, key))
            } else {
                Arc::new(GatewayDoor::new(url, key))
            }
        }
    };

    // Where a coworker's computer comes from. box.ascii.dev when a key is present, otherwise local
    // Docker — so a coworker gets a computer on a laptop with no account anywhere, and the hosted
    // one is an upgrade rather than a prerequisite. `OG_COMPUTER=none` turns computers off.
    let computer: Option<Arc<dyn opengrok_box::Computer>> = match std::env::var("OG_COMPUTER")
        .as_deref()
    {
        Ok("none") => {
            tracing::warn!("OG_COMPUTER=none — coworkers will have no computer and no tools");
            None
        }
        Ok("docker") => Some(Arc::new(opengrok_box::DockerComputer::new())),
        Ok("ascii") | Ok("box") => Some(Arc::new(opengrok_box::AsciiBoxes::new(
            std::env::var("OG_BOX_API_KEY").context("OG_COMPUTER=ascii needs OG_BOX_API_KEY")?,
        ))),
        _ => match std::env::var("OG_BOX_API_KEY") {
            Ok(key) if !key.is_empty() => Some(Arc::new(opengrok_box::AsciiBoxes::new(key))),
            _ => {
                tracing::info!("no OG_BOX_API_KEY — using local Docker for coworkers");
                Some(Arc::new(opengrok_box::DockerComputer::new()))
            }
        },
    };

    // Seals connector credentials. Absent is a legitimate deployment — one with no connectors —
    // and must read as "connectors unavailable" rather than as a crash at boot.
    let vault = match std::env::var("OG_CREDENTIAL_KEK") {
        Ok(kek) if !kek.is_empty() => Some(Arc::new(
            opengrok_store::Vault::from_base64_key(&kek)
                .map_err(|error| anyhow::anyhow!("{error}"))?,
        )),
        _ => {
            tracing::info!("no OG_CREDENTIAL_KEK — connectors are unavailable on this server");
            None
        }
    };

    let connectors = load_connectors()?;
    let plugins = load_plugins();

    let state = AgUiState {
        auth,
        door,
        model: std::env::var("OG_MODEL").unwrap_or_else(|_| "oag/cheap".to_string()),
        computer,
        vault,
        connectors,
        plugins: Arc::new(plugins),
    };

    // Pick up whatever the last process abandoned. Started before the listener, because the most
    // likely moment to find an abandoned run is immediately after the restart that abandoned it.
    tokio::spawn(opengrok_server::recovery::sweep_forever(state.clone()));

    // The autonomy loops: due schedules fire runs, and monitors react to the event log. These are
    // the half of the mission that does not wait for a request.
    tokio::spawn(opengrok_server::autonomy::sweep::schedules_forever(
        state.clone(),
    ));
    tokio::spawn(opengrok_server::autonomy::sweep::monitors_forever(
        state.clone(),
    ));

    // Seam A: the desktop client's gateway. The bearer is optional — absent means loopback-only,
    // the shipped host's own fallback — and the email names whose coworkers are the roster.
    //
    // OG_GATEWAY_BEARER, deliberately not OG_GATEWAY_TOKEN: that name already means the key WE
    // present to the model gateway. One name meaning "what we show upstream" and "what clients
    // must show us" is how a model key ends up handed to every desktop client.
    let gateway = opengrok_server::gateway::GatewayState::new(
        state.clone(),
        std::env::var("OG_GATEWAY_BEARER")
            .ok()
            .filter(|token| !token.is_empty()),
        std::env::var("OG_GATEWAY_EMAIL").unwrap_or_else(|_| "host@opengrok.local".to_string()),
        std::env::var("OG_PUBLIC_GATEWAY_URL")
            .ok()
            .filter(|url| !url.is_empty()),
    );

    // The tonic listener — internal gRPC on the transcribed seam-B contract. Opt-in: absent
    // means no listener, because nothing internal dials it yet and an unused open port is a
    // liability, not a feature.
    if let Ok(bind) = std::env::var("OG_GRPC_BIND")
        && let Ok(addr) = bind.parse()
    {
        tokio::spawn(opengrok_server::grpc::serve(gateway.clone(), addr));
    }

    let app = opengrok_server::router(state, gateway);
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

/// Provider configuration, read from the JSON file named by `OG_CONNECTORS`.
///
/// A FILE RATHER THAN ENVIRONMENT VARIABLES, because a provider needs five fields and several
/// providers need five each; `OG_GMAIL_CLIENT_ID`-style names multiply until nobody can list them.
/// The file holds client secrets, so its permissions are the guard — the same bargain the gateway's
/// `config.yaml` makes.
fn load_connectors() -> anyhow::Result<opengrok_server::connections::routes::Connectors> {
    use opengrok_server::connections::oauth::ProviderConfig;

    let redirect_uri = std::env::var("OG_OAUTH_REDIRECT_URI")
        .unwrap_or_else(|_| "http://127.0.0.1:1337/connections/callback".to_string());

    let Ok(path) = std::env::var("OG_CONNECTORS") else {
        return Ok(opengrok_server::connections::routes::Connectors {
            providers: Arc::new(std::collections::BTreeMap::new()),
            redirect_uri,
        });
    };

    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("could not read OG_CONNECTORS at {path}"))?;
    let configs: Vec<ProviderConfig> = serde_json::from_str(&text)
        .with_context(|| format!("{path} is not a list of provider configurations"))?;

    let providers = configs
        .into_iter()
        .map(|config| (config.connector.clone(), config))
        .collect::<std::collections::BTreeMap<_, _>>();

    tracing::info!(
        connectors = ?providers.keys().collect::<Vec<_>>(),
        "connector providers configured"
    );

    Ok(opengrok_server::connections::routes::Connectors {
        providers: Arc::new(providers),
        redirect_uri,
    })
}

/// Plugins installed on this server, from the directory named by `OG_PLUGINS_DIR`.
///
/// A plugin that will not load is SKIPPED with a warning rather than failing the boot: one bad
/// folder must not take a server down, and the others are still useful.
fn load_plugins() -> std::collections::BTreeMap<String, opengrok_plugins::Plugin> {
    let Ok(dir) = std::env::var("OG_PLUGINS_DIR") else {
        return std::collections::BTreeMap::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        tracing::warn!(dir, "OG_PLUGINS_DIR could not be read; no plugins loaded");
        return std::collections::BTreeMap::new();
    };

    let mut plugins = std::collections::BTreeMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        match opengrok_plugins::Plugin::load(&path) {
            Ok(plugin) => {
                tracing::info!(
                    plugin = plugin.manifest.name,
                    skills = plugin.skills.len(),
                    servers = plugin.mcp.servers.len(),
                    trust = ?plugin.trust,
                    "loaded a plugin"
                );
                plugins.insert(plugin.manifest.name.clone(), plugin);
            }
            Err(error) => {
                tracing::warn!(path = ?path, %error, "skipping a plugin that would not load");
            }
        }
    }
    plugins
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
