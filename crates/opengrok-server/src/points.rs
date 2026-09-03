//! Points: what a coworker may spend, in the gateway's reference tokens (`docs/plan-spend-policy.md`).
//!
//! A point is one token at the reference price the org admin sets on the gateway (USD per
//! million tokens), so a subscription seat and an API key count the same: each request's points
//! are its list-price cost over that price, rounded per request and summed — the gateway does
//! that arithmetic in one place (open-ai-gateway #53) and this server never re-derives it.
//!
//! Two limits, two authorities. The org admin sets each MEMBER's monthly POOL; every coworker
//! the member owns draws on it. A coworker's OWNER sets an optional monthly CAP on that
//! coworker (at most the pool) and an optional daily BRAKE (a rolling 24 hours). `None` is "no
//! limit here", never zero: zero is an explicit stop. `GuardedDoor` (`spend.rs`) refuses a model
//! call at any of the three with a sentence in the bubble; nothing is sent to the model.
//!
//! Usage is a report and knows nothing about limits: per model, per window, from the gateway's
//! ledger (`usage_for`).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_store::{PgStore, PointsLimit, PointsScope};
use serde::Serialize;
use serde_json::{Value, json};

use crate::agui::AgUiState;
use crate::gateway_admin::{AdminError, GatewayAdmin, ModelPoints, ModelUsage};
use crate::spend::micros;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// The most a limit may be: a thousand billion reference tokens. Anything larger is a typo, and
/// the column is a bigint.
pub const MAX_POINTS: i64 = 1_000_000_000_000_000;

/// The windows the gateway meters, in their wire spelling.
pub const WINDOWS: [&str; 4] = ["5h", "24h", "7d", "month"];

/// A whole number of points, or the sentence for a 400.
pub fn validate_points(label: &str, value: Option<i64>) -> Result<(), String> {
    match value {
        Some(points) if points < 0 => Err(format!("{label} must be a whole number of points")),
        Some(points) if points > MAX_POINTS => Err(format!(
            "{label} must be at most {} points",
            commas(MAX_POINTS)
        )),
        _ => Ok(()),
    }
}

/// `1234567` → `1,234,567`, the way every sentence and card writes points.
pub fn commas(points: i64) -> String {
    let digits = points.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    if points < 0 {
        out.insert(0, '-');
    }
    out
}

/// The limits THIS person's turn on a coworker is under: the coworker's own cap and brake (and
/// who set them), and the payer's pool (and who set it). No `Default`: an `Effective` with no
/// payer would be a set of limits nobody is answerable for, and the only reason to want one is
/// to skip saying whose turn it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effective {
    pub cap: Option<i64>,
    pub day_cap: Option<i64>,
    pub cap_set_by: Option<String>,
    /// Whose pool this turn draws on — the person TALKING, not the coworker's hirer. On a
    /// coworker only its owner can reach they are the same account; on a shared one they are
    /// not, and billing the hirer for somebody else's conversation is the bug this names away.
    pub payer: AccountId,
    pub pool: Option<i64>,
    pub pool_set_by: Option<String>,
}

impl Effective {
    /// Nothing set, for the arithmetic tests. Deliberately NOT a `Default` impl: outside a test
    /// an `Effective` has to name whose pool it is, and a derived default would let a caller
    /// skip saying — which is exactly the bug this type was re-keyed to prevent.
    #[cfg(test)]
    pub fn none_set() -> Self {
        Self {
            cap: None,
            day_cap: None,
            cap_set_by: None,
            payer: AccountId::from_stored("acct_test".to_string()),
            pool: None,
            pool_set_by: None,
        }
    }

    /// Whether anything at all is enforced: with nothing set the guard never touches the meter.
    pub fn is_limited(&self) -> bool {
        self.cap.is_some() || self.day_cap.is_some() || self.pool.is_some()
    }
}

/// The limits that bind THIS person's turn on this coworker: the coworker's own cap and daily
/// brake, and the payer's monthly pool. The payer is an argument rather than a lookup because
/// the answer differs per member on a shared coworker, and a function that resolved the owner
/// itself would silently give every member the hirer's pool.
pub async fn effective(
    store: &PgStore,
    coworker: &CoworkerId,
    payer: &AccountId,
) -> Result<Effective, String> {
    let own = store
        .points_limit(PointsScope::Coworker, coworker.as_str())
        .await
        .map_err(|error| error.to_string())?;
    let pool = store
        .points_limit(PointsScope::Member, payer.as_str())
        .await
        .map_err(|error| error.to_string())?;
    Ok(Effective {
        cap: own.as_ref().and_then(|row| row.limit.month_points),
        day_cap: own.as_ref().and_then(|row| row.limit.day_points),
        cap_set_by: own.map(|row| row.set_by),
        payer: payer.clone(),
        pool: pool.as_ref().and_then(|row| row.limit.month_points),
        pool_set_by: pool.map(|row| row.set_by),
    })
}

