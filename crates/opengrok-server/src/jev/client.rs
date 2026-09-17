//! The live door: TypeSafe's own SDK, configured once at boot.
//!
//! RETRIES AND TIMEOUTS ARE THE SDK'S, NOT OURS. It already retries a connection failure, a
//! timeout and a 408/429/5xx twice after the first attempt, backing off 0.5s to 5s with jitter
//! and giving up on a thirty-second budget — and it honours a `retry-after` when the service
//! sends one. A second retry loop wrapped around that one would multiply the attempts rather than
//! add to them, and a caller holding a turn open would wait the product of two patiences. So the
//! only thing this file does about retrying is hand the SDK a policy: the same policy it ships,
//! with the numbers a deployment chose to change.

use std::sync::Arc;
use std::time::Duration;

use typesafe_sdk::{Client, Error, RetryPolicy, SystemOneOpts};

use super::{Ask, JevDoor, JevError, Judgement, SharedJev};

/// How this deployment reaches Jev. Built from the environment at boot (`from_env`), or by hand
/// in a test that wants a client pointed at a stand-in.
///
/// NO `Debug` DERIVE, EVER: this holds the API key, and a derived one puts it in any log line
/// that prints a config. The hand-written one below redacts it.
#[derive(Clone)]
pub struct JevConfig {
    /// TypeSafe's API key. Never logged, never echoed in a reply, never stored — it lives in the
    /// process environment and in the header the SDK sets.
    pub api_key: String,
    /// Where the questions go. Always set explicitly, even to the SDK's own default, because the
    /// SDK falls back to `TYPESAFE_BASE_URL` when a builder leaves this unset — and a variable
    /// that silently retargets a deployment's classifier to somebody else's endpoint is not a
    /// setting this server should let through.
    pub base_url: String,
    /// The Jev model asked for when a question names none. Set explicitly for the same reason as
    /// the base URL: unset, the SDK reads `TYPESAFE_DEFAULT_MODEL`.
    pub model: String,
    /// What ONE attempt may take. The SDK's default is ten seconds; the retry policy below then
    /// decides whether a timed-out attempt is tried again.
    pub timeout: Duration,
    /// The SDK's own policy, as shipped unless a deployment changed a number.
    pub retry: RetryPolicy,
}

impl std::fmt::Debug for JevConfig {
    /// Hand-written so the API key cannot reach a log through a derived `Debug`, the way
    /// `GatewayAdmin` does it for the gateway's admin token.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JevConfig")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("timeout", &self.timeout)
            .field("api_key", &"«redacted»")
            .finish()
    }
}

impl JevConfig {
    /// A configuration with this deployment's key and every other setting at its default.
    #[must_use]
    pub fn with_key(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: typesafe_sdk::DEFAULT_BASE_URL.to_string(),
            model: typesafe_sdk::DEFAULT_MODEL.to_string(),
            timeout: Duration::from_secs_f64(typesafe_sdk::DEFAULT_TIMEOUT_SECS),
            retry: RetryPolicy::default(),
        }
    }

    /// From the environment, or `None` when this deployment has not been given a key — which is a
    /// legitimate deployment, and must read as "Jev is not configured" rather than as a crash.
    ///
    /// `OG_JEV_API_KEY`, not the SDK's own `TYPESAFE_API_KEY`: every other knob this server reads
    /// is `OG_`-prefixed, and accepting both would give one setting two names, so a server could
    /// be configured by a variable that is not in its own documentation.
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("OG_JEV_API_KEY")
            .ok()
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty())?;
        let mut config = Self::with_key(api_key);
        if let Some(base_url) = non_empty("OG_JEV_BASE_URL") {
            config.base_url = base_url.trim_end_matches('/').to_string();
        }
        if let Some(model) = non_empty("OG_JEV_MODEL") {
            config.model = model;
        }
        if let Some(timeout) = millis("OG_JEV_TIMEOUT_MS") {
            config.timeout = timeout;
        }
        if let Some(budget) = millis("OG_JEV_BUDGET_MS") {
            config.retry.timeout = Some(budget);
        }
        Some(config)
    }
}

fn non_empty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// A duration in milliseconds, or `None` — and a warning when the value was there but unusable.
///
/// Zero is refused rather than read as "off". A zero per-attempt timeout is not a timeout at all,
/// and a zero retry budget would mean "never retry" to one reader and "retry forever" to the
/// next; the shipped default is a better answer than either guess.
fn millis(name: &str) -> Option<Duration> {
    let raw = non_empty(name)?;
    match raw.parse::<u64>() {
        Ok(millis) if millis > 0 => Some(Duration::from_millis(millis)),
        _ => {
            tracing::warn!(
                variable = name,
                "not a positive whole number of milliseconds; keeping the shipped default"
            );
            None
        }
    }
}

