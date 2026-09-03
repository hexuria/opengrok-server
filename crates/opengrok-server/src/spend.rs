//! Per-coworker keys, meters, and the door that enforces POINTS limits before each model call
//! (`docs/plan-spend-policy.md`; the limits themselves live in `points.rs`).
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
use std::sync::{Arc, Mutex, OnceLock};

use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_harness::{DeltaStream, GatewayKey, ModelDoor, ModelError, ModelRequest};
use opengrok_store::PgStore;
pub use opengrok_store::{SpendLimit, SpendScope};
use serde::Serialize;

use crate::agui::AgUiState;
use crate::gateway_admin::{GatewayAdmin, KeyUsage};

const SECRET_PREFIX: &str = "coworker-gateway-key:";

/// Where a key minted for this PERSON on this coworker is sealed. The vault binds the id into
/// the ciphertext as AAD, so this string is part of the credential: changing its shape for an
/// existing row does not move the secret, it loses it.
fn secret_id(coworker: &CoworkerId, account: &AccountId) -> String {
    format!("{SECRET_PREFIX}{}:{}", coworker.as_str(), account.as_str())
}

/// Where a key minted before a coworker could have more than one is sealed. Read only when the
/// row says so (`secret_scoped == false`) — never as a fallback for a missing scoped secret,
/// which would hand one member the owner's credential.
fn legacy_secret_id(coworker: &CoworkerId) -> String {
    format!("{SECRET_PREFIX}{}", coworker.as_str())
}

