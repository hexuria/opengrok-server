//! Where a person's turns are answered: open-ai-gateway, or their own subscription (CLAUDE.md #4).
//!
//! The own-subscription door is an OpenAI-compatible proxy (opencodex) that holds the person's
//! provider sign-in itself, so no provider credential ever reaches us. It is reached one of two
//! ways (`Via`): on the loopback of the machine THIS SERVER runs on, which is why a base URL is
//! loopback or nothing (`opengrok_harness::local_proxy`), or through the person's own Mac, which
//! holds a relay stream open to this server and carries the call to its own opencodex
//! (`opengrok_harness::relay`, #292) — never a wider address here.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::coworker::{Coworker, Effort};

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

/// Which way a person's own subscription is reached: the proxy on this server's loopback, or
/// their own Mac carrying the call over the relay stream it holds (#292). The words are the ones
/// agreed with NativeChat. Its third, `helper`, is not built (#293), and is refused by name
/// rather than read as either: a turn sent a way the person did not choose is a re-route.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    #[default]
    Loopback,
    Mac,
}

impl Via {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Loopback => "loopback",
            Self::Mac => "mac",
        }
    }

    /// A via a request names: absent, null or `""` names none, which is the account's default.
    /// Otherwise the wire word exactly, as `SourceKind::parse` reads a kind.
    pub fn named(value: Option<&Value>) -> Result<Option<Self>, &'static str> {
        match value {
            None | Some(Value::Null) => Ok(None),
            Some(word) => match word.as_str() {
                Some("") => Ok(None),
                Some("loopback") => Ok(Some(Self::Loopback)),
                Some("mac") => Ok(Some(Self::Mac)),
                Some("helper") => {
                    Err("\"helper\" is not built yet (#293); use \"loopback\" or \"mac\"")
                }
                _ => Err("must be \"loopback\" or \"mac\""),
            },
        }
    }
}

/// Where one turn asks, as a request names it (`forwardedProps.inferenceSource`, a queued send)
/// or a run captured it: the kind, and on the proxy which way. `None` via is the account's
/// default when a request names it, and the loopback on a run logged before the relay existed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnSource {
    pub kind: SourceKind,
    pub via: Option<Via>,
}

impl From<SourceKind> for TurnSource {
    fn from(kind: SourceKind) -> Self {
        Self { kind, via: None }
    }
}

impl TurnSource {
    /// A turn's own pick (a queued send's, else its request's) over its coworker's own source,
    /// which names a kind and never a way. THE ONE MERGE of the two, taken before any setting both
    /// by `local_proxy::route` and by a queued send asking whether it waits for the person's Mac
    /// (`InferenceSource::by_mac`), so they cannot disagree about where a send goes (#304 review).
    pub fn picked(chosen: Option<Self>, coworker: Option<SourceKind>) -> Option<Self> {
        chosen.or(coworker.map(Self::from))
    }

    /// What a request names: absent or null names nothing, a wire word names a kind, and
    /// `{kind, via?}` names both. The refusal is a sentence under the field's name.
    pub fn named(value: Option<&Value>, field: &str) -> Result<Option<Self>, String> {
        let Some(Value::Object(object)) = value else {
            let kind = SourceKind::named(value).map_err(|why| format!("{field} {why}"))?;
            return Ok(kind.map(Self::from));
        };
        let kind = SourceKind::named(object.get("kind"))
            .map_err(|why| format!("{field}.kind {why}"))?
            .ok_or_else(|| format!("{field}.kind must be \"gateway\" or \"local_proxy\""))?;
        let via = Via::named(object.get("via")).map_err(|why| format!("{field}.via {why}"))?;
        Ok(Some(Self { kind, via }))
    }

    /// As a reply echoes it: the word alone when no via was named, else `{kind, via}`.
    pub fn to_value(self) -> Value {
        match self.via {
            None => Value::from(self.kind.as_str()),
            Some(via) => serde_json::json!({ "kind": self.kind.as_str(), "via": via.as_str() }),
        }
    }

    /// As a queued send's row keeps it: `local_proxy`, or `local_proxy:mac` with a via. A column
    /// of plain words before the relay, which still read as themselves.
    pub fn stored(self) -> String {
        match self.via {
            None => self.kind.as_str().to_string(),
            Some(via) => format!("{}:{}", self.kind.as_str(), via.as_str()),
        }
    }

