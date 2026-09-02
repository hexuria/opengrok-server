//! Per-coworker spend limits (`docs/plan-spend-policy.md`).
//!
//! The gateway keeps the ledger; the server evaluates the policy. A coworker hired by an org
//! member gets a gateway key of its OWN at hire (minted on the org's principal, sealed in the
//! vault like a connector credential), so the gateway can tell its spend from everybody
//! else's. The limits — a rolling five hours, a rolling seven days, the calendar month — are
//! authored here at three scopes (org default, member override, the coworker itself; the most
//! specific value per window wins) and evaluated by `GuardedDoor` before EACH model call from
//! the gateway's windowed usage read. At a limit the call is refused with a sentence that names
//! the window and when it frees up. The gateway's own per-key quota is left unset: one enforcer
//! per rule, or two answers disagree.
//!
//! Fail closed where it matters. A coworker with limits whose key cannot be produced, or whose
//! meter cannot be read (two-second timeout, a reading younger than sixty seconds stands in),
//! has its turn HELD with the reason — never run on the deployment's key, never run unmetered.
//! A coworker with no limits at any layer never touches the meter: with "unlimited" as the
//! shipped default, a gateway blip must not become an outage for people who never opted in.
//! Never a 4xx on hire: no admin connection, no vault or no org means the hire proceeds on the
//! deployment's key and the console says which.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_harness::{DeltaStream, GatewayKey, ModelDoor, ModelError, ModelRequest};
use opengrok_store::PgStore;
pub use opengrok_store::{SpendLimit, SpendScope};
use serde::Serialize;

use crate::agui::AgUiState;
use crate::gateway_admin::{GatewayAdmin, KeyUsage};

const SECRET_PREFIX: &str = "coworker-gateway-key:";