/// The most this coworker could still be allowed to have used this month: its cap, bounded by
/// what its owner's pool leaves after the OTHER coworkers' spend — so a pool lowered after a
/// cap was set still binds. `None` when neither exists.
pub fn effective_cap(
    limits: &Effective,
    own_used: Option<i64>,
    pool_used: Option<i64>,
) -> Option<i64> {
    let others = pool_used.unwrap_or(0).saturating_sub(own_used.unwrap_or(0));
    let from_pool = limits.pool.map(|pool| pool.saturating_sub(others).max(0));
    match (limits.cap, from_pool) {
        (Some(cap), Some(room)) => Some(cap.min(room)),
        (Some(cap), None) => Some(cap),
        (None, Some(room)) => Some(room),
        (None, None) => None,
    }
}

/// The key ids a person's pool sums over: every key ever minted FOR THEM on any coworker,
/// revoked ones included — a retired coworker's month still counts, and on a shared coworker
/// their own key counts toward their own pool rather than the hirer's.
pub async fn pool_keys(store: &PgStore, payer: &AccountId) -> Result<Vec<String>, String> {
    store
        .coworker_keys_for_account(payer)
        .await
        .map(|rows| rows.into_iter().map(|row| row.key_id).collect())
        .map_err(|error| error.to_string())
}

// ---- The limit, read and written --------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolView {
    pub max: Option<i64>,
    pub used: Option<i64>,
    pub resets_at: Option<String>,
    pub set_by: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferenceView {
    pub usd_per_mtok: String,
}

/// `GET /coworkers/{id}/limit`. Every field present; null where the gateway does not say.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitView {
    pub metered: bool,
    pub note: Option<String>,
    pub cap: Option<i64>,
    pub effective_cap: Option<i64>,
    pub used_points: Option<i64>,
    pub day_cap: Option<i64>,
    pub used_today: Option<i64>,
    pub day_frees_at: Option<String>,
    pub pool: PoolView,
    pub reference: Option<ReferenceView>,
}

/// The first of next UTC month, for a reply that has no gateway reading to take it from.
fn next_month_start() -> String {
    let now = chrono::Utc::now();
    let (year, month) = if now.format("%m").to_string() == "12" {
        (
            now.format("%Y").to_string().parse::<i32>().unwrap_or(2026) + 1,
            1,
        )
    } else {
        (
            now.format("%Y").to_string().parse::<i32>().unwrap_or(2026),
            now.format("%m").to_string().parse::<u32>().unwrap_or(1) + 1,
        )
    };
    format!("{year:04}-{month:02}-01T00:00:00Z")
}

/// The pool's month total over the payer's keys, or `None` when it cannot be read (no admin
/// connection, no reference price, the gateway not answering).
async fn pool_used(state: &AgUiState, admin: &GatewayAdmin, payer: &AccountId) -> Option<i64> {
    let keys = pool_keys(&state.auth.store, payer).await.ok()?;
    if keys.is_empty() {
        return Some(0);
    }
    match admin
        .points_for_keys_within(&keys, "month", std::time::Duration::from_secs(15))
        .await
    {
        Ok(Some(per_key)) => Some(per_key.values().sum()),
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(%error, payer = %payer.as_str(), "points: the pool could not be read");
            None
        }
    }
}