/// The door this deployment asks, or `None` when it has no key — in which case the route says so
/// in a sentence instead of answering.
///
/// Resolved ONCE, at boot, beside the other doors (`AuthState::new`). The environment is not a
/// per-request input, and a client rebuilt per request would also rebuild an HTTP connection pool
/// per request.
pub fn from_env() -> Option<SharedJev> {
    let Some(config) = JevConfig::from_env() else {
        // Said at boot rather than left to the first refused request, the way a missing
        // `OG_CREDENTIAL_KEK` says connectors are off: the absence of a line is a poor signal in a
        // wall of startup logs, and an operator who mistyped the variable should learn it here.
        tracing::info!("no OG_JEV_API_KEY — Jev is unavailable on this server");
        return None;
    };
    match TypeSafeJev::new(config) {
        Ok(jev) => {
            tracing::info!(base_url = %jev.base_url(), model = %jev.model(), "Jev is configured");
            Some(Arc::new(jev))
        }
        Err(why) => {
            // A key is present but the client could not be built, so this deployment MEANT to have
            // Jev and does not. Loud, and then `None`: the route's refusal says no key is
            // configured, which is nearly true and is the only sentence it can offer — this line
            // is where the real reason is written down.
            tracing::error!(%why, "OG_JEV_API_KEY is set but the Jev client could not be built");
            None
        }
    }
}

/// Jev over TypeSafe's SDK.
pub struct TypeSafeJev {
    client: Client,
    base_url: String,
    model: String,
}

impl std::fmt::Debug for TypeSafeJev {
    /// Hand-written, and NOT delegating to the SDK's client: `typesafe_sdk::Client` derives
    /// `Debug` over a config that holds the API key in a plain `String`, so `{:?}` on it prints
    /// the credential. Printing where it points and nothing else is the whole of what a log line
    /// here is allowed to say.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TypeSafeJev")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl TypeSafeJev {
    /// Build the client, or the sentence saying why it could not be built.
    pub fn new(config: JevConfig) -> Result<Self, String> {
        let client = Client::builder()
            .api_key(config.api_key)
            .base_url(config.base_url.clone())
            .model(config.model.clone())
            .timeout(config.timeout)
            .retry(config.retry)
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            client,
            base_url: config.base_url,
            model: config.model,
        })
    }

    /// Where this door points. What the redacted `Debug` prints, and what a test asserts to show
    /// that no client here was left to inherit an address from the environment.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The model asked for when a question names none.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }
}

#[async_trait::async_trait]
impl JevDoor for TypeSafeJev {
    async fn ask(&self, ask: Ask) -> Result<Judgement, JevError> {
        let asked = ask.questions.len();
        let options = SystemOneOpts {
            model: ask.model,
            ..SystemOneOpts::default()
        };
        let answered = self
            .client
            .system_one_opts(ask.state, ask.questions, options)
            .await
            .map_err(|error| {
                let error = from_sdk(error);
                tracing::warn!(%error, asked, "Jev did not answer");
                error
            })?;
        // The accounting trail (see the module note): the counts and the id come back on every
        // answer, and this is where they are written down. The server has nowhere to bank them —
        // the gateway owns the ledger and Jev does not pass through it — so a log line and the
        // reply are the whole of it, and neither of them carries the key.
        let request_id = answered.request_id().ok().map(str::to_string);
        tracing::info!(
            model = %answered.model,
            request_id = request_id.as_deref().unwrap_or("-"),
            input_tokens = ?answered.usage.input_tokens,
            output_tokens = ?answered.usage.output_tokens,
            asked,
            answered = answered.answers.len(),
            "Jev answered"
        );
        Ok(Judgement {
            model: answered.model,
            request_id,
            usage: answered.usage,
            answers: answered.answers.into_iter().collect(),
        })
    }
}

/// The SDK's four failures, kept as four.
fn from_sdk(error: Error) -> JevError {
    match error {
        Error::Sdk(message) => JevError::Asked(message),
        Error::Connection { message } => JevError::Unreachable(message),
        Error::Timeout { timeout } => JevError::TimedOut(timeout),
        // `ApiError`'s own `Display` is already the sentence: the endpoint with its query and any
        // userinfo stripped, the status, the service's message truncated to 200 characters, and
        // the request id. None of it is the key.
        Error::Api(api) => JevError::Refused {
            status: api.status,
            message: api.to_string(),
        },
    }
}