fn secret_id(coworker: &CoworkerId) -> String {
    format!("{SECRET_PREFIX}{}", coworker.as_str())
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// What minting at hire came to. Never an error: a hire is not refused over a cap.
#[derive(Debug)]
pub enum KeyOutcome {
    Minted { key_prefix: String },
    Unavailable(String),
}

/// The three preconditions, as one sentence when one is missing.
async fn availability(
    state: &AgUiState,
    account_id: &AccountId,
) -> Result<
    (
        crate::gateway_admin::GatewayAdmin,
        std::sync::Arc<opengrok_store::Vault>,
        String,
    ),
    String,
> {
    let Some(admin) = state.auth.gateway_admin.clone() else {
        return Err(
            "this deployment has no gateway admin connection (OG_GATEWAY_ADMIN_URL), so a \
             coworker cannot be given a key of its own"
                .to_string(),
        );
    };
    let Some(vault) = state.vault.clone() else {
        return Err(
            "this deployment has no vault (OG_VAULT_KEK) to keep a coworker's key in".to_string(),
        );
    };
    let org_id = match state.auth.store.load_account(account_id).await {
        Ok((account, _)) => account.org_id.filter(|org| !org.is_empty()),
        Err(error) => return Err(format!("the hirer's account could not be read: {error}")),
    };
    let Some(org_id) = org_id else {
        return Err(
            "the hirer is not in an org; a coworker's key is minted on the org's gateway principal"
                .to_string(),
        );
    };
    Ok((admin, vault, org_id))
}

/// Give a freshly hired coworker a key of its own, or say why not. Idempotent: a coworker that
/// already has one keeps it.
pub async fn ensure_key_for(
    state: &AgUiState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    name: &str,
) -> KeyOutcome {
    let store = &state.auth.store;
    match store.coworker_key(coworker_id).await {
        Ok(Some(existing)) => {
            return KeyOutcome::Minted {
                key_prefix: existing.key_prefix,
            };
        }
        Ok(None) => {}
        Err(error) => {
            return KeyOutcome::Unavailable(format!("the key row could not be read: {error}"));
        }
    }
    let (admin, vault, org_id) = match availability(state, account_id).await {
        Ok(ready) => ready,
        Err(reason) => {
            tracing::info!(coworker = %coworker_id.as_str(), reason, "spend cap: no key of its own");
            return KeyOutcome::Unavailable(reason);
        }
    };
    let label = format!("coworker: {name}");
    let minted = match admin.mint_member_key(&org_id, &label, None).await {
        Ok(minted) => minted,
        Err(error) => {
            tracing::error!(%error, coworker = %coworker_id.as_str(), "spend cap: the gateway would not mint");
            return KeyOutcome::Unavailable(format!("the gateway would not mint a key: {error}"));
        }
    };
    let at_ms = now_ms();
    let id = secret_id(coworker_id);
    let stored = match vault.seal(&id, &minted.key) {
        Ok(sealed) => store.put_secret(&id, &sealed, at_ms).await,
        Err(error) => Err(error),
    };
    if let Err(error) = stored {
        // A key we cannot keep is a key nobody can use: revoke it rather than leave a live
        // credential the gateway knows and we have lost.
        tracing::error!(%error, coworker = %coworker_id.as_str(), "spend cap: the key could not be sealed; revoking it");
        if let Err(revoke) = admin.revoke_key(&minted.id).await {
            tracing::error!(%revoke, key = %minted.id, "spend cap: and it could not be revoked either");
        }
        return KeyOutcome::Unavailable(format!("the key could not be kept: {error}"));
    }
    let view = opengrok_store::CoworkerKeyView {
        coworker_id: coworker_id.as_str().to_string(),
        account_id: account_id.as_str().to_string(),
        key_id: minted.id.clone(),
        key_prefix: minted.key_prefix.clone(),
        quota_usd: None,
        created_at_ms: at_ms,
    };
    if let Err(error) = store.insert_coworker_key(&view).await {
        tracing::error!(%error, coworker = %coworker_id.as_str(), "spend cap: the key row could not be written; revoking the key");
        let _ = store.delete_secret(&id).await;
        if let Err(revoke) = admin.revoke_key(&minted.id).await {
            tracing::error!(%revoke, key = %minted.id, "spend cap: and it could not be revoked either");
        }
        return KeyOutcome::Unavailable(format!("the key could not be recorded: {error}"));
    }
    // The org's key listing attributes it too, so the console's keys card shows "coworker: Ada"
    // rather than an unattributed key the gateway has and we do not. Best effort: the coworker's
    // own row above is the one the run path reads.
    if let Err(error) = store
        .insert_gateway_key(&opengrok_store::NewGatewayKey {
            key_id: &minted.id,
            org_id: &org_id,
            member_account_id: account_id.as_str(),
            key_prefix: &minted.key_prefix,
            label: &label,
            mint_nonce: None,
            at_ms,
        })
        .await
    {
        tracing::warn!(%error, key = %minted.id, "spend cap: the org key listing will show this key unattributed");
    }
    tracing::info!(coworker = %coworker_id.as_str(), key_prefix = %minted.key_prefix, "spend cap: minted the coworker's own key");
    KeyOutcome::Minted {
        key_prefix: minted.key_prefix,
    }
}

/// The credential this coworker's next request goes out with. `None` ⇒ the deployment's key
/// (no key of its own). A row whose secret cannot be opened is `Unavailable`, which the door
/// refuses — fail closed, and say why.
pub async fn key_for(state: &AgUiState, coworker_id: &CoworkerId) -> Option<GatewayKey> {
    let row = match state.auth.store.coworker_key(coworker_id).await {
        Ok(Some(row)) => row,
        Ok(None) => return None,
        Err(error) => {
            tracing::error!(%error, coworker = %coworker_id.as_str(), "spend cap: the key row could not be read");
            return Some(GatewayKey::unavailable(format!(
                "its key row could not be read ({error})"
            )));
        }
    };
    let Some(vault) = state.vault.as_ref() else {
        return Some(GatewayKey::unavailable(
            "this deployment has no vault to open it with (OG_VAULT_KEK)",
        ));
    };
    match state
        .auth
        .store
        .open_credential(vault, &secret_id(coworker_id))
        .await
    {
        Ok(Some(key)) => Some(GatewayKey::new(key)),
        Ok(None) => Some(GatewayKey::unavailable(format!(
            "the sealed key {} is missing",
            row.key_prefix
        ))),
        Err(error) => {
            tracing::error!(%error, coworker = %coworker_id.as_str(), "spend cap: the sealed key could not be opened");
            Some(GatewayKey::unavailable(
                "the sealed key could not be opened".to_string(),
            ))
        }
    }
}

/// `key_for` for the paths that may not have a coworker (a plain AG-UI run).
pub async fn key_for_opt(
    state: &AgUiState,
    coworker_id: Option<&CoworkerId>,
) -> Option<GatewayKey> {
    match coworker_id {
        Some(coworker_id) => key_for(state, coworker_id).await,
        None => None,
    }
}

/// Money as the gateway writes it ("12.345678", up to six decimals, no sign) in micro-dollars,
/// so two amounts compare exactly. `None` for anything else — a limit that does not parse is
/// refused at the door it came in through, never stored.
pub fn micros(text: &str) -> Option<i64> {
    let text = text.trim();
    if text.is_empty() || text.len() > 20 {
        return None;
    }
    let (whole, frac) = text.split_once('.').unwrap_or((text, ""));
    if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if frac.len() > 6 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let whole: i64 = whole.parse().ok()?;
    let frac: i64 = if frac.is_empty() {
        0
    } else {
        format!("{frac:0<6}").parse().ok()?
    };
    whole.checked_mul(1_000_000)?.checked_add(frac)
}

/// Micro-dollars as prose: "$4.90".
fn dollars(micros: i64) -> String {
    format!(
        "{}.{:02}",
        micros / 1_000_000,
        (micros % 1_000_000) / 10_000
    )
}

/// Every amount in a limit parses, or the reason it does not.
pub fn validate_limit(limit: &SpendLimit) -> Result<(), String> {
    for (label, value) in [
        ("fiveHourUsd", &limit.five_hour_usd),
        ("sevenDayUsd", &limit.seven_day_usd),
        ("monthUsd", &limit.month_usd),
    ] {
        if let Some(value) = value
            && micros(value).is_none()
        {
            return Err(format!(
                "{label}: '{value}' is not an amount — digits with up to six decimals, like 5.00"
            ));
        }
    }
    Ok(())
}

/// The limits this coworker is under: its own row, then its hirer's, then the org's default,
/// per window — the most specific admin-written value wins. `Err` is a store failure.
pub async fn effective_limits(
    store: &PgStore,
    coworker: &CoworkerId,
) -> Result<SpendLimit, String> {
    let own = store
        .spend_limit(SpendScope::Coworker, coworker.as_str())
        .await
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    let Some(owner) = store
        .coworker_owner(coworker)
        .await
        .map_err(|error| error.to_string())?
    else {
        return Ok(own);
    };
    let member = store
        .spend_limit(SpendScope::Member, owner.as_str())
        .await
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    let org = match store.load_account(&owner).await {
        Ok((account, _)) => match account.org_id.filter(|org| !org.is_empty()) {
            Some(org_id) => store
                .spend_limit(SpendScope::Org, &org_id)
                .await
                .map_err(|error| error.to_string())?
                .unwrap_or_default(),
            None => SpendLimit::default(),
        },
        Err(error) => return Err(error.to_string()),
    };
    Ok(own.over(&member).over(&org))
}

/// One window as the console shows it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowMeter {
    /// `5h`, `7d` or `month`.
    pub window: &'static str,
    pub used_usd: Option<String>,
    pub limit_usd: Option<String>,
    /// RFC 3339: when the rolling window next frees up (its oldest spend ageing out), or when
    /// the month resets. Absent when the window is empty.
    pub frees_at: Option<String>,
}