    pub fn from_stored(text: &str) -> Option<Self> {
        let (kind, via) = text.split_once(':').unwrap_or((text, ""));
        Some(Self {
            kind: SourceKind::parse(kind)?,
            via: Via::named(Some(&Value::from(via))).ok()?,
        })
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
    /// The way a proxy turn goes when it names none; `None` is the loopback, where every turn
    /// went before the relay. Absent from the log until someone sets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<Via>,
    /// The model the person's Mac is asked for, apart from `local_model`: the Mac's opencodex is
    /// not the loopback's, and need not serve the same ids. Held to `subscription_model` too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_model: Option<String>,
    /// What a bot is born on when its hire and its template name no model (#318). Absent from
    /// the log until set, so every event from before it reads as none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_bot_default: Option<NewBotDefault>,
    /// The person's computers whose relay is on: each un-revoked enrolled machine whose own switch
    /// (`local_exec_daemon.relay_enabled`) is on, filled on every read of the setting
    /// (`local_proxy::Saved::setting`) and never logged, so a switch counts from the next turn.
    /// NONE IS "RELAY OFF", FOR THE MAC'S WAY ALONE: a turn that would go by the Mac goes to the
    /// gateway on `plan_fallback`, or is refused. The loopback never reads it, so a plan on this
    /// server's own machine, with no Mac enrolled, is never off. #332 kept an account-wide switch
    /// here as `relay_off`; an event that carries it replays with it unread.
    #[serde(skip)]
    pub relays: Vec<String>,
    /// What answers in the Mac's place while the relay is off (#332). Absent until set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_fallback: Option<PlanFallback>,
}

/// The Server model a turn by the person's Mac asks while they switched the relay off, and how hard
/// it thinks (#332): the gateway's, metered there, never written onto a bot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanFallback {
    pub model: String,
    #[serde(default)]
    pub effort: Effort,
}

impl PlanFallback {
    /// `planFallback` as a `PUT /account/inference-source` names it: null is none; an object's model
    /// is held to what a hire's is on the gateway, and its effort to a coworker's words.
    pub fn named(value: &Value) -> Result<Option<Self>, String> {
        let object = match value {
            Value::Null => return Ok(None),
            Value::Object(object) => object,
            _ => return Err("planFallback must be an object or null".to_string()),
        };
        let model = object
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let model = Coworker::non_blank(model.to_string());
        let model = model.map_err(|why| format!("planFallback.model: {why}"))?;
        let effort = Effort::named(object.get("effort"));
        let effort = effort.map_err(|why| format!("planFallback.effort {why}"))?;
        let effort = effort.unwrap_or_default();
        Ok(Some(Self { model, effort }))
    }
}

/// What saving a setting does to the proxy's key. No `Debug`: `Set` holds the key.
pub enum KeyChange {
    Keep,
    Set(String),
    Clear,
}

/// A person's default for new bots: the door, the model and how hard it thinks, ALL WRITTEN ONTO
/// THE BOT AT ITS HIRE, so nothing about a bot is read from its owner's account afterwards and a
/// changed default moves only the bots hired after it. The words are the ones agreed with
/// NativeChat on 2 Oct 2026; NativeChat's Fast is the `--fast` id, not a field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewBotDefault {
    pub source: SourceKind,
    pub model: String,
    #[serde(default)]
    pub effort: Effort,
}

impl NewBotDefault {
    /// `newBotDefault` as a `PUT /account/inference-source` names it: null is none, and an object
    /// is held to what its parts are held to elsewhere, in their words: on the gateway its model
    /// to what a hire's is, on a person's own plan to `subscription_model` (as a `localModel` is),
    /// and its effort to a coworker's words. Refused whole, before anything is saved.
    pub fn named(value: &Value) -> Result<Option<Self>, String> {
        let object = match value {
            Value::Null => return Ok(None),
            Value::Object(object) => object,
            _ => return Err("newBotDefault must be an object or null".to_string()),
        };
        let source = SourceKind::named(object.get("source")).ok().flatten();
        let source = source.ok_or("newBotDefault.source must be \"gateway\" or \"local_proxy\"")?;
        let model = object
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let model = match source {
            SourceKind::Gateway => {
                Coworker::non_blank(model.to_string()).map_err(|e| e.to_string())
            }
            SourceKind::LocalProxy => subscription_model(model).map(|()| model.trim().to_string()),
        };
        let model = model.map_err(|why| format!("newBotDefault.model: {why}"))?;
        let effort = Effort::named(object.get("effort"));
        let effort = effort.map_err(|why| format!("newBotDefault.effort {why}"))?;
        Ok(Some(Self {
            source,
            model,
            effort: effort.unwrap_or_default(),
        }))
    }
}