pub async fn limit_for(
    state: &AgUiState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
) -> Result<LimitView, String> {
    let store = &state.auth.store;
    // The caller's own key on this coworker, and their own pool: a shared coworker's console
    // row answers about the person reading it, not about its hirer.
    let limits = effective(store, coworker_id, account_id).await?;
    let key = store
        .coworker_key(coworker_id, account_id)
        .await
        .map_err(|error| format!("the key row could not be read: {error}"))?
        .filter(|row| row.revoked_at_ms.is_none());
    let admin = state.auth.gateway_admin.as_ref();
    let mut note = None;
    let reference = match admin {
        Some(admin) => match admin.points_reference().await {
            Ok(reference) => reference,
            Err(error) => {
                note = Some(format!("the gateway could not be asked: {error}"));
                None
            }
        },
        None => {
            note = Some("no gateway admin connection; points cannot be read".to_string());
            None
        }
    };
    if reference.is_none() && note.is_none() {
        note = Some(
            "the gateway has no reference price yet, so points cannot be counted; an admin sets \
             it on the admin page"
                .to_string(),
        );
    }
    let metered = key.is_some() && admin.is_some();
    if key.is_none() && note.is_none() {
        note = Some("this coworker has no key of its own yet, so it is not metered".to_string());
    }
    let (mut used_points, mut used_today, mut day_frees_at, mut resets_at) =
        (None, None, None, None);
    if let (Some(admin), Some(key), Some(_)) = (admin, key.as_ref(), reference.as_ref()) {
        match admin.key_usage(&key.key_id).await {
            Ok(Some(usage)) => {
                used_points = usage.month_points;
                used_today = usage.day_points;
                day_frees_at = usage.day_frees_at;
                resets_at = usage.month_resets_at;
            }
            Ok(None) => note = Some("the gateway no longer knows this key".to_string()),
            Err(error) => note = Some(format!("the gateway could not be asked: {error}")),
        }
    }
    let pool_total = match (admin, reference.as_ref()) {
        (Some(admin), Some(_)) => pool_used(state, admin, &limits.payer).await,
        _ => None,
    };
    Ok(LimitView {
        metered,
        note,
        cap: limits.cap,
        effective_cap: effective_cap(&limits, used_points, pool_total),
        used_points,
        day_cap: limits.day_cap,
        used_today,
        day_frees_at,
        pool: PoolView {
            max: limits.pool,
            used: pool_total,
            resets_at: Some(resets_at.unwrap_or_else(next_month_start)),
            set_by: limits.pool_set_by,
        },
        reference: reference.map(|usd_per_mtok| ReferenceView { usd_per_mtok }),
    })
}

/// A field of the PUT body: absent is "leave it", null is "clear it", a number is the value.
fn field(body: &Value, name: &str) -> Result<Option<Option<i64>>, String> {
    match body.get(name) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(value) => match value.as_i64() {
            Some(points) => {
                validate_points(name, Some(points))?;
                Ok(Some(Some(points)))
            }
            None => Err(format!(
                "{name} must be a whole number of points, or null to clear it"
            )),
        },
    }
}

/// `PUT /coworkers/{id}/limit` ← `{ cap, dayCap }`, the owner's. A cap above the owner's pool
/// is refused with the numbers. `Err((400, sentence))` for the caller to answer as is.
pub async fn set_limit(
    state: &AgUiState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    body: &Value,
) -> Result<LimitView, (u16, String)> {
    let cap = field(body, "cap").map_err(|m| (400, m))?;
    let day_cap = field(body, "dayCap").map_err(|m| (400, m))?;
    let store = &state.auth.store;
    let limits = effective(store, coworker_id, account_id)
        .await
        .map_err(|m| (503, m))?;
    if let (Some(Some(cap)), Some(pool)) = (cap, limits.pool)
        && cap > pool
    {
        return Err((
            400,
            format!(
                "a cap of {} points is above your pool of {}",
                commas(cap),
                commas(pool)
            ),
        ));
    }
    let next = PointsLimit {
        month_points: cap.unwrap_or(limits.cap),
        day_points: day_cap.unwrap_or(limits.day_cap),
    };
    store
        .put_points_limit(
            PointsScope::Coworker,
            coworker_id.as_str(),
            next,
            account_id.as_str(),
            now_ms(),
        )
        .await
        .map_err(|error| (503, format!("the limit could not be saved: {error}")))?;
    limit_for(state, account_id, coworker_id)
        .await
        .map_err(|m| (503, m))
}

// ---- Usage: a report, per model ---------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelUsageView {
    pub model_id: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cost_usd: String,
    pub list_usd: String,
    pub points: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TotalsView {
    pub requests: Option<i64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub cost_usd: Option<String>,
    pub list_usd: Option<String>,
    pub points: Option<i64>,
}

/// `GET /coworkers/{id}/usage?window=`. `models` is empty and `totals` all null for a coworker
/// that is not metered; the reason is in `note`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageView {
    pub metered: bool,
    pub note: Option<String>,
    pub seat: Option<&'static str>,
    pub key_prefix: Option<String>,
    pub window: String,
    pub models: Vec<ModelUsageView>,
    pub totals: TotalsView,
}