/// What the console shows next to a coworker: whether it is metered at all, why not when it
/// is not, and the three meters.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CoworkerSpend {
    pub metered: bool,
    pub note: Option<String>,
    pub key_prefix: Option<String>,
    pub limits: SpendLimit,
    pub windows: Vec<WindowMeter>,
}

fn windows_of(usage: Option<&KeyUsage>, limits: &SpendLimit) -> Vec<WindowMeter> {
    vec![
        WindowMeter {
            window: "5h",
            used_usd: usage.and_then(|u| u.five_hour_usd.clone()),
            limit_usd: limits.five_hour_usd.clone(),
            frees_at: usage.and_then(|u| u.five_hour_frees_at.clone()),
        },
        WindowMeter {
            window: "7d",
            used_usd: usage.and_then(|u| u.seven_day_usd.clone()),
            limit_usd: limits.seven_day_usd.clone(),
            frees_at: usage.and_then(|u| u.seven_day_frees_at.clone()),
        },
        WindowMeter {
            window: "month",
            used_usd: usage.map(|u| u.month_to_date_usd.clone()),
            limit_usd: limits.month_usd.clone(),
            frees_at: usage.and_then(|u| u.month_resets_at.clone()),
        },
    ]
}

/// What the console shows. The coworker must be this account's (the caller checks ownership).
pub async fn spend_for(
    state: &AgUiState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
) -> Result<CoworkerSpend, String> {
    let store = &state.auth.store;
    let limits = effective_limits(store, coworker_id).await?;
    let row = store
        .coworker_key(coworker_id)
        .await
        .map_err(|error| format!("the key row could not be read: {error}"))?
        .filter(|row| row.account_id == account_id.as_str());
    let Some(row) = row else {
        let note = match availability(state, account_id).await {
            Ok(_) => "this coworker has no key of its own yet, so it is not metered".to_string(),
            Err(reason) => reason,
        };
        return Ok(CoworkerSpend {
            metered: false,
            note: Some(note),
            key_prefix: None,
            limits: limits.clone(),
            windows: windows_of(None, &limits),
        });
    };
    let (usage, note) = match state.auth.gateway_admin.as_ref() {
        None => (
            None,
            Some("no gateway admin connection; spend cannot be read".to_string()),
        ),
        Some(admin) => match admin.key_usage(&row.key_id).await {
            Ok(Some(usage)) => (Some(usage), None),
            Ok(None) => (
                None,
                Some("the gateway no longer knows this key".to_string()),
            ),
            Err(error) => (
                None,
                Some(format!("the gateway could not be asked: {error}")),
            ),
        },
    };
    Ok(CoworkerSpend {
        metered: true,
        note,
        key_prefix: Some(row.key_prefix),
        windows: windows_of(usage.as_ref(), &limits),
        limits,
    })
}

