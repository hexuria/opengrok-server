//! The model catalogue — what a person may pin a coworker to, and a way to prove a pin works.
//!
//! THE KEY NEVER LEAVES THIS PROCESS. A picker needs the list of routes the gateway will serve,
//! and the only credential that can ask for it is the deployment's `oag_live_` key. So the server
//! asks on the browser's behalf and returns ids: the browser talks to OpenGrok, never to the
//! gateway. A 200 carrying that key in a header or a body would be a ship-blocker, which is why
//! the reply is built from parsed ids rather than by forwarding anything.
//!
//! An empty catalogue is `[]` **with a reason**, never a bare 200 — "there are no models in the
//! world" is exactly the empty success that reads as a working page and is not (CLAUDE.md §3).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a listing is considered fresh. A picker showing a slightly stale catalogue is a much
/// smaller problem than one that re-asks the gateway on every keystroke.
const FRESH_FOR: Duration = Duration::from_secs(60);

/// The smallest gap between two probes from one account. A probe is a REAL, billed completion on
/// the deployment's own key, so an unbounded one is somebody else's money in a loop. One every few
/// seconds is far more than a person clicking Test needs and far less than a script wants.
const PROBE_EVERY: Duration = Duration::from_secs(3);

/// The longest gateway sentence worth passing on. Long enough for a real explanation, short enough
/// that a gateway echoing a whole request cannot dump it into a browser.
const DETAIL_CLIP: usize = 300;