fn dollars(micros: i64) -> String {
    format!("{}.{:06}", micros / 1_000_000, micros % 1_000_000)
}

fn totals_of(models: &[ModelUsage]) -> TotalsView {
    let cost: i64 = models.iter().filter_map(|m| micros(&m.cost_usd)).sum();
    let list: i64 = models.iter().filter_map(|m| micros(&m.list_usd)).sum();
    let points = if models.iter().all(|m| m.points.is_some()) {
        Some(models.iter().filter_map(|m| m.points).sum())
    } else {
        None
    };
    TotalsView {
        requests: Some(models.iter().map(|m| m.requests).sum()),
        input_tokens: Some(models.iter().map(|m| m.input_tokens).sum()),
        output_tokens: Some(models.iter().map(|m| m.output_tokens).sum()),
        cache_read_tokens: Some(models.iter().map(|m| m.cache_read_tokens).sum()),
        cache_write_tokens: Some(models.iter().map(|m| m.cache_write_tokens).sum()),
        cost_usd: Some(dollars(cost)),
        list_usd: Some(dollars(list)),
        points,
    }
}

/// `"api"` when the window carries cost, `"subscription"` when only a displaced bill, nothing
/// when neither.
fn seat_of(totals: &TotalsView) -> Option<&'static str> {
    let cost = totals.cost_usd.as_deref().and_then(micros).unwrap_or(0);
    let list = totals.list_usd.as_deref().and_then(micros).unwrap_or(0);
    if cost > 0 {
        Some("api")
    } else if list > 0 {
        Some("subscription")
    } else {
        None
    }
}

pub async fn usage_for(
    state: &AgUiState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    window: &str,
) -> Result<UsageView, (u16, String)> {
    if !WINDOWS.contains(&window) {
        return Err((
            400,
            format!("'{window}' is not a window; one of {}", WINDOWS.join(", ")),
        ));
    }
    let store = &state.auth.store;
    let key = store
        .coworker_key(coworker_id, account_id)
        .await
        .map_err(|error| (503, format!("the key row could not be read: {error}")))?
        .filter(|row| row.revoked_at_ms.is_none());
    let unmetered = |note: String| UsageView {
        metered: false,
        note: Some(note),
        seat: None,
        key_prefix: None,
        window: window.to_string(),
        models: Vec::new(),
        totals: TotalsView::default(),
    };
    let Some(key) = key else {
        return Ok(unmetered(
            "this coworker has no key of its own yet, so it is not metered".to_string(),
        ));
    };
    let Some(admin) = state.auth.gateway_admin.as_ref() else {
        return Ok(unmetered(
            "no gateway admin connection; usage cannot be read".to_string(),
        ));
    };
    let (models, note) = match admin.key_usage_models(&key.key_id, window).await {
        Ok(Some(models)) => (models, None),
        Ok(None) => (
            Vec::new(),
            Some("the gateway no longer knows this key".to_string()),
        ),
        Err(AdminError::Refused(detail)) if detail.starts_with("HTTP 404") => (
            Vec::new(),
            Some(
                "this gateway does not report usage per model yet (older than open-ai-gateway #53)"
                    .to_string(),
            ),
        ),
        Err(error) => (
            Vec::new(),
            Some(format!("the gateway could not be asked: {error}")),
        ),
    };
    let totals = totals_of(&models);
    Ok(UsageView {
        metered: true,
        note,
        seat: seat_of(&totals),
        key_prefix: Some(key.key_prefix),
        window: window.to_string(),
        models: models
            .into_iter()
            .map(|m| ModelUsageView {
                model_id: m.model_id,
                requests: m.requests,
                input_tokens: m.input_tokens,
                output_tokens: m.output_tokens,
                cache_read_tokens: m.cache_read_tokens,
                cache_write_tokens: m.cache_write_tokens,
                cost_usd: m.cost_usd,
                list_usd: m.list_usd,
                points: m.points,
            })
            .collect(),
        totals,
    })
}

// ---- Multipliers for the picker ---------------------------------------------------------

/// How long the multipliers are reused: a picker mounts often, the catalog moves rarely.
const MODELS_FRESH_MS: i64 = 60_000;
type ModelsCache = Mutex<(i64, Option<HashMap<String, ModelPoints>>)>;
static MODELS_CACHE: OnceLock<ModelsCache> = OnceLock::new();