/// A reading of the meter, and when it was taken.
#[derive(Debug, Clone)]
struct Reading {
    usage: KeyUsage,
    at_ms: i64,
}

/// A reading this young is reused as is — a tool loop pays for one read, not ten.
const FRESH_MS: i64 = 15_000;
/// Under a failed read, a reading this young stands in; older than this the turn is held.
const STALE_OK_MS: i64 = 60_000;
/// How long a model call waits for the meter.
const METER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// The door every model call goes through: evaluates the coworker's spend limits from the
/// gateway's windowed usage before handing the request to the real door. A request with no
/// `spend_scope`, or whose coworker has no limits at any layer, passes straight through and
/// never touches the meter.
pub struct GuardedDoor {
    inner: Arc<dyn ModelDoor>,
    store: PgStore,
    admin: Option<GatewayAdmin>,
    cache: Mutex<HashMap<String, Reading>>,
    /// The resolved limits per coworker, for `fresh_ms`: resolving them is four or five reads,
    /// and a tool loop of twenty calls would otherwise pay a hundred to learn "no limits"
    /// twenty times. A limit written on the admin page shows up within the freshness window.
    limits_cache: Mutex<HashMap<String, (SpendLimit, i64)>>,
    /// How long a reading is reused without asking the meter again. `FRESH_MS` in production;
    /// a test that counts reads sets it to zero.
    fresh_ms: i64,
}

impl std::fmt::Debug for GuardedDoor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuardedDoor")
            .field("admin", &self.admin.is_some())
            .finish_non_exhaustive()
    }
}

impl GuardedDoor {
    pub fn new(inner: Arc<dyn ModelDoor>, store: PgStore, admin: Option<GatewayAdmin>) -> Self {
        Self {
            inner,
            store,
            admin,
            cache: Mutex::new(HashMap::new()),
            limits_cache: Mutex::new(HashMap::new()),
            fresh_ms: FRESH_MS,
        }
    }

    async fn limits_for(&self, coworker: &CoworkerId) -> Result<SpendLimit, String> {
        if self.fresh_ms > 0
            && let Ok(cache) = self.limits_cache.lock()
            && let Some((limits, at_ms)) = cache.get(coworker.as_str())
            && now_ms() - *at_ms <= self.fresh_ms
        {
            return Ok(limits.clone());
        }
        let limits = effective_limits(&self.store, coworker).await?;
        if let Ok(mut cache) = self.limits_cache.lock() {
            cache.insert(coworker.as_str().to_string(), (limits.clone(), now_ms()));
        }
        Ok(limits)
    }