impl InferenceSource {
    /// The source a turn opens on. The turn's own word wins: the client configures, and a person
    /// who picked a source for this message meant this message.
    pub fn for_turn(&self, chosen: Option<SourceKind>) -> SourceKind {
        chosen.unwrap_or(self.kind)
    }

    /// Where a turn that named `chosen` goes over this setting: its kind, and on the proxy which
    /// way. The turn's own words win, each of the two on its own; an omitted via is this
    /// setting's default, and the loopback when there is none.
    pub fn resolve(&self, chosen: Option<TurnSource>) -> (SourceKind, Via) {
        let kind = chosen.map_or(self.kind, |chosen| chosen.kind);
        let via = chosen.and_then(|chosen| chosen.via).or(self.via);
        (kind, via.unwrap_or_default())
    }

    /// Whether a turn that named `chosen`, with a coworker whose own source is `coworker`, goes
    /// by the person's Mac over this setting: the question a queued send asks to know whether it
    /// waits for one (opengrok-server `pending::Held`), resolved as the turn itself is. Never
    /// while the relay is off (#332), no computer's on: such a turn goes to the fallback or is
    /// refused, at once.
    pub fn by_mac(&self, chosen: Option<TurnSource>, coworker: Option<SourceKind>) -> bool {
        let way = self.resolve(TurnSource::picked(chosen, coworker));
        !self.relays.is_empty() && way == (SourceKind::LocalProxy, Via::Mac)
    }

    /// The setting a `PUT /account/inference-source` body asks for over this one, what it does to
    /// the key, and the relay switch it names, or the sentence it is refused with; `base` judges
    /// an address (`opengrok_harness::local_proxy::loopback_base`). A field absent keeps what is
    /// saved; `null` or blank clears it, and so for `via`, `relay.localModel`, `newBotDefault` and
    /// `planFallback` (null only); `relayEnabled` is true or false, is not part of the setting
    /// (the caller switches every one of the person's computers to it), and SWITCHES NO WAY: the
    /// app sends `via` beside it when it means one (#332). EVERY RULE IS ASKED HERE, BEFORE
    /// ANYTHING IS WRITTEN: an address that is not this machine, or a model the terms forbid,
    /// beside a fresh key saves neither. The key goes out in a header, so it is printable and
    /// bounded or refused now, not at a turn.
    pub fn applied(
        &self,
        body: &Value,
        base: impl Fn(&str) -> Result<String, String>,
    ) -> Result<(Self, KeyChange, Option<bool>), String> {
        let text = |object: &Value, field: &str| -> Result<Option<Option<String>>, String> {
            match object.get(field) {
                None => Ok(None),
                Some(Value::Null) => Ok(Some(None)),
                Some(Value::String(text)) => Ok(Some(
                    Some(text.trim().to_string()).filter(|t| !t.is_empty()),
                )),
                Some(_) => Err(format!("{field} must be a string or null")),
            }
        };
        let model =
            |model: Option<String>, field: &str| match model.as_deref().map(subscription_model) {
                Some(Err(why)) => Err(format!("{field}: {why}")),
                _ => Ok(model),
            };
        let mut source = self.clone();
        source.kind = SourceKind::named(body.get("kind"))
            .ok()
            .flatten()
            .ok_or("kind must be \"gateway\" or \"local_proxy\"")?;
        if let Some(chosen) = text(body, "baseUrl")? {
            source.base_url = chosen
                .as_deref()
                .map(&base)
                .transpose()
                .map_err(|why| format!("baseUrl: {why}"))?;
        }
        if let Some(chosen) = text(body, "localModel")? {
            source.local_model = model(chosen, "localModel")?;
        }
        if let Some(via) = body.get("via") {
            source.via = Via::named(Some(via)).map_err(|why| format!("via {why}"))?;
        }
        // THE MAC'S MODEL IS ITS OWN, held to the same allowlist: the Mac's opencodex is not this
        // machine's, and need not serve the ids the loopback's does.
        match body.get("relay") {
            None => {}
            Some(Value::Null) => source.relay_model = None,
            Some(relay @ Value::Object(_)) => {
                let chosen = text(relay, "localModel").map_err(|why| format!("relay.{why}"))?;
                if let Some(chosen) = chosen {
                    source.relay_model = model(chosen, "relay.localModel")?;
                }
            }
            Some(_) => return Err("relay must be an object or null".to_string()),
        }
        if let Some(chosen) = body.get("newBotDefault") {
            source.new_bot_default = NewBotDefault::named(chosen)?;
        }
        let switched = body.get("relayEnabled").map(Value::as_bool);
        let switched = switched
            .map(|on| on.ok_or("relayEnabled must be true or false"))
            .transpose()?;
        if let Some(chosen) = body.get("planFallback") {
            source.plan_fallback = PlanFallback::named(chosen)?;
        }
        let key = match text(body, "apiKey")? {
            None => KeyChange::Keep,
            Some(None) => KeyChange::Clear,
            Some(Some(key)) if key.len() <= 512 && key.bytes().all(|b| b.is_ascii_graphic()) => {
                KeyChange::Set(key)
            }
            Some(Some(_)) => return Err("apiKey must be printable ASCII, 512 at most".to_string()),
        };
        source.has_key = match key {
            KeyChange::Keep => source.has_key,
            KeyChange::Set(_) => true,
            KeyChange::Clear => false,
        };
        Ok((source, key, switched))
    }

