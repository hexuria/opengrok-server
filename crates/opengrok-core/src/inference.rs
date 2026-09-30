//! Where a person's turns are answered: open-ai-gateway, or their own subscription (CLAUDE.md #4).
//!
//! The own-subscription door is an OpenAI-compatible proxy (opencodex) listening on the loopback
//! of the machine THIS SERVER runs on; it holds the person's provider sign-in itself, so no
//! provider credential ever reaches us. Only that machine can reach it, which is why a base URL
//! is loopback or nothing (`opengrok_harness::local_proxy`): it serves a person only where server
//! and proxy share a machine. A remote deployment needs a relay — NativeChat carrying the call,
//! as local-exec does — not a wider address here.

use serde::{Deserialize, Serialize};

/// Where a turn's model calls go. The gateway is the default and is exactly what every turn did
/// before a person could choose, so an account that never set one, and a run logged before this
/// existed, read as it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    #[default]
    Gateway,
    LocalProxy,
}

impl SourceKind {
    /// The word the wire and the log use, as agreed with NativeChat.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gateway => "gateway",
            Self::LocalProxy => "local_proxy",
        }
    }

    /// The wire word, exactly. Anything else is no source at all, never a guess: a misspelt
    /// `local-proxy` read as the gateway would bill the person on a key they chose not to use.
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "gateway" => Some(Self::Gateway),
            "local_proxy" => Some(Self::LocalProxy),
            _ => None,
        }
    }

    /// A source a request names (`forwardedProps.inferenceSource`, `?source=`): absent or null
    /// names none, and anything but a wire word is refused rather than read as either.
    pub fn named(value: Option<&serde_json::Value>) -> Result<Option<Self>, &'static str> {
        match value {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(word) => word
                .as_str()
                .and_then(Self::parse)
                .map(Some)
                .ok_or("must be \"gateway\" or \"local_proxy\""),
        }
    }
}

/// A person's setting, as their account records it. The proxy's own key is NOT here: the log is
/// durable and exportable, so it records that a key exists and the vault holds what it is, the
/// bargain `connection.rs` makes for a token.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InferenceSource {
    #[serde(default)]
    pub kind: SourceKind,
    /// A loopback address, normalised where it was set.
    #[serde(default)]
    pub base_url: Option<String>,
    /// A model id the proxy serves, one `subscription_model` allows.
    #[serde(default)]
    pub local_model: Option<String>,
    /// Whether a key for the proxy is sealed in the vault.
    #[serde(default)]
    pub has_key: bool,
}

impl InferenceSource {
    /// The source a turn opens on. The turn's own word wins: the client configures, and a person
    /// who picked a source for this message meant this message.
    pub fn for_turn(&self, chosen: Option<SourceKind>) -> SourceKind {
        chosen.unwrap_or(self.kind)
    }
}

/// Providers whose terms forbid routing a consumer subscription through a third-party app, so a
/// refusal can say why rather than call the model unrecognised.
const FORBIDDEN: &[(&str, &str)] = &[
    ("anthropic", "Anthropic"),
    ("claude", "Anthropic"),
    ("google", "Google"),
    ("gemini", "Google"),
];

/// THE ONE LIST OF WHAT A PERSON'S OWN SUBSCRIPTION MAY ANSWER: OpenAI's models (`gpt-*`,
/// `o1*`/`o3*`/`o4*`, anything codex) and xAI's (`grok-*`), bare or as `openai/…` and `xai/…`,
/// with or without opencodex's `--fast` tier (`gpt-6-sol--fast`, `xai/grok-4.7--fast`). Those
/// providers sell the subscriptions opencodex signs in with. Anthropic and Google forbid using a
/// consumer subscription through a third-party app, and are refused by name.
///
/// AN ALLOWLIST, NOT A DENYLIST: an id this does not recognise is refused, so a provider nobody
/// has looked at cannot ride a person's subscription by being new. Asked where the setting is
/// saved and again at the door on every call, whatever path built the request.
pub fn subscription_model(model: &str) -> Result<(), String> {
    let id = model.trim().to_ascii_lowercase();
    if id.is_empty() {
        return Err("name a model the proxy serves".to_string());
    }
    if let Some((_, provider)) = FORBIDDEN.iter().find(|(word, _)| id.contains(word)) {
        return Err(format!(
            "{} is one of {provider}'s models, and {provider}'s terms forbid using a consumer \
             subscription through a third-party app; pick an OpenAI or xAI model, or use the \
             gateway",
            model.trim()
        ));
    }
    let (provider, name) = id
        .split_once('/')
        .map_or((None, id.as_str()), |(p, n)| (Some(p), n));
    let name = name.strip_suffix("--fast").unwrap_or(name);
    let openai = name.starts_with("gpt-")
        || ["o1", "o3", "o4"].iter().any(|o| name.starts_with(o))
        || name.contains("codex");
    let xai = name.starts_with("grok-");
    let recognised = match provider {
        None => openai || xai,
        Some("openai") => openai,
        Some("xai") => xai,
        Some(_) => false,
    };
    let plain = id.len() <= 128
        && !name.contains('/')
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._:/".contains(&byte));
    if recognised && plain {
        return Ok(());
    }
    Err(format!(
        "{:?} is not a model this server knows to be OpenAI's or xAI's, and only theirs (gpt-*, \
         o1, o3, o4, codex, grok-*) may use your own subscription",
        model.trim()
    ))
}
