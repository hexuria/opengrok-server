//! A person's own subscription, through the OpenAI-compatible proxy on this server's loopback
//! (opencodex): which base URLs this server may dial, the client that dials them, and the two
//! things asked of a proxy outside a turn — is it up, and what does it serve.
//!
//! ONLY THIS MACHINE. The proxy holds the person's provider sign-in and listens on loopback, so
//! it serves a person only where it runs beside the server. Anywhere else their own Mac carries
//! the call (`via: "mac"`, `relay`), not a wider address here: every other host is somebody
//! else's, and a URL a person may type would make this server a tunnel into its own network.
//!
//! NOTHING HERE FOLLOWS A REDIRECT, READS A PROXY SETTING OR ASKS DNS. A loopback address that
//! answers 302 to another host, an `HTTP_PROXY` in the server's environment, or a resolver that
//! maps `localhost` elsewhere would each carry the person's prompt, and their key, off the machine.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use opengrok_core::id::AccountId;
use opengrok_core::inference::{InferenceSource, SourceKind, TurnSource, Via, subscription_model};

use crate::model::ModelEndpoint;
use crate::relay::{RelayBroker, RelayTo};

/// The only header opencodex reads a key from on a non-loopback bind; `Authorization: Bearer` is
/// refused there (opencodex 2.22.0). On loopback it asks for none.
pub const KEY_HEADER: &str = "X-OpenCodex-API-Key";

/// How long `/healthz` and `/v1/models` may take. A proxy is a process on this machine: past
/// this it is not answering, and a settings page, asked on every read, must not hang on it.
const PROBE: Duration = Duration::from_secs(1);
const LISTING: Duration = Duration::from_secs(3);