    /// Reuse a reading for this long. Zero ⇒ every call reads the meter — for tests that assert
    /// on the meter itself; production keeps the default so a tool loop pays for one read.
    #[must_use]
    pub fn with_fresh_ms(mut self, fresh_ms: i64) -> Self {
        self.fresh_ms = fresh_ms;
        self
    }

    fn cached(&self, key_id: &str, within_ms: i64) -> Option<KeyUsage> {
        let cache = self.cache.lock().ok()?;
        let reading = cache.get(key_id)?;
        (now_ms() - reading.at_ms <= within_ms).then(|| reading.usage.clone())
    }

    /// The meter, read with the turn's patience. Fresh cache first; then the gateway with a
    /// two-second wait; then, if that fails, a reading under a minute old; then the turn is held.
    async fn reading(&self, key_id: &str, name: &str) -> Result<KeyUsage, ModelError> {
        if self.fresh_ms > 0
            && let Some(usage) = self.cached(key_id, self.fresh_ms)
        {
            return Ok(usage);
        }
        let Some(admin) = self.admin.as_ref() else {
            return Err(ModelError::SpendCap(format!(
                "{name} has spend limits, but this deployment has no gateway admin connection to \
                 read its meter with (OG_GATEWAY_ADMIN_URL). Its turns are held until it does."
            )));
        };
        match admin.key_usage_within(key_id, METER_TIMEOUT).await {
            Ok(Some(usage)) => {
                if let Ok(mut cache) = self.cache.lock() {
                    cache.insert(
                        key_id.to_string(),
                        Reading {
                            usage: usage.clone(),
                            at_ms: now_ms(),
                        },
                    );
                }
                Ok(usage)
            }
            Ok(None) => Err(ModelError::SpendCap(format!(
                "{name} has spend limits, but the gateway no longer knows its key; its turns \
                 are held. Retire and re-hire it, or ask an admin."
            ))),
            Err(error) => {
                tracing::error!(%error, key_id, "spend guard: the meter could not be read");
                match self.cached(key_id, STALE_OK_MS) {
                    Some(usage) => Ok(usage),
                    None => Err(ModelError::SpendCap(format!(
                        "{name}'s spend meter could not be read ({error}); the turn is held. \
                         Try again in a moment."
                    ))),
                }
            }
        }
    }
}

/// One window's verdict.
struct Verdict {
    label: &'static str,
    used: i64,
    limit: i64,
    reset: String,
}

fn rfc3339_at(text: Option<&str>) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(text?)
        .ok()
        .map(|t| t.with_timezone(&chrono::Utc))
}

/// The sentence a person reads when a window is used up, or `None` when every window has
/// room. Names the first window in the way and when it frees up, says which others still have
/// room, and names any other window that is also used up.
fn over_limit(name: &str, usage: &KeyUsage, limits: &SpendLimit) -> Option<String> {
    let windows = [
        (
            "5-hour",
            usage.five_hour_usd.as_deref(),
            limits.five_hour_usd.as_deref(),
            rfc3339_at(usage.five_hour_frees_at.as_deref())
                .map(|t| format!("it begins to free up at {}", t.format("%H:%M UTC"))),
        ),
        (
            "7-day",
            usage.seven_day_usd.as_deref(),
            limits.seven_day_usd.as_deref(),
            rfc3339_at(usage.seven_day_frees_at.as_deref()).map(|t| {
                format!(
                    "it begins to free up at {}",
                    t.format("%H:%M UTC on %-d %b")
                )
            }),
        ),
        (
            "monthly",
            Some(usage.month_to_date_usd.as_str()),
            limits.month_usd.as_deref(),
            rfc3339_at(usage.month_resets_at.as_deref())
                .map(|t| format!("it resets on {}", t.format("%-d %b"))),
        ),
    ];
    let mut over: Vec<Verdict> = Vec::new();
    let mut room: Vec<&'static str> = Vec::new();
    for (label, used, limit, reset) in windows {
        let Some(limit) = limit.and_then(micros) else {
            continue;
        };
        let used = used.and_then(micros).unwrap_or(0);
        if used >= limit {
            over.push(Verdict {
                label,
                used,
                limit,
                reset: reset.unwrap_or_else(|| "it frees up as older spend ages out".to_string()),
            });
        } else {
            room.push(label);
        }
    }
    let first = over.first()?;
    let mut sentence = format!(
        "{name} has used its {} allowance ({} of {}); {}.",
        first.label,
        dollars(first.used),
        dollars(first.limit),
        first.reset
    );
    for also in over.iter().skip(1) {
        sentence.push_str(&format!(
            " Its {} allowance is also used up ({} of {}); {}.",
            also.label,
            dollars(also.used),
            dollars(also.limit),
            also.reset
        ));
    }
    if !room.is_empty() {
        sentence.push_str(&format!(
            " The {} {} still {} room.",
            room.join(" and "),
            if room.len() == 1 {
                "allowance"
            } else {
                "allowances"
            },
            if room.len() == 1 { "has" } else { "have" }
        ));
    }
    Some(sentence)
}