/// The id this row's secret actually lives at.
fn secret_id_of(row: &opengrok_store::CoworkerKeyView, coworker: &CoworkerId) -> String {
    if row.secret_scoped {
        format!("{SECRET_PREFIX}{}:{}", coworker.as_str(), row.account_id)
    } else {
        legacy_secret_id(coworker)
    }
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
    match store.coworker_key(coworker_id, account_id).await {
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
    // Bind the org to its principal first — idempotent, and the same step the member-key mint
    // takes. Skipping it worked only on a gateway that had already seen the org: on the dev
    // gateway on 2 Sep 2026 (a fresh gateway, the org created while the admin connection was
    // wrong) every coworker mint was refused "no principal with that email", at hire and late.
    if let Err(error) = admin.ensure_org_principal(&org_id, None).await {
        tracing::error!(%error, coworker = %coworker_id.as_str(), "spend cap: the gateway would not bind the org's principal");
        return KeyOutcome::Unavailable(format!(
            "the gateway would not bind the org's principal: {error}"
        ));
    }
    // Both identities, in a fixed order, because a shared coworker has one key per member and
    // the gateway's key listing is flat with no grouping — a label naming only the coworker
    // would show the operator several identical rows.
    let label = format!("coworker: {name} — member: {}", account_id.as_str());
    let minted = match admin.mint_member_key(&org_id, &label, None).await {
        Ok(minted) => minted,
        Err(error) => {
            tracing::error!(%error, coworker = %coworker_id.as_str(), "spend cap: the gateway would not mint");
            return KeyOutcome::Unavailable(format!("the gateway would not mint a key: {error}"));
        }
    };
    let at_ms = now_ms();
    let id = secret_id(coworker_id, account_id);
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
        revoked_at_ms: None,
        secret_scoped: true,
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

/// How long the run path waits after a failed late mint before asking the gateway again for
/// that coworker: a gateway that refuses (a wrong admin key, a principal gone) is asked once an
/// interval, not once a turn. Per process, like every cache in this module.
const MINT_RETRY_MS: i64 = 10 * 60 * 1_000;
static MINT_ATTEMPTS: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();

/// Every per-coworker cache in this module keys on the PAIR, because a shared coworker's answer
/// differs per member: whose pool, whose key, whose limits. Keying on the coworker alone would
/// hand one member another member's answer for a freshness window.
fn pair_key(coworker: &CoworkerId, account: &AccountId) -> String {
    format!("{}:{}", coworker.as_str(), account.as_str())
}

/// A coworker without a key of its own gets one on its next turn. Minting is a hire-time step,
/// and a hire while the admin connection was wrong left the coworker unmetered for good —
/// nothing ever tried again. That was the dev server on 2 Sep 2026: OG_GATEWAY_ADMIN_TOKEN was
/// an inference key, every mint answered 403, and every coworker hired that day ran on the
/// deployment's key with no meter, cap or not. `true` when a key now exists.
async fn mint_late(state: &AgUiState, coworker_id: &CoworkerId, account_id: &AccountId) -> bool {
    let now = now_ms();
    let attempt = pair_key(coworker_id, account_id);
    {
        let Ok(mut attempts) = MINT_ATTEMPTS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
        else {
            return false;
        };
        if let Some(last) = attempts.get(&attempt)
            && now - last < MINT_RETRY_MS
        {
            return false;
        }
        attempts.insert(attempt, now);
    }
    let Ok((coworker, _)) = state.auth.store.load_coworker(coworker_id).await else {
        return false;
    };
    if coworker.retired || coworker.is_group() {
        return false;
    }
    match ensure_key_for(state, account_id, coworker_id, &coworker.name).await {
        KeyOutcome::Minted { key_prefix } => {
            tracing::info!(coworker = %coworker_id.as_str(), %key_prefix, "spend cap: minted the coworker's own key late, on its turn");
            true
        }
        KeyOutcome::Unavailable(reason) => {
            tracing::info!(coworker = %coworker_id.as_str(), reason, "spend cap: still no key of its own; the deployment's key carries this turn");
            false
        }
    }
}

/// The credential this coworker's next request goes out with. `None` ⇒ the deployment's key
/// (no key of its own). A row whose secret cannot be opened is `Unavailable`, which the door
/// refuses — fail closed, and say why.
pub async fn key_for(
    state: &AgUiState,
    coworker_id: &CoworkerId,
    actor: &AccountId,
) -> Option<GatewayKey> {
    let row = match state.auth.store.coworker_key(coworker_id, actor).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            if !mint_late(state, coworker_id, actor).await {
                return None;
            }
            match state.auth.store.coworker_key(coworker_id, actor).await {
                Ok(Some(row)) => row,
                _ => return None,
            }
        }
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
        .open_credential(vault, &secret_id_of(&row, coworker_id))
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
    actor: Option<&AccountId>,
) -> Option<GatewayKey> {
    match (coworker_id, actor) {
        (Some(coworker_id), Some(actor)) => key_for(state, coworker_id, actor).await,
        // No coworker, or nobody named: the deployment's key. An anonymous AG-UI run has no
        // person to bill, and inventing one would put a stranger's turn on somebody's pool.
        _ => None,
    }
}

/// Whose spend a turn is. The person talking when there is one; the coworker's owner when there
/// is not — a coworker acting on its own schedule is acting for whoever hired it, and that is an
/// answer rather than a default. `None` only when the coworker has no owner to fall back to.
pub async fn actor_for(
    state: &AgUiState,
    coworker_id: &CoworkerId,
    acting: Option<&AccountId>,
) -> Option<AccountId> {
    match acting {
        Some(account) => Some(account.clone()),
        None => state
            .auth
            .store
            .coworker_owner(coworker_id)
            .await
            .ok()
            .flatten(),
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
    /// Requests inside the window. Absent when the coworker is not metered or the gateway is
    /// older than open-ai-gateway #51.
    pub requests: Option<i64>,
    /// What the window's tokens would have cost at the model's list API price — a subscription
    /// seat's "12 requests · would have cost $0.41 on API". Absent as `requests` is.
    pub counterfactual_usd: Option<String>,
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
    /// How the coworker's usage is paid for, read off the month: `"subscription"` when it
    /// carries a bill it displaced and no cost of its own, `"api"` when it carries cost. Absent
    /// when not metered, when nothing has run this month, or on an older gateway — the block
    /// then infers, or says nothing.
    pub seat: Option<&'static str>,
}

fn windows_of(usage: Option<&KeyUsage>, limits: &SpendLimit) -> Vec<WindowMeter> {
    vec![
        WindowMeter {
            window: "5h",
            used_usd: usage.and_then(|u| u.five_hour_usd.clone()),
            limit_usd: limits.five_hour_usd.clone(),
            frees_at: usage.and_then(|u| u.five_hour_frees_at.clone()),
            requests: usage.and_then(|u| u.five_hour_requests),
            counterfactual_usd: usage.and_then(|u| u.five_hour_counterfactual_usd.clone()),
        },
        WindowMeter {
            window: "7d",
            used_usd: usage.and_then(|u| u.seven_day_usd.clone()),
            limit_usd: limits.seven_day_usd.clone(),
            frees_at: usage.and_then(|u| u.seven_day_frees_at.clone()),
            requests: usage.and_then(|u| u.seven_day_requests),
            counterfactual_usd: usage.and_then(|u| u.seven_day_counterfactual_usd.clone()),
        },
        WindowMeter {
            window: "month",
            used_usd: usage.map(|u| u.month_to_date_usd.clone()),
            limit_usd: limits.month_usd.clone(),
            frees_at: usage.and_then(|u| u.month_resets_at.clone()),
            requests: usage.map(|u| u.requests),
            counterfactual_usd: usage.and_then(|u| u.month_counterfactual_usd.clone()),
        },
    ]
}

/// `"api"` when this month carries cost, `"subscription"` when it carries only the bill a seat
/// displaced, nothing when it carries neither or the gateway does not say.
fn seat_of(usage: Option<&KeyUsage>) -> Option<&'static str> {
    let usage = usage?;
    let displaced = micros(usage.month_counterfactual_usd.as_deref()?)?;
    let cost = micros(&usage.month_to_date_usd)?;
    if cost > 0 {
        Some("api")
    } else if displaced > 0 {
        Some("subscription")
    } else {
        None
    }
}

/// What the console shows. The coworker must be this account's (the caller checks ownership).
pub async fn spend_for(
    state: &AgUiState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
) -> Result<CoworkerSpend, String> {
    let store = &state.auth.store;
    // The USD limits are retired (`points.rs` holds the limits now); the shape keeps its
    // `limits` field, empty, until the desktop's usage modal has replaced this read.
    let limits = SpendLimit::default();
    // The caller's own key on this coworker: a shared coworker's console row is about the
    // person reading it, and the pair is what the row is keyed by.
    let row = store
        .coworker_key(coworker_id, account_id)
        .await
        .map_err(|error| format!("the key row could not be read: {error}"))?
        .filter(|row| row.revoked_at_ms.is_none());
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
            seat: None,
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
        seat: seat_of(usage.as_ref()),
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
    limits_cache: Mutex<HashMap<String, (crate::points::Effective, i64)>>,
    /// The pool reading per OWNER: every coworker of one member shares one batch read per
    /// freshness window, rather than each paying for its own.
    pool_cache: Mutex<HashMap<String, (i64, i64)>>,
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
            pool_cache: Mutex::new(HashMap::new()),
            fresh_ms: FRESH_MS,
        }
    }

    /// Keyed on (coworker, PAYER), not on the coworker: the cap is the coworker's but the pool
    /// is the payer's, so two members talking to one shared coworker have different answers and
    /// a coworker-keyed entry would serve one of them the other's for a freshness window.
    /// Public for the test that pins the cache key: the leak this prevents is invisible from
    /// outside, because it shows up as one member being told another member's limits for a
    /// freshness window and then quietly correcting itself.
    pub async fn limits_for(
        &self,
        coworker: &CoworkerId,
        payer: &AccountId,
    ) -> Result<crate::points::Effective, String> {
        let cache_key = pair_key(coworker, payer);
        if self.fresh_ms > 0
            && let Ok(cache) = self.limits_cache.lock()
            && let Some((limits, at_ms)) = cache.get(&cache_key)
            && now_ms() - *at_ms <= self.fresh_ms
        {
            return Ok(limits.clone());
        }
        let limits = crate::points::effective(&self.store, coworker, payer).await?;
        if let Ok(mut cache) = self.limits_cache.lock() {
            cache.insert(cache_key, (limits.clone(), now_ms()));
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

impl GuardedDoor {
    /// Keyed on the PAYER alone, deliberately unlike `limits_cache`. A pool is a per-person
    /// budget: keying it on the pair would fragment one person's month across every coworker
    /// they talk to and let them overspend by a freshness window per coworker, with no single
    /// cache line ever noticing.
    fn cached_pool(&self, payer: &str, within_ms: i64) -> Option<i64> {
        let cache = self.pool_cache.lock().ok()?;
        let (points, at_ms) = cache.get(payer)?;
        (now_ms() - at_ms <= within_ms).then_some(*points)
    }

    /// The PAYER's pool total this month — one batch read over every key ever minted for that
    /// person on any coworker, with the turn's patience and the same fresh/stale ladder as the
    /// meter. Not "the owner's", and not "their coworkers' keys": on a shared coworker the
    /// person talking is not the hirer, and reaching for `coworker_owner` here would restore
    /// the bug this module was re-keyed to remove — invisibly, because on an unshared coworker
    /// the two still agree and every test would still pass.
    async fn pool_reading(&self, payer: &AccountId, name: &str) -> Result<i64, ModelError> {
        if self.fresh_ms > 0
            && let Some(points) = self.cached_pool(payer.as_str(), self.fresh_ms)
        {
            return Ok(points);
        }
        let Some(admin) = self.admin.as_ref() else {
            return Err(ModelError::SpendCap(format!(
                "{name}'s turn draws on a points pool, but this deployment has no gateway admin \
                 connection to read it with (OG_GATEWAY_ADMIN_URL). Its turns are held until it does."
            )));
        };
        let keys = crate::points::pool_keys(&self.store, payer)
            .await
            .map_err(|error| {
                ModelError::SpendCap(format!(
                    "The keys {name}'s pool sums over could not be listed ({error}); the turn \
                     is held."
                ))
            })?;
        match admin
            .points_for_keys_within(&keys, "month", METER_TIMEOUT)
            .await
        {
            Ok(Some(per_key)) => {
                let points: i64 = per_key.values().sum();
                if let Ok(mut cache) = self.pool_cache.lock() {
                    cache.insert(payer.as_str().to_string(), (points, now_ms()));
                }
                Ok(points)
            }
            Ok(None) => Err(ModelError::SpendCap(format!(
                "{name}'s turn draws on a points pool, but the gateway has no reference price \
                 set, so points cannot be counted; an admin sets it on the admin page. The turn \
                 is held."
            ))),
            Err(error) => {
                tracing::error!(%error, payer = %payer.as_str(), "points guard: the pool could not be read");
                match self.cached_pool(payer.as_str(), STALE_OK_MS) {
                    Some(points) => Ok(points),
                    None => Err(ModelError::SpendCap(format!(
                        "The pool {name}'s turn draws on could not be read ({error}); it is held. \
                         Try again in a moment."
                    ))),
                }
            }
        }
    }
}

fn rfc3339_at(text: Option<&str>) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(text?)
        .ok()
        .map(|t| t.with_timezone(&chrono::Utc))
}

/// What the guard knows when it decides: the coworker's month and day, and the owner's pool.
struct Counted {
    month: i64,
    day: i64,
    day_frees_at: Option<String>,
    resets_at: Option<String>,
    pool_used: Option<i64>,
}

/// The sentence a person reads when a limit is reached, or `None` when there is room at every
/// one. The cap first, then the pool, then the day's brake: the numbers the person can act
/// on, and when it frees up.
fn over_points(name: &str, limits: &crate::points::Effective, counted: &Counted) -> Option<String> {
    use crate::points::commas;
    let month_name = chrono::Utc::now().format("%B").to_string();
    let resets = rfc3339_at(counted.resets_at.as_deref())
        .map(|t| format!("it resets on {}", t.format("%-d %B")))
        .unwrap_or_else(|| "it resets on the first of next month".to_string());
    if let Some(cap) = limits.cap
        && counted.month >= cap
    {
        let mut sentence = format!(
            "{name} has used its {} points for {month_name} ({} used); {resets}.",
            commas(cap),
            commas(counted.month)
        );
        if let (Some(pool), Some(used)) = (limits.pool, counted.pool_used) {
            sentence.push_str(&format!(
                " {} of your {} remain for other agents.",
                commas(pool.saturating_sub(used).max(0)),
                commas(pool)
            ));
        }
        return Some(sentence);
    }
    if let (Some(pool), Some(used)) = (limits.pool, counted.pool_used)
        && used >= pool
    {
        return Some(format!(
            "Your pool of {} points for {month_name} is used up ({} used); {resets}.",
            commas(pool),
            commas(used)
        ));
    }
    if let Some(day_cap) = limits.day_cap
        && counted.day >= day_cap
    {
        let frees = rfc3339_at(counted.day_frees_at.as_deref())
            .map(|t| format!("it frees up at {}", t.format("%H:%M UTC")))
            .unwrap_or_else(|| "it frees up as older spend ages out".to_string());
        return Some(format!(
            "{name} has used its {} points for today ({} used); {frees}.",
            commas(day_cap),
            commas(counted.day)
        ));
    }
    None
}

#[async_trait::async_trait]
impl ModelDoor for GuardedDoor {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
        let Some(scope) = request.spend_scope.clone() else {
            return self.inner.stream(request).await;
        };
        let coworker = CoworkerId::from_stored(scope);
        // Whose spend this is. A request that names a scope but no actor is one the server built
        // without resolving a payer; it is held rather than billed to a guess, because the guess
        // that reads best — the coworker's owner — is the one that would let a shared coworker's
        // turns quietly draw down somebody else's pool.
        let Some(payer) = request.spend_actor.clone().map(AccountId::from_stored) else {
            // Loud on this side too: only our own code can build a request like this, and a
            // sentence to the person turns our bug into their mystery.
            tracing::error!(
                coworker = %coworker.as_str(),
                "points guard: a request named a spend scope but no actor; the turn is held"
            );
            return Err(ModelError::SpendCap(
                "This turn does not say whose spend it is, so it cannot be counted against \
                 anybody's limits; it is held. This is a server bug, not a limit you have hit."
                    .to_string(),
            ));
        };
        let limits = self.limits_for(&coworker, &payer).await.map_err(|error| {
                tracing::error!(%error, coworker = %coworker.as_str(), "points guard: limits could not be read");
                ModelError::SpendCap(format!(
                    "This coworker's points limits could not be read ({error}); the turn is held."
                ))
            })?;
        if !limits.is_limited() {
            return self.inner.stream(request).await;
        }
        let name = self
            .store
            .load_coworker(&coworker)
            .await
            .map(|(coworker, _)| coworker.name)
            .unwrap_or_else(|_| "This coworker".to_string());
        // A limit — its own, or its owner's pool — cannot be honoured without a key of its own
        // to count on: held, and the sentence says what would make it countable.
        let key = match self.store.coworker_key(&coworker, &payer).await {
            Ok(Some(key)) => key,
            Ok(None) => {
                return Err(ModelError::SpendCap(format!(
                    "{name} is under a points limit but has no gateway key of its own to count on, \
                     so its turns are held. A coworker is metered when its hirer is in an org and \
                     the deployment has a gateway admin connection and a vault; ask an admin."
                )));
            }
            Err(error) => {
                return Err(ModelError::SpendCap(format!(
                    "{name}'s key could not be read ({error}); the turn is held."
                )));
            }
        };
        let usage = self.reading(&key.key_id, &name).await?;
        let (Some(month), Some(day)) = (usage.month_points, usage.day_points) else {
            return Err(ModelError::SpendCap(format!(
                "{name} is under a points limit, but the gateway has no reference price set, so \
                 points cannot be counted; an admin sets it on the admin page. The turn is held."
            )));
        };
        let pool_used = match limits.pool {
            Some(_) => Some(self.pool_reading(&payer, &name).await?),
            None => None,
        };
        let counted = Counted {
            month,
            day,
            day_frees_at: usage.day_frees_at.clone(),
            resets_at: usage.month_resets_at.clone(),
            pool_used,
        };
        if let Some(sentence) = over_points(&name, &limits, &counted) {
            tracing::info!(coworker = %coworker.as_str(), "points guard: refused — {sentence}");
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
    // The row stays, marked: a retired coworker's month still counts toward its owner's pool.
    // EVERY key, not the owner's: a shared coworker has one per member, and revoking only the
    // hirer's would leave every other member holding a live credential on a retired coworker.
    let rows = match store
        .mark_coworker_keys_revoked(coworker_id, now_ms())
        .await
    {
        Ok(rows) if rows.is_empty() => return,
        Ok(rows) => rows,
        Err(error) => {
            tracing::error!(%error, coworker = %coworker_id.as_str(), "spend cap: the key rows could not be marked revoked at retirement");
            return;
        }
    };
    for row in rows {
        revoke_one(state, coworker_id, row).await;
    }
}

async fn revoke_one(
    state: &AgUiState,
    coworker_id: &CoworkerId,
    row: opengrok_store::CoworkerKeyView,
) {
    let store = &state.auth.store;
    if let Some(admin) = state.auth.gateway_admin.as_ref() {
        if let Err(error) = admin.revoke_key(&row.key_id).await {
            tracing::error!(%error, key = %row.key_id, "spend cap: a retired coworker's key could not be revoked on the gateway");
        }
    } else {
        tracing::error!(key = %row.key_id, "spend cap: no gateway admin connection; a retired coworker's key is still live on the gateway");
    }
    if let Err(error) = store.delete_secret(&secret_id_of(&row, coworker_id)).await {
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
    }

    fn counted(month: i64, day: i64, pool_used: Option<i64>) -> Counted {
        Counted {
            month,
            day,
            day_frees_at: Some("2026-09-03T14:32:10Z".into()),
            resets_at: Some("2026-10-01T00:00:00Z".into()),
            pool_used,
        }
    }

    /// The plan's sentences: the cap with what the pool leaves others, the pool, the day's
    /// brake with when it frees up; nothing when there is room everywhere.
    #[test]
    fn the_sentence_names_the_limit_in_the_way_with_the_numbers_and_when_it_frees_up() {
        let month = chrono::Utc::now().format("%B").to_string();
        let limits = crate::points::Effective {
            cap: Some(100_000),
            day_cap: Some(30_000),
            pool: Some(1_000_000),
            ..crate::points::Effective::none_set()
        };
        assert_eq!(
            over_points("New Bot", &limits, &counted(99_999, 100, Some(500_000))),
            None
        );
        let s = over_points("New Bot", &limits, &counted(102_340, 100, Some(588_000))).unwrap();
        assert_eq!(
            s,
            format!(
                "New Bot has used its 100,000 points for {month} (102,340 used); it resets on \
                 1 October. 412,000 of your 1,000,000 remain for other agents."
            )
        );
        let s = over_points("New Bot", &limits, &counted(10, 5, Some(1_000_000))).unwrap();
        assert_eq!(
            s,
            format!(
                "Your pool of 1,000,000 points for {month} is used up (1,000,000 used); it \
                 resets on 1 October."
            )
        );
        let s = over_points("New Bot", &limits, &counted(10, 30_000, Some(10))).unwrap();
        assert_eq!(
            s,
            "New Bot has used its 30,000 points for today (30,000 used); it frees up at 14:32 UTC."
        );
        // A cap alone says nothing about a pool.
        let cap_only = crate::points::Effective {
            cap: Some(1_000),
            ..crate::points::Effective::none_set()
        };
        let s = over_points("Ada", &cap_only, &counted(1_000, 0, None)).unwrap();
        assert_eq!(
            s,
            format!(
                "Ada has used its 1,000 points for {month} (1,000 used); it resets on 1 October."
            )
        );
        assert_eq!(
            over_points(
                "Ada",
                &crate::points::Effective::none_set(),
                &counted(1, 1, None)
            ),
            None
        );
    }
}
