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

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a listing is considered fresh. A picker showing a slightly stale catalogue is a much
/// smaller problem than one that re-asks the gateway on every keystroke.
const FRESH_FOR: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub id: String,
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
        }
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
        if let Some(models) = self.fresh() {
            return Catalogue { models, note: None };
        }
        let response = self
            .http
            .get(format!("{}/v1/models", self.base_url))
            .bearer_auth(&self.key)
            .timeout(Duration::from_secs(10))
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                return Catalogue {
                    models: Vec::new(),
                    note: Some(format!("the gateway could not be reached: {error}")),
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
    pub async fn probe(&self, model: &str) -> Result<String, String> {
        let response = self
            .http
            .post(format!("{}/v1/chat/completions", self.base_url))
            .bearer_auth(&self.key)
            .timeout(Duration::from_secs(30))
            .json(&serde_json::json!({
                "model": model,
                "messages": [{"role": "user", "content": "say ok"}],
                "max_tokens": 8,
            }))
            .send()
            .await
            .map_err(|error| format!("the gateway could not be reached: {error}"))?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let parsed: serde_json::Value =
            serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
        if !status.is_success() {
            // The gateway's own sentence names the real problem ("no credential available for
            // provider anthropic on this route"), which is worth far more than our paraphrase.
            let detail = parsed
                .get("error")
                .and_then(|error| error.get("message").or(Some(error)))
                .and_then(|message| message.as_str().map(str::to_string))
                .unwrap_or_else(|| format!("the gateway answered {status}"));
            return Err(detail);
        }
        let served = parsed
            .get("model")
            .and_then(|model| model.as_str())
            .unwrap_or(model);
        Ok(served.to_string())
    }
}

/// Ids out of an OpenAI-shaped `/v1/models` body. Unknown fields are ignored, and a body that is
/// not what we expected yields nothing rather than a guess.
fn parse_models(body: &str) -> Vec<Model> {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    parsed
        .get("data")
        .and_then(|data| data.as_array())
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.get("id").and_then(|id| id.as_str()))
                .map(|id| Model { id: id.to_string() })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_read_from_an_openai_shaped_listing() {
        let body = r#"{"object":"list","data":[{"id":"oag/auto"},{"id":"openai/gpt-5.5"}]}"#;
        let models = parse_models(body);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "oag/auto");
        assert_eq!(models[1].id, "openai/gpt-5.5");
    }

    /// A body we did not expect yields nothing — never a guessed id a person could pin to.
    #[test]
    fn an_unexpected_body_yields_no_models_rather_than_a_guess() {
        assert!(parse_models("not json").is_empty());
        assert!(parse_models(r#"{"error":"nope"}"#).is_empty());
        assert!(parse_models(r#"{"data":"not an array"}"#).is_empty());
    }

    #[test]
    fn debug_never_prints_the_key() {
        let catalogue = ModelCatalogue::new("http://gateway.local:29080", "oag_live_supersecret");
        let rendered = format!("{catalogue:?}");
        assert!(!rendered.contains("supersecret"), "{rendered}");
        assert!(rendered.contains("«redacted»"), "{rendered}");
    }
}