#[async_trait::async_trait]
impl ModelDoor for GuardedDoor {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
        let Some(scope) = request.spend_scope.clone() else {
            return self.inner.stream(request).await;
        };
        let coworker = CoworkerId::from_stored(scope);
        let limits = self.limits_for(&coworker).await.map_err(|error| {
                tracing::error!(%error, coworker = %coworker.as_str(), "spend guard: limits could not be read");
                ModelError::SpendCap(format!(
                    "This coworker's spend limits could not be read ({error}); the turn is held."
                ))
            })?;
        if limits.is_empty() {
            return self.inner.stream(request).await;
        }
        let name = self
            .store
            .load_coworker(&coworker)
            .await
            .map(|(coworker, _)| coworker.name)
            .unwrap_or_else(|_| "This coworker".to_string());
        // Limits without a key of its own cannot be metered, so they cannot be honoured: held,
        // and the sentence says what would make it meterable.
        let key = match self.store.coworker_key(&coworker).await {
            Ok(Some(key)) => key,
            Ok(None) => {
                return Err(ModelError::SpendCap(format!(
                    "{name} has spend limits but no gateway key of its own to meter them on, so \
                     its turns are held. A coworker is metered when its hirer is in an org and the \
                     deployment has a gateway admin connection and a vault; ask an admin."
                )));
            }
            Err(error) => {
                return Err(ModelError::SpendCap(format!(
                    "{name}'s key could not be read ({error}); the turn is held."
                )));
            }
        };
        let usage = self.reading(&key.key_id, &name).await?;
        if let Some(sentence) = over_limit(&name, &usage, &limits) {
            tracing::info!(coworker = %coworker.as_str(), "spend guard: refused — {sentence}");
            return Err(ModelError::SpendCap(sentence));
        }
        self.inner.stream(request).await
    }
}