/// The address as this server will dial it, or why it will not. Parsed by the same parser the
/// client dials with, and stored as that parser writes it back, so what was checked is what is
/// dialled: `http://0x7f.1` is `http://127.0.0.1`, and `0127.0.0.1` — octal, 87.0.0.1 — is
/// refused rather than read as 127 by a looser eye.
pub fn loopback_base(raw: &str) -> Result<String, String> {
    let refused = |why: &str| {
        format!(
            "{why}; give the address of the proxy on this server's own machine, with no path, \
             like http://127.0.0.1:8080"
        )
    };
    if raw.trim().len() > 256 {
        return Err(refused("that address is too long"));
    }
    let url = reqwest::Url::parse(raw.trim()).map_err(|_| refused("that is not a URL"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(refused("only http and https are allowed"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(refused("an address may not carry a user or a password"));
    }
    // Written back canonical by the parser: dotted IPv4, compressed IPv6 in brackets, a
    // lower-cased name. Loopback is 127.0.0.0/8, ::1, or the name `localhost` exactly, which the
    // client below never looks up.
    let host = url.host_str().unwrap_or_default();
    let loopback = host == "localhost"
        || host == "[::1]"
        || host.parse::<Ipv4Addr>().is_ok_and(|ip| ip.is_loopback());
    if !loopback {
        return Err(refused(&format!("{host:?} is not this machine")));
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err(refused(
            "the proxy's address takes no path, query or fragment",
        ));
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

/// The client every call to a proxy is made with: no redirects, no proxy setting from the
/// environment, and `localhost` pinned to loopback instead of resolved.
pub fn client(connect: Duration, read: Duration) -> Result<reqwest::Client, String> {
    let loopback = [
        SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, 0)),
    ];
    reqwest::Client::builder()
        .connect_timeout(connect)
        .read_timeout(read)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .resolve_to_addrs("localhost", &loopback)
        .build()
        .map_err(|error| error.to_string())
}

/// One client for the settings page's questions, built once: a client is a TLS setup.
fn asking() -> Result<&'static reqwest::Client, String> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| client(PROBE, LISTING))
        .as_ref()
        .map_err(Clone::clone)
}

/// Whether `GET {base}/healthz` answered 2xx in time. False for an address that is not loopback,
/// which is never asked.
pub async fn healthy(base: &str) -> bool {
    let (Ok(base), Ok(http)) = (loopback_base(base), asking()) else {
        return false;
    };
    http.get(format!("{base}/healthz"))
        .timeout(PROBE)
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
}

/// The ids `GET {base}/v1/models` lists that a person's subscription may use
/// (`opengrok_core::inference::subscription_model`), or why there are none to show.
pub async fn models(base: &str, key: Option<&str>) -> Result<Vec<String>, String> {
    let base = loopback_base(base)?;
    let mut ask = asking()?.get(format!("{base}/v1/models")).timeout(LISTING);
    if let Some(key) = key {
        ask = ask.header(KEY_HEADER, key);
    }
    let response = ask
        .send()
        .await
        .map_err(|error| format!("your proxy could not be reached: {}", error.without_url()))?;
    if !response.status().is_success() {
        let status = response.status();
        return Err(format!(
            "your proxy answered {status} when asked for its models"
        ));
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|_| "your proxy's list of models could not be read".to_string())?;
    Ok(allowed_ids(&body))
}

/// The ids an opencodex `/v1/models` body lists that a person's subscription may use, wherever
/// the list came from: this machine's proxy, or the one on their Mac (`relay`).
pub(crate) fn allowed_ids(body: &serde_json::Value) -> Vec<String> {
    body["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|model| model["id"].as_str())
        .filter(|id| subscription_model(id).is_ok())
        .map(str::to_string)
        .collect()
}

/// What saving a setting does to the proxy's key. No `Debug`: `Set` holds the key.
pub enum KeyChange {
    Keep,
    Set(String),
    Clear,
}

/// The setting a `PUT /account/inference-source` body asks for over `current`, and what it does
/// to the key, or the sentence it is refused with. A field absent keeps what is saved; `null` or
/// blank clears it, and so for `via` and `relay.localModel`. EVERY RULE IS ASKED HERE, BEFORE
/// ANYTHING IS WRITTEN: an address that is not this machine, or a model the terms forbid, beside
/// a fresh key saves neither. The key goes out in a header, so it is printable and bounded or
/// refused now, not at a turn.
pub fn apply(
    current: &InferenceSource,
    body: &serde_json::Value,
) -> Result<(InferenceSource, KeyChange), String> {
    let text =
        |object: &serde_json::Value, field: &str| -> Result<Option<Option<String>>, String> {
            match object.get(field) {
                None => Ok(None),
                Some(serde_json::Value::Null) => Ok(Some(None)),
                Some(serde_json::Value::String(text)) => Ok(Some(
                    Some(text.trim().to_string()).filter(|t| !t.is_empty()),
                )),
                Some(_) => Err(format!("{field} must be a string or null")),
            }
        };
    let model = |model: Option<String>, field: &str| match model.as_deref().map(subscription_model)
    {
        Some(Err(why)) => Err(format!("{field}: {why}")),
        _ => Ok(model),
    };
    let mut source = current.clone();
    source.kind = SourceKind::named(body.get("kind"))
        .ok()
        .flatten()
        .ok_or("kind must be \"gateway\" or \"local_proxy\"")?;
    if let Some(base) = text(body, "baseUrl")? {
        source.base_url = base
            .as_deref()
            .map(loopback_base)
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
        Some(serde_json::Value::Null) => source.relay_model = None,
        Some(relay @ serde_json::Value::Object(_)) => {
            let chosen = text(relay, "localModel").map_err(|why| format!("relay.{why}"))?;
            if let Some(chosen) = chosen {
                source.relay_model = model(chosen, "relay.localModel")?;
            }
        }
        Some(_) => return Err("relay must be an object or null".to_string()),
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
    Ok((source, key))
}

/// The reads the server makes for a person's source: their setting, their proxy's key, and the
/// relay their Mac holds. A trait so what decides where a turn asks lives here, beside what dials
/// it, and the server reads.
#[async_trait::async_trait]
pub trait Saved: Send + Sync {
    /// The account's setting; `None` when it cannot be read.
    async fn setting(&self, account: &AccountId) -> Option<InferenceSource>;
    /// The proxy's key when `saved` says there is one, or why it cannot be opened.
    async fn key(&self, account: &AccountId, saved: bool) -> Result<Option<String>, String>;
    /// The broker holding this process's Macs' streams.
    fn relay(&self) -> Arc<RelayBroker>;
}

/// Where a turn's calls go, as `route` resolves it. On a person's own subscription, the model it
/// asks for — the proxy's own id, never the coworker's gateway pin — and where that is, or why it
/// cannot be used: still `LocalProxy` then, so the run records what the person chose and the door
/// refuses in words.
#[derive(Debug, Default)]
pub enum Route {
    #[default]
    Gateway,
    LocalProxy {
        model: String,
        endpoint: ModelEndpoint,
    },
}

impl Route {
    /// What a run captures on `RunEvent::Started`, and a carry-on is resolved by.
    pub fn kind(&self) -> SourceKind {
        match self {
            Self::Gateway => SourceKind::Gateway,
            Self::LocalProxy { .. } => SourceKind::LocalProxy,
        }
    }

    /// The kind with the way it goes, as a run captures them.
    pub fn source(&self) -> TurnSource {
        match self {
            Self::Gateway => SourceKind::Gateway.into(),
            Self::LocalProxy { endpoint, .. } => TurnSource {
                kind: SourceKind::LocalProxy,
                via: endpoint.via(),
            },
        }
    }

    /// The model a turn's request asks for, and where: on the gateway, the coworker's `pin` and
    /// no endpoint, which is the gateway.
    pub fn asked(self, pin: String) -> (String, Option<ModelEndpoint>) {
        match self {
            Self::Gateway => (pin, None),
            Self::LocalProxy { model, endpoint } => (model, Some(endpoint)),
        }
    }
}

/// THE ONE PLACE A TURN'S SOURCE IS RESOLVED, for every path that asks a model for a person: a
/// fresh turn, whose `chosen` is its own pick (a drained queued send's, else its
/// `forwardedProps.inferenceSource`) over the account's setting; a carry-on after a card or a
/// restart, whose `chosen` is the kind and the way its run captured; and, through that turn's
/// request, its judge and its wrap-up. On the proxy the model is `captured` — the one the run
/// started on — else the setting's for that way. The address and key are the setting's as it
/// stands: they say where the proxy lives now, not what the turn chose.
///
/// THE RELAY IS A WAY, NOT A SOURCE: `via: "mac"` behind the same `kind: "local_proxy"`, resolved
/// here to `ModelEndpoint::Relay` for the account's Mac and `run_id`, whose `infer` frames name
/// it. No caller changed for it. A Mac is picked per call, so none being connected is the door's
/// `relay_offline`, in words, and never a turn sent the other way or to the gateway.
///
/// A SETTING THAT CANNOT BE READ REFUSES THE TURN, unless the turn named the gateway itself (a
/// carry-on of a run that started there does). Guessed as the gateway, it would be a silent fall
/// back for a person who chose their own subscription, billing a key they chose not to use. It
/// is refused as a proxy turn with a gap is: in words, on no model, with nothing asked anywhere,
/// no gateway key minted and no meter consulted.
pub async fn route(
    saved: &dyn Saved,
    account: Option<&AccountId>,
    chosen: Option<TurnSource>,
    captured: Option<&str>,
    run_id: &str,
) -> Route {
    let named_gateway = chosen.is_some_and(|chosen| chosen.kind == SourceKind::Gateway);
    let (Some(account), false) = (account, named_gateway) else {
        return Route::Gateway;
    };
    // Empty when neither names one, which the door refuses: a proxy turn never carries the
    // coworker's gateway pin, even to be refused — its frame and its run's start would both name
    // a model it never asked.
    let captured = captured.map(str::to_string);
    let Some(setting) = saved.setting(account).await else {
        let why = "Your reply source could not be read, so the turn was not sent; try again in a \
                   moment.";
        let via = chosen.and_then(|chosen| chosen.via);
        let endpoint = ModelEndpoint::Unavailable {
            why: why.to_string(),
            via,
        };
        let model = captured.unwrap_or_default();
        return Route::LocalProxy { model, endpoint };
    };
    let base = match setting.resolve(chosen) {
        (SourceKind::Gateway, _) => return Route::Gateway,
        (SourceKind::LocalProxy, Via::Loopback) => setting.base_url.as_deref(),
        (SourceKind::LocalProxy, Via::Mac) => {
            let model = captured.or(setting.relay_model).unwrap_or_default();
            let endpoint = if model.is_empty() {
                refused(
                    "Choose a model for your Mac first: your inference source names none for it, \
                     so the turn was not sent. Pick one your Mac's opencodex serves",
                    Via::Mac,
                )
            } else {
                ModelEndpoint::Relay(RelayTo {
                    broker: saved.relay(),
                    account: account.as_str().to_string(),
                    run_id: run_id.to_string(),
                })
            };
            return Route::LocalProxy { model, endpoint };
        }
    };
    let model = captured.or(setting.local_model).unwrap_or_default();
    let key = saved.key(account, setting.has_key).await;
    let endpoint = endpoint(base, &model, key);
    Route::LocalProxy { model, endpoint }
}

/// What `GET /models` says of the person's own subscription, whatever their setting's kind:
/// `localProxy` — whether their proxy answers `/healthz` and whether their Mac holds its relay —
/// and, when `listing`, the models each serves, in the gateway's entry shape plus `source` and
/// `via`. `None` when there is nothing to say: no address stored, no Mac connected, and the Mac
/// not the setting's way. A proxy that is down or a Mac that is away lists nothing, and says so
/// beside the gateway's list, never as an error over it.
pub async fn listed(
    saved: &dyn Saved,
    account: &AccountId,
    listing: bool,
) -> Option<(serde_json::Value, Vec<serde_json::Value>)> {
    let setting = saved.setting(account).await?;
    let relay = saved.relay();
    let mac = relay.connected(account.as_str());
    let by_mac = setting.via == Some(Via::Mac);
    if setting.base_url.is_none() && mac.is_none() && !by_mac {
        return None;
    }
    let up = match &setting.base_url {
        Some(base) => healthy(base).await,
        None => false,
    };
    let key = if up && listing {
        saved.key(account, setting.has_key).await.ok()
    } else {
        None
    };
    let loopback = match (key, &setting.base_url) {
        (Some(key), Some(base)) => models(base, key.as_deref()).await.unwrap_or_default(),
        _ => Vec::new(),
    };
    let from_mac = match (listing, &mac) {
        (true, Some(_)) => relay.models(account.as_str()).await,
        _ => Vec::new(),
    };
    let entry = |via: Via| {
        move |id| {
            serde_json::json!({ "id": id, "points": null, "source": "local_proxy",
                                      "via": via.as_str() })
        }
    };
    let mut entries: Vec<_> = loopback.into_iter().map(entry(Via::Loopback)).collect();
    entries.extend(from_mac.into_iter().map(entry(Via::Mac)));
    let said = serde_json::json!({ "healthy": up, "relayConnected": mac.is_some() });
    Some((said, entries))
}

/// The setting as `GET` and `PUT /account/inference-source` answer with it — the other half of
/// `apply`. Never the key, only whether there is one; `healthy` is a live `/healthz` on every read,
/// whatever the kind, and false with no address. `mac` is the account's connected Mac and its
/// enrolled label, for `relay`: an account with none reads `connected: false` and nulls, whole.
pub async fn described(
    source: &InferenceSource,
    mac: Option<(String, Option<String>)>,
) -> serde_json::Value {
    let healthy = match &source.base_url {
        Some(base) => healthy(base).await,
        None => false,
    };
    let (machine_id, machine_label) = mac.map_or((None, None), |(id, label)| (Some(id), label));
    serde_json::json!({
        "kind": source.kind.as_str(),
        "via": source.via.unwrap_or_default().as_str(),
        "baseUrl": source.base_url,
        "localModel": source.local_model,
        "healthy": healthy,
        "hasApiKey": source.has_key,
        "relay": {
            "connected": machine_id.is_some(),
            "machineId": machine_id,
            "machineLabel": machine_label,
            "localModel": source.relay_model,
        },
    })
}

/// A turn its setting cannot send the way it goes, refused in words at the door.
fn refused(why: &str, via: Via) -> ModelEndpoint {
    ModelEndpoint::Unavailable {
        why: format!("{why}, or switch this turn to the gateway."),
        via: Some(via),
    }
}

/// Where a turn on a person's own subscription goes by the loopback, from their setting: `base`
/// as saved, `model` the id it asks for, `key` the proxy's key as the vault gave it. Every gap is
/// a refusal the door says, never a fall back to the gateway. Built only by `route`.
fn endpoint(base: Option<&str>, model: &str, key: Result<Option<String>, String>) -> ModelEndpoint {
    let Some(base) = base.filter(|base| !base.is_empty()) else {
        return refused(
            "You chose your own subscription, but no proxy address is set; set one in your \
             inference source (like http://127.0.0.1:8080)",
            Via::Loopback,
        );
    };
    if model.is_empty() {
        return refused(
            "Choose a model for your own subscription first: your inference source names none, \
             so the turn was not sent. Pick one your proxy serves",
            Via::Loopback,
        );
    }
    match key {
        Ok(key) => ModelEndpoint::Proxy {
            base_url: base.to_string(),
            auth: key.map(|key| (KEY_HEADER.to_string(), key)),
        },
        Err(why) => refused(
            &format!(
                "Your proxy's key could not be opened ({why}), so the turn was not sent; save \
                 the key again"
            ),
            Via::Loopback,
        ),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "../tests/unit/local_proxy_tests.rs"]
mod tests;