    /// The efforts a `PUT` body names, by field, each with the door and model it is held to
    /// (opengrok-server `inference::effort_refused`): the default's on its own source, and the
    /// fallback's on the gateway, which answers it. One the body leaves out stands as saved.
    pub fn named_efforts(&self, body: &Value) -> [Option<(&'static str, Named<'_>)>; 2] {
        let default = self.new_bot_default.as_ref();
        let default = default.map(|d| (Some(d.source), d.model.as_str(), d.effort));
        let fallback = self.plan_fallback.as_ref();
        let fallback = fallback.map(|f| (Some(SourceKind::Gateway), f.model.as_str(), f.effort));
        let named = |field, chosen| Some((field, body.get(field).and(chosen)?));
        [
            named("newBotDefault", default),
            named("planFallback", fallback),
        ]
    }
}

/// An effort a save names, with the door and the model it is held to.
pub type Named<'a> = (Option<SourceKind>, &'a str, Effort);

/// Providers whose terms forbid routing a consumer subscription through a third-party app, so a
/// refusal can say why rather than call the model unrecognised.
const FORBIDDEN: &[(&str, &str)] = &[
    ("anthropic", "Anthropic"),
    ("claude", "Anthropic"),
    ("google", "Google"),
    ("gemini", "Google"),
];

/// THE ONE LIST OF WHAT A PERSON'S OWN SUBSCRIPTION MAY ANSWER: OpenAI's models and xAI's, bare
/// or as `openai/…` and `xai/…`, with or without opencodex's `--fast` tier (`gpt-6-sol--fast`,
/// `xai/grok-4.7--fast`). Those providers sell the subscriptions opencodex signs in with.
/// Anthropic and Google forbid using a consumer subscription through a third-party app, and are
/// refused by name wherever their names appear in an id: that match only ever refuses.
///
/// EVERY ALLOWED PATTERN IS ANCHORED. With the prefix and the tier taken off, the core STARTS with
/// `gpt-`, `o1`, `o3`, `o4` or `codex` (OpenAI's: `gpt-5-codex` and `codex-mini-latest` alike) or
/// `grok-` (xAI's), and a prefix must name the provider its core is from. An id that only
/// contains an allowed word (`my-codex-thing`, `notgrok-1`) is nobody's this server knows.
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
    let (provider, core) = id
        .split_once('/')
        .map_or((None, id.as_str()), |(p, n)| (Some(p), n));
    let core = core.strip_suffix("--fast").unwrap_or(core);
    let openai = ["gpt-", "o1", "o3", "o4", "codex"]
        .iter()
        .any(|start| core.starts_with(start));
    let xai = core.starts_with("grok-");
    let recognised = match provider {
        None => openai || xai,
        Some("openai") => openai,
        Some("xai") => xai,
        Some(_) => false,
    };
    let plain = id.len() <= 128
        && !core.contains('/')
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._:/".contains(&byte));
    if recognised && plain {
        return Ok(());
    }
    Err(format!(
        "{:?} is not a model this server knows to be OpenAI's or xAI's, and only theirs (gpt-*, \
         o1*, o3*, o4*, codex*, grok-*) may use your own subscription",
        model.trim()
    ))
}