/// Retirement: the key is revoked on the gateway, the row and the sealed secret dropped, the
/// org listing marked. Best effort at each step, each logged — a retired coworker with a key
/// still live on the gateway is the failure that matters, so the revoke goes first.
pub async fn revoke_for(state: &AgUiState, coworker_id: &CoworkerId) {
    let store = &state.auth.store;
    let row = match store.delete_coworker_key(coworker_id).await {
        Ok(Some(row)) => row,
        Ok(None) => return,
        Err(error) => {
            tracing::error!(%error, coworker = %coworker_id.as_str(), "spend cap: the key row could not be dropped at retirement");
            return;
        }
    };
    if let Some(admin) = state.auth.gateway_admin.as_ref() {
        if let Err(error) = admin.revoke_key(&row.key_id).await {
            tracing::error!(%error, key = %row.key_id, "spend cap: a retired coworker's key could not be revoked on the gateway");
        }
    } else {
        tracing::error!(key = %row.key_id, "spend cap: no gateway admin connection; a retired coworker's key is still live on the gateway");
    }
    if let Err(error) = store.delete_secret(&secret_id(coworker_id)).await {
        tracing::error!(%error, coworker = %coworker_id.as_str(), "spend cap: the sealed key could not be dropped");
    }
    if let Ok((account, _)) = store
        .load_account(&AccountId::from_stored(row.account_id.clone()))
        .await
        && let Some(org_id) = account.org_id.filter(|org| !org.is_empty())
        && let Err(error) = store.mark_gateway_key_revoked(&row.key_id, &org_id).await
    {
        tracing::warn!(%error, key = %row.key_id, "spend cap: the org key listing still shows the key live");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn money_parses_exactly_and_refuses_what_is_not_money() {
        assert_eq!(micros("5"), Some(5_000_000));
        assert_eq!(micros("5.00"), Some(5_000_000));
        assert_eq!(micros("0.000001"), Some(1));
        assert_eq!(micros("12.345678"), Some(12_345_678));
        assert_eq!(micros(" 1.5 "), Some(1_500_000));
        assert_eq!(
            micros("1."),
            Some(1_000_000),
            "a trailing point is still money"
        );
        for bad in ["", "-1", "1.2345678", "lots", "1e3", ".5", "$5", "1,5"] {
            assert_eq!(micros(bad), None, "{bad}");
        }
        assert_eq!(dollars(4_900_000), "4.90");
        assert_eq!(dollars(5_000_000), "5.00");
        assert_eq!(dollars(123_456), "0.12");
    }

    #[test]
    fn the_most_specific_window_wins_and_absent_means_nothing() {
        let org = SpendLimit {
            five_hour_usd: Some("1.00".into()),
            seven_day_usd: Some("5.00".into()),
            month_usd: Some("20.00".into()),
        };
        let member = SpendLimit {
            seven_day_usd: Some("9.00".into()),
            ..Default::default()
        };
        let own = SpendLimit {
            month_usd: Some("50.00".into()),
            ..Default::default()
        };
        let effective = own.over(&member).over(&org);
        assert_eq!(effective.five_hour_usd.as_deref(), Some("1.00"));
        assert_eq!(effective.seven_day_usd.as_deref(), Some("9.00"));
        assert_eq!(effective.month_usd.as_deref(), Some("50.00"));
        assert!(SpendLimit::default().is_empty());
    }

    fn usage(five: &str, seven: &str, month: &str) -> KeyUsage {
        KeyUsage {
            quota_usd: None,
            spent_usd: "0".into(),
            month_to_date_usd: month.into(),
            month_resets_at: Some("2026-10-01T00:00:00Z".into()),
            requests: 3,
            five_hour_usd: Some(five.into()),
            five_hour_frees_at: Some("2026-09-02T14:32:10Z".into()),
            seven_day_usd: Some(seven.into()),
            seven_day_frees_at: Some("2026-09-09T05:12:44Z".into()),
        }
    }

    #[test]
    fn the_sentence_names_the_window_in_the_way_and_when_it_frees_up() {
        let limits = SpendLimit {
            five_hour_usd: Some("5.00".into()),
            seven_day_usd: Some("20.00".into()),
            month_usd: Some("50.00".into()),
        };
        assert_eq!(over_limit("Ada", &usage("4.90", "10", "10"), &limits), None);
        let s = over_limit("Ada", &usage("5.000000", "10", "10"), &limits).unwrap();
        assert_eq!(
            s,
            "Ada has used its 5-hour allowance (5.00 of 5.00); it begins to free up at 14:32 UTC. \
             The 7-day and monthly allowances still have room."
        );
        let s = over_limit("Ada", &usage("1", "21", "50"), &limits).unwrap();
        assert!(s.starts_with("Ada has used its 7-day allowance (21.00 of 20.00); it begins to free up at 05:12 UTC on 9 Sep."), "{s}");
        assert!(
            s.contains(
                "Its monthly allowance is also used up (50.00 of 50.00); it resets on 1 Oct."
            ),
            "{s}"
        );
        assert!(s.ends_with("The 5-hour allowance still has room."), "{s}");
        // A limit at one window only: the others are not mentioned as having room.
        let only_month = SpendLimit {
            month_usd: Some("50.00".into()),
            ..Default::default()
        };
        let s = over_limit("Ada", &usage("9", "9", "50"), &only_month).unwrap();
        assert_eq!(
            s,
            "Ada has used its monthly allowance (50.00 of 50.00); it resets on 1 Oct."
        );
    }
}