/// Every model's multipliers by id, `None` while the gateway has no reference price (or is
/// older than open-ai-gateway #52), cached a minute per process.
pub async fn models_points(state: &AgUiState) -> Option<HashMap<String, ModelPoints>> {
    let admin = state.auth.gateway_admin.as_ref()?;
    if let Ok(cache) = MODELS_CACHE.get_or_init(|| Mutex::new((0, None))).lock()
        && now_ms() - cache.0 <= MODELS_FRESH_MS
    {
        return cache.1.clone();
    }
    let fetched = match admin.points_models().await {
        Ok(Some(models)) => Some(
            models
                .into_iter()
                .map(|model| (model.id.clone(), model))
                .collect::<HashMap<_, _>>(),
        ),
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(%error, "points: the multipliers could not be read");
            return None;
        }
    };
    if let Ok(mut cache) = MODELS_CACHE.get_or_init(|| Mutex::new((0, None))).lock() {
        *cache = (now_ms(), fetched.clone());
    }
    fetched
}

/// The catalog id an advertised id prices at: `xai/grok-4.6@sub` and `xai/grok-4.6@api` are
/// the same model on two credentials, and the gateway's multipliers are listed by catalog id
/// alone — so an alias inherits its base model's. A ladder id (`oag/cheap`) has no base price
/// and stays null.
pub fn base_model(id: &str) -> &str {
    id.split_once('@').map_or(id, |(base, _)| base)
}

/// The `points` object a `/models` entry carries, or null.
pub fn points_json(model: Option<&ModelPoints>) -> Value {
    match model {
        Some(m) => json!({
            "inputX": m.input_x,
            "outputX": m.output_x,
            "cacheReadX": m.cache_read_x,
            "cacheWriteX": m.cache_write_x,
            "shownX": m.shown_x,
        }),
        None => Value::Null,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn an_alias_prices_at_its_base_model() {
        assert_eq!(base_model("xai/grok-4.6@sub"), "xai/grok-4.6");
        assert_eq!(base_model("openai/gpt-5.5@api"), "openai/gpt-5.5");
        assert_eq!(base_model("xai/grok-4.6"), "xai/grok-4.6");
        assert_eq!(base_model("oag/cheap"), "oag/cheap");
    }

    #[test]
    fn points_are_written_with_commas() {
        assert_eq!(commas(0), "0");
        assert_eq!(commas(999), "999");
        assert_eq!(commas(1_000), "1,000");
        assert_eq!(commas(1_234_567), "1,234,567");
        assert_eq!(commas(50_000_000), "50,000,000");
    }

    #[test]
    fn the_effective_cap_is_the_cap_bounded_by_what_the_pool_leaves_others() {
        let both = Effective {
            cap: Some(100_000),
            pool: Some(1_000_000),
            ..Effective::none_set()
        };
        // Others used 950,000 of the million: the pool leaves 50,000, under the cap.
        assert_eq!(
            effective_cap(&both, Some(10_000), Some(960_000)),
            Some(50_000)
        );
        // Others used little: the cap binds.
        assert_eq!(
            effective_cap(&both, Some(10_000), Some(20_000)),
            Some(100_000)
        );
        let cap_only = Effective {
            cap: Some(100_000),
            ..Effective::none_set()
        };
        assert_eq!(effective_cap(&cap_only, None, None), Some(100_000));
        let pool_only = Effective {
            pool: Some(1_000_000),
            ..Effective::none_set()
        };
        assert_eq!(
            effective_cap(&pool_only, Some(5), Some(400_005)),
            Some(600_000)
        );
        assert_eq!(effective_cap(&Effective::none_set(), None, None), None);
    }

    #[test]
    fn a_limit_is_a_whole_non_negative_number_of_points() {
        assert!(validate_points("cap", None).is_ok());
        assert!(validate_points("cap", Some(0)).is_ok());
        assert!(validate_points("cap", Some(-1)).is_err());
        assert!(validate_points("cap", Some(MAX_POINTS + 1)).is_err());
        let body = json!({ "cap": 100, "dayCap": null });
        assert_eq!(field(&body, "cap").unwrap(), Some(Some(100)));
        assert_eq!(field(&body, "dayCap").unwrap(), Some(None));
        assert_eq!(field(&body, "other").unwrap(), None, "absent is leave it");
        assert!(field(&json!({ "cap": "ten" }), "cap").is_err());
    }
}