/// Scrub anything key-shaped out of a string the GATEWAY produced before it reaches a caller.
///
/// `list()` solves this by never passing a body on at all. `probe()` cannot: its whole value is
/// the gateway's own sentence ("no credential available for provider anthropic on this route"),
/// which is what tells a person why a pin they can see advertised will not serve them. So the
/// sentence travels, and anything that looks like a credential in it does not — because the one
/// thing this module must never do is hand a browser the key it holds, and a gateway that echoes
/// the failed request would otherwise do exactly that through us.
fn redact_secrets(detail: &str) -> String {
    let mut out = String::with_capacity(detail.len());
    for word in detail.split_inclusive(char::is_whitespace) {
        let bare = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-');
        let secretish = bare.len() >= 16
            && (bare.starts_with("oag_")
                || bare.starts_with("sk-")
                || bare.starts_with("Bearer")
                || bare.starts_with("bearer")
                || bare
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
        if secretish {
            out.push_str("«redacted» ");
        } else {
            out.push_str(word);
        }
    }
    let out = out.trim().to_string();
    if out.chars().count() > DETAIL_CLIP {
        let clipped: String = out.chars().take(DETAIL_CLIP).collect();
        return format!("{clipped}… (clipped)");
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub id: String,
    /// `oag.context_window`: how many tokens the route reads. Null on virtual entries
    /// (`oag/auto`), whose model is chosen per request.
    pub context_window: Option<u64>,
    /// `oag.alias_of`: the canonical id an `@sub`/`@api` entry is a channel of.
    pub alias_of: Option<String>,
}

/// The context a turn is held to when the catalogue cannot say: `OG_CONTEXT_TOKENS`.
///
/// A DEFAULT, NOT "UNKNOWN MEANS UNGUARDED" (#90). The shipping pin `xai/grok-4.6` is served but
/// never advertised (docs/setup/environment.md), so with no default the one route every hire
/// starts on would be the one never guarded. 128k is the smallest window among the routes we
/// ship; a larger model only trims later than it could.
pub const DEFAULT_CONTEXT_TOKENS: u64 = 128_000;

/// How long a turn waits for the catalogue before it falls back. A turn is not a picker: it
/// must not pay the listing's ten seconds for a gateway that is down.
const CONTEXT_LOOKUP: Duration = Duration::from_secs(3);

/// `OG_CONTEXT_TOKENS`: unset ⇒ the default; `0` ⇒ no guard at all; a number ⇒ that fallback.
pub fn context_setting_from_env() -> Option<u64> {
    match std::env::var("OG_CONTEXT_TOKENS")
        .ok()
        .map(|raw| raw.trim().parse::<u64>())
    {
        Some(Ok(0)) => None,
        Some(Ok(tokens)) => Some(tokens),
        Some(Err(_)) => {
            tracing::warn!("OG_CONTEXT_TOKENS is not a number; using {DEFAULT_CONTEXT_TOKENS}");
            Some(DEFAULT_CONTEXT_TOKENS)
        }
        None => Some(DEFAULT_CONTEXT_TOKENS),
    }
}

/// How many tokens a turn on `pin` may send: the gateway's word for it when it has one, else
/// `setting`. `setting` of `None` turns the guard off, whatever the catalogue says.
pub async fn context_for(
    catalogue: Option<&ModelCatalogue>,
    setting: Option<u64>,
    pin: &str,
) -> Option<u64> {
    let fallback = setting?;
    match catalogue {
        Some(catalogue) => Some(catalogue.context_window(pin).await.unwrap_or(fallback)),
        None => Some(fallback),
    }
}

/// The advertised window for `pin`: its own entry, else the entry it is a channel of, else the
/// same model on another channel. An exact entry with no window (a virtual route) is final: its
/// model is picked per request, and borrowing another's window would be a guess.
fn context_of(models: &[Model], pin: &str) -> Option<u64> {
    if let Some(exact) = models.iter().find(|model| model.id == pin) {
        return exact.context_window;
    }
    let base = crate::points::base_model(pin);
    models
        .iter()
        .find(|model| model.alias_of.as_deref() == Some(pin))
        .or_else(|| {
            models.iter().find(|model| {
                crate::points::base_model(&model.id) == base
                    || model.alias_of.as_deref() == Some(base)
            })
        })
        .and_then(|model| model.context_window)
}

/// What the catalogue answered, and why it is empty when it is.
#[derive(Debug, Clone)]
pub struct Catalogue {
    pub models: Vec<Model>,
    pub note: Option<String>,
}

pub struct ModelCatalogue {
    base_url: String,
    key: String,
    http: reqwest::Client,
    cached: Mutex<Option<(Instant, Vec<Model>)>>,
    /// When a turn last asked the gateway for its listing. A failed listing is not cached, so
    /// without this a gateway that is down would cost every turn a lookup.
    looked_up: Mutex<Option<Instant>>,
    /// When each account last spent the deployment's money on a probe.
    probed: Mutex<HashMap<String, Instant>>,
}

impl std::fmt::Debug for ModelCatalogue {
    /// Hand-written so the key cannot reach a log through a derived `Debug` — the same rule
    /// `GatewayDoor` and `GatewayAdmin` follow.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelCatalogue")
            .field("base_url", &self.base_url)
            .field("key", &"«redacted»")
            .finish()
    }
}

impl ModelCatalogue {
    #[must_use]
    pub fn new(base_url: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            key: key.into(),
            http: reqwest::Client::new(),
            cached: Mutex::new(None),
            looked_up: Mutex::new(None),
            probed: Mutex::new(HashMap::new()),
        }
    }

    /// May this account probe right now? Records the attempt when it may.
    pub fn may_probe(&self, account: &str) -> bool {
        let mut probed = match self.probed.lock() {
            Ok(probed) => probed,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(last) = probed.get(account)
            && last.elapsed() < PROBE_EVERY
        {
            return false;
        }
        probed.insert(account.to_string(), Instant::now());
        true
    }

    /// The same two variables the model door is built from, so the picker can never advertise a
    /// gateway the runs do not use. `None` when the deployment has no gateway (the mock doors).
    pub fn from_env() -> Option<Self> {
        let key = std::env::var("OG_GATEWAY_TOKEN")
            .ok()
            .filter(|key| !key.is_empty())?;
        let base_url = std::env::var("OG_GATEWAY_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:29080".to_string());
        Some(Self::new(base_url, key))
    }

    fn fresh(&self) -> Option<Vec<Model>> {
        let cached = match self.cached.lock() {
            Ok(cached) => cached,
            Err(poisoned) => poisoned.into_inner(),
        };
        cached
            .as_ref()
            .filter(|(at, _)| at.elapsed() < FRESH_FOR)
            .map(|(_, models)| models.clone())
    }

    /// What a turn reads: the last listing of any age, and one lookup a minute at most.
    async fn context_window(&self, pin: &str) -> Option<u64> {
        let due = {
            let mut looked_up = match self.looked_up.lock() {
                Ok(looked_up) => looked_up,
                Err(poisoned) => poisoned.into_inner(),
            };
            let due =
                self.fresh().is_none() && looked_up.is_none_or(|at| at.elapsed() >= FRESH_FOR);
            if due {
                *looked_up = Some(Instant::now());
            }
            due
        };
        if due {
            self.list_within(CONTEXT_LOOKUP).await;
        }
        let cached = match self.cached.lock() {
            Ok(cached) => cached,
            Err(poisoned) => poisoned.into_inner(),
        };
        context_of(&cached.as_ref()?.1, pin)
    }

    fn remember(&self, models: &[Model]) {
        let mut cached = match self.cached.lock() {
            Ok(cached) => cached,
            Err(poisoned) => poisoned.into_inner(),
        };
        *cached = Some((Instant::now(), models.to_vec()));
    }

    /// The routes this gateway advertises. Never an error: a gateway that cannot be listed yields
    /// an empty catalogue and the reason, because a picker that cannot offer a list must still let
    /// somebody type a route by hand.
    pub async fn list(&self) -> Catalogue {
        self.list_within(Duration::from_secs(10)).await
    }

    async fn list_within(&self, timeout: Duration) -> Catalogue {
        if let Some(models) = self.fresh() {
            return Catalogue { models, note: None };
        }
        let response = self
            .http
            .get(format!("{}/v1/models", self.base_url))
            .bearer_auth(&self.key)
            .timeout(timeout)
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                return Catalogue {
                    models: Vec::new(),
                    note: Some(redact_secrets(&format!(
                        "the gateway could not be reached: {error}"
                    ))),
                };
            }
        };
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Catalogue {
                models: Vec::new(),
                // The gateway's status, never its body: a body could echo the request, and the
                // request carried the key.
                note: Some(format!(
                    "the gateway answered {status} when asked for its models"
                )),
            };
        }
        let models = parse_models(&body);
        if models.is_empty() {
            return Catalogue {
                models,
                note: Some(
                    "the gateway advertises no models on this key's route — a pin can still be \
                     typed by hand"
                        .to_string(),
                ),
            };
        }
        self.remember(&models);
        Catalogue { models, note: None }
    }

    /// Ask the gateway to answer one tiny prompt on `model`. This is how a pin is proven BEFORE it
    /// is saved — the alternative is discovering it at the first real turn, which the person who
    /// typed it is no longer watching.
    ///
    /// It offers one trivial tool exactly as a coworker's turn offers its computer (`tool_choice`
    /// "auto", as `GatewayDoor` sends), so it asks what a real turn asks. "Say ok" proved a route
    /// answers, never that it can act: `gpt-5.6-luna` passed it while making zero tool calls
    /// (ROADMAP, "Blocked on the operator").
    pub async fn probe(&self, model: &str) -> Result<Probed, String> {
        let response = self
            .http
            .post(format!("{}/v1/chat/completions", self.base_url))
            .bearer_auth(&self.key)
            .timeout(Duration::from_secs(30))
            .json(&serde_json::json!({
                "model": model,
                "messages": [{"role": "user", "content": "Call the ping tool."}],
                "tools": [{"type": "function", "function": {"name": "ping", "description":
                    "Answers pong.", "parameters": {"type": "object", "properties": {}}}}],
                "tool_choice": "auto",
                // Room for the call and for any reasoning a route counts against this: a clipped
                // call reads as no call — a false "cannot act" on a working route.
                "max_tokens": 512,
            }))
            .send()
            .await
            .map_err(|error| {
                redact_secrets(&format!("the gateway could not be reached: {error}"))
            })?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let parsed: serde_json::Value =
            serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
        if !status.is_success() {
            // The gateway's own sentence names the real problem ("no credential available for
            // provider anthropic on this route"), which is worth far more than our paraphrase.
            // Only the message STRING, never the whole error object (which could carry
            // structured echoes of the request), and scrubbed on the way out.
            let detail = parsed
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(|message| message.as_str())
                .map_or_else(|| format!("the gateway answered {status}"), redact_secrets);
            return Err(detail);
        }
        let served = parsed.get("model").and_then(|model| model.as_str());
        Ok(Probed {
            served: served.unwrap_or(model).to_string(),
            // Anything unrecognised is `false`: the probe may understate a route, never overstate.
            tool_calls: parsed
                .pointer("/choices/0/message/tool_calls")
                .and_then(|calls| calls.as_array())
                .is_some_and(|calls| !calls.is_empty()),
        })
    }
}

/// What a probe proved. `tool_calls: false` is not a failure: a route that only talks is a working
/// pin for a coworker with no computer, and a useless one for a coworker with one.
pub struct Probed {
    pub served: String,
    pub tool_calls: bool,
}

/// Ids out of an OpenAI-shaped `/v1/models` body, with the gateway's `oag` window where it gives
/// one. Unknown fields are ignored, and a body that is not what we expected yields nothing rather
/// than a guess.
fn parse_models(body: &str) -> Vec<Model> {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    parsed
        .get("data")
        .and_then(|data| data.as_array())
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let id = row.get("id").and_then(|id| id.as_str())?;
                    Some(Model {
                        id: id.to_string(),
                        context_window: row
                            .pointer("/oag/context_window")
                            .and_then(|tokens| tokens.as_u64()),
                        alias_of: row
                            .pointer("/oag/alias_of")
                            .and_then(|alias| alias.as_str())
                            .map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "../tests/unit/models.rs"]
mod tests;
