//! The other door's key desk — our client for open-ai-gateway's admin API.
//!
//! One identity, two doors: an org here is a *principal* there, and each member's key is an
//! *api_key* on that principal. That mapping is not decoration — it is what makes the gateway's
//! own machinery do the work: the principal carries the org's monthly budget and is what its usage
//! rolls up to, and the api_key carries the member's spend cap and is what an individual revoke
//! acts on. We store no budget or usage of our own, because a second copy of a number about money
//! is a number that will eventually disagree.
//!
//! NOTE the two different gateway credentials. `OG_GATEWAY_TOKEN` is an *inference* key — what a
//! run spends. `OG_GATEWAY_ADMIN_TOKEN` is an *admin* key (`oag admin key create --admin`) and is
//! only ever used here. Unset means this whole surface is off, which is the right default for a
//! deployment that has not wired the two together; it is never a boot failure.
//!
//! The gateway has **no GET list** of principals (`GET /admin/api/principals` is 405 by design).
//! Upsert is `POST /admin/api/principals`; usage is `GET /admin/api/principals/{email}/usage`.
//! A 405 is an answer from a live gateway (`Refused`), never `Unreachable`.

use serde::Deserialize;

/// A minted key, as the gateway hands it back. `key` is the plaintext and this is the only time it
/// exists — it goes straight to the person who asked for it and is never written down.
#[derive(Debug, Clone, Deserialize)]
pub struct MintedKey {
    pub id: String,
    #[serde(rename = "key_prefix")]
    pub key_prefix: String,
    pub key: String,
}

/// An org's spend against its cap, read live from the gateway's ledger.
/// One key as the gateway lists it (`GET /admin/api/keys`): enough to reconcile our attribution
/// against the authority — which keys exist for the org's principal, and whether each still
/// authenticates. No secret, no hash.
#[derive(Debug, Clone, Deserialize)]
pub struct GatewayKeyListing {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub key_prefix: String,
    /// The principal's email — `org_principal_email(org)` for keys minted through the console.
    #[serde(default)]
    pub principal: String,
    #[serde(default = "default_true")]
    pub active: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct PrincipalUsage {
    pub email: String,
    #[serde(rename = "monthly_budget_usd")]
    pub monthly_budget_usd: Option<String>,
    #[serde(rename = "month_to_date_usd")]
    pub month_to_date_usd: String,
    pub requests: i64,
}

/// One key's cap and spend as the gateway reports it (`GET /admin/api/keys/{id}/usage`). Two
/// spend figures on purpose: `spent_usd` is what the cap is enforced against (lifetime, the
/// gateway's counter); `month_to_date_usd` is the ledger this month. Money as strings, as the
/// gateway sends it — never a float.
#[derive(Debug, Clone, Deserialize)]
pub struct KeyUsage {
    #[serde(rename = "quota_usd")]
    pub quota_usd: Option<String>,
    #[serde(rename = "spent_usd")]
    pub spent_usd: String,
    #[serde(rename = "month_to_date_usd")]
    pub month_to_date_usd: String,
    /// RFC 3339: the first of the next UTC month.
    #[serde(rename = "month_resets_at", default)]
    pub month_resets_at: Option<String>,
    pub requests: i64,
    /// The rolling windows: the sum, and when the window next frees up (its oldest spend
    /// ageing out), RFC 3339 or absent for an empty window.
    #[serde(rename = "five_hour_usd", default)]
    pub five_hour_usd: Option<String>,
    #[serde(rename = "five_hour_frees_at", default)]
    pub five_hour_frees_at: Option<String>,
    #[serde(rename = "seven_day_usd", default)]
    pub seven_day_usd: Option<String>,
    #[serde(rename = "seven_day_frees_at", default)]
    pub seven_day_frees_at: Option<String>,
    /// Requests inside the rolling windows (the month's are `requests`). Absent on a gateway
    /// older than open-ai-gateway #51.
    #[serde(rename = "five_hour_requests", default)]
    pub five_hour_requests: Option<i64>,
    #[serde(rename = "seven_day_requests", default)]
    pub seven_day_requests: Option<i64>,
    /// What the same tokens would have cost at the model's list API price: a subscription
    /// seat's usage against the bill it displaced, its own cost being truthfully zero. Absent on
    /// an older gateway.
    #[serde(rename = "month_counterfactual_usd", default)]
    pub month_counterfactual_usd: Option<String>,
    #[serde(rename = "five_hour_counterfactual_usd", default)]
    pub five_hour_counterfactual_usd: Option<String>,
    #[serde(rename = "seven_day_counterfactual_usd", default)]
    pub seven_day_counterfactual_usd: Option<String>,
    /// The rolling day (open-ai-gateway #53): the optional daily brake. Absent on an older
    /// gateway.
    #[serde(rename = "day_usd", default)]
    pub day_usd: Option<String>,
    #[serde(rename = "day_frees_at", default)]
    pub day_frees_at: Option<String>,
    #[serde(rename = "day_requests", default)]
    pub day_requests: Option<i64>,
    #[serde(rename = "day_counterfactual_usd", default)]
    pub day_counterfactual_usd: Option<String>,
    /// Points per window: each request's list-price cost over the reference price, rounded
    /// per request and summed. Absent on an older gateway, null while no reference is set.
    #[serde(rename = "month_points", default)]
    pub month_points: Option<i64>,
    #[serde(rename = "five_hour_points", default)]
    pub five_hour_points: Option<i64>,
    #[serde(rename = "day_points", default)]
    pub day_points: Option<i64>,
    #[serde(rename = "seven_day_points", default)]
    pub seven_day_points: Option<i64>,
}

/// A model's points multipliers over the reference price, as the gateway derives them from
/// its catalog (open-ai-gateway #52). Strings like `10`, `2.5`, `0.125`; `None` for a token
/// class the catalog has no price for.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct ModelPoints {
    pub id: String,
    pub input_x: String,
    pub output_x: String,
    #[serde(default)]
    pub cache_read_x: Option<String>,
    #[serde(default)]
    pub cache_write_x: Option<String>,
    pub shown_x: String,
}

/// One model's share of a key's usage inside a window (open-ai-gateway #53).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct ModelUsage {
    pub model_id: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cost_usd: String,
    pub list_usd: String,
    #[serde(default)]
    pub points: Option<i64>,
}

/// Why a call to the gateway's admin API did not do what was asked. There is no
/// "not configured" case here on purpose: a deployment with no admin connection has
/// no `GatewayAdmin` at all, so the refusal happens before a call is attempted.
#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    #[error("the gateway refused: {0}")]
    Refused(String),
    #[error("the gateway is unreachable: {0}")]
    Unreachable(String),
}

#[derive(Clone)]
pub struct GatewayAdmin {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

impl std::fmt::Debug for GatewayAdmin {
    /// Hand-written so the admin token cannot reach a log through a derived `Debug`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayAdmin")
            .field("base_url", &self.base_url)
            .field("token", &"«redacted»")
            .finish()
    }
}

impl GatewayAdmin {
    /// An explicit connection. What a test points at a stand-in gateway, and what `from_env`
    /// builds after reading the two variables.
    #[must_use]
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
            http: reqwest::Client::new(),
        }
    }

    /// From the environment, or `None` when the deployment has not wired the admin door.
    pub fn from_env() -> Option<Self> {
        let base_url = std::env::var("OG_GATEWAY_ADMIN_URL")
            .ok()
            .filter(|url| !url.is_empty())?;
        let token = std::env::var("OG_GATEWAY_ADMIN_TOKEN")
            .ok()
            .filter(|token| !token.is_empty())?;
        Some(Self::new(base_url, token))
    }

    /// The principal that IS this org. Deterministic, so we store no gateway ids: the org id is
    /// the identity, and the address is derivable from it on both sides of a restart.
    #[must_use]
    pub fn org_principal_email(org_id: &str) -> String {
        format!("org-{org_id}@gateway.local")
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, AdminError> {
        self.send_within(method, path, body, std::time::Duration::from_secs(15))
            .await
    }

    /// `send` with the caller's patience: a read on a turn's critical path waits two seconds,
    /// a console press fifteen.
    async fn send_within(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
        timeout: std::time::Duration,
    ) -> Result<serde_json::Value, AdminError> {
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .timeout(timeout);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(|error| AdminError::Unreachable(error.to_string()))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        classify_admin_reply(path, status, &text)
    }

    /// Bind the org to its principal, optionally setting the org's monthly budget. Idempotent, so
    /// it is safe (and correct) to call before every mint rather than tracking whether we have.
    ///
    /// The gateway has **no GET list** of principals: `GET /admin/api/principals` is 405 by
    /// design. Upsert is `POST` with `{email, role?, monthly_budget_usd?}`. `role` is optional
    /// there (default `member`; only `member` is accepted via the API) — we send it so a default
    /// change cannot silently promote an org principal. Omit `monthly_budget_usd` to leave the
    /// budget (COALESCE).
    pub async fn ensure_org_principal(
        &self,
        org_id: &str,
        monthly_budget_usd: Option<&str>,
    ) -> Result<(), AdminError> {
        let mut body = serde_json::json!({
            "email": Self::org_principal_email(org_id),
            "role": "member",
        });
        if let Some(budget) = monthly_budget_usd {
            body["monthly_budget_usd"] = serde_json::Value::String(budget.to_string());
        }
        self.send(reqwest::Method::POST, "/admin/api/principals", Some(body))
            .await
            .map(|_| ())
    }

    /// The method OAG does not serve. GET of the collection is 405 by design; this exists so
    /// the contract test can prove that 405 is `Refused`, never `Unreachable`.
    #[cfg(test)]
    async fn get_principals_collection(&self) -> Result<serde_json::Value, AdminError> {
        self.send(reqwest::Method::GET, "/admin/api/principals", None)
            .await
    }

    /// Mint one member's key on the org's principal. `label` is what the console shows; the
    /// gateway also uses it as the key's name, so a key is identifiable from the gateway side too.
    pub async fn mint_member_key(
        &self,
        org_id: &str,
        label: &str,
        quota_usd: Option<&str>,
    ) -> Result<MintedKey, AdminError> {
        let mut body = serde_json::json!({
            "principal_email": Self::org_principal_email(org_id),
            "name": label,
        });
        if let Some(quota) = quota_usd {
            body["quota_usd"] = serde_json::Value::String(quota.to_string());
        }
        let value = self
            .send(reqwest::Method::POST, "/admin/api/keys", Some(body))
            .await?;
        serde_json::from_value(value)
            .map_err(|error| AdminError::Refused(format!("unexpected mint reply: {error}")))
    }

    /// Every key the gateway holds for this org's principal. The gateway lists all keys and
    /// names each one's principal; the filter is ours, so another org's keys never leave this
    /// function. The listing is accepted as a bare array or `{"keys": [...]}`.
    pub async fn org_keys(&self, org_id: &str) -> Result<Vec<GatewayKeyListing>, AdminError> {
        let body = self
            .send(reqwest::Method::GET, "/admin/api/keys", None)
            .await?;
        let rows = body.get("keys").cloned().unwrap_or(body);
        let listed: Vec<GatewayKeyListing> = serde_json::from_value(rows)
            .map_err(|error| AdminError::Refused(format!("unreadable key listing: {error}")))?;
        let principal = Self::org_principal_email(org_id);
        Ok(listed
            .into_iter()
            .filter(|key| key.principal == principal)
            .collect())
    }

    pub async fn revoke_key(&self, key_id: &str) -> Result<(), AdminError> {
        self.send(
            reqwest::Method::POST,
            &format!("/admin/api/keys/{key_id}/revoke"),
            None,
        )
        .await
        .map(|_| ())
    }

    pub async fn set_org_budget(
        &self,
        org_id: &str,
        monthly_budget_usd: Option<&str>,
    ) -> Result<(), AdminError> {
        let email = encode_path_segment(&Self::org_principal_email(org_id));
        self.send(
            reqwest::Method::PATCH,
            &format!("/admin/api/principals/{email}/budget"),
            Some(serde_json::json!({ "monthly_budget_usd": monthly_budget_usd })),
        )
        .await
        .map(|_| ())
    }

    pub async fn set_key_quota(
        &self,
        key_id: &str,
        quota_usd: Option<&str>,
    ) -> Result<(), AdminError> {
        self.send(
            reqwest::Method::PATCH,
            &format!("/admin/api/keys/{key_id}/quota"),
            Some(serde_json::json!({ "quota_usd": quota_usd })),
        )
        .await
        .map(|_| ())
    }

    /// One key's cap and spend. `None` when the gateway no longer knows the key.
    pub async fn key_usage(&self, key_id: &str) -> Result<Option<KeyUsage>, AdminError> {
        self.key_usage_within(key_id, std::time::Duration::from_secs(15))
            .await
    }

    /// The same read with a bounded wait — what the spend guard uses before a model call.
    pub async fn key_usage_within(
        &self,
        key_id: &str,
        timeout: std::time::Duration,
    ) -> Result<Option<KeyUsage>, AdminError> {
        match self
            .send_within(
                reqwest::Method::GET,
                &format!("/admin/api/keys/{key_id}/usage"),
                None,
                timeout,
            )
            .await
        {
            Ok(value) => serde_json::from_value(value)
                .map(Some)
                .map_err(|error| AdminError::Refused(format!("unexpected usage reply: {error}"))),
            Err(AdminError::Refused(detail)) if detail.contains("no key") => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// The points reference price, `None` while the admin has not set one — or on a gateway
    /// older than open-ai-gateway #52, which has no such door (a 404 either way).
    pub async fn points_reference(&self) -> Result<Option<String>, AdminError> {
        match self
            .send(reqwest::Method::GET, "/admin/api/points/reference", None)
            .await
        {
            Ok(value) => Ok(value
                .get("usd_per_mtok")
                .and_then(|v| v.as_str())
                .map(str::to_string)),
            Err(AdminError::Refused(detail)) if detail.starts_with("HTTP 404") => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Set the reference price; the gateway's own sentence on refusal.
    pub async fn set_points_reference(&self, usd_per_mtok: &str) -> Result<String, AdminError> {
        let value = self
            .send(
                reqwest::Method::PUT,
                "/admin/api/points/reference",
                Some(serde_json::json!({ "usd_per_mtok": usd_per_mtok })),
            )
            .await?;
        value
            .get("usd_per_mtok")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| AdminError::Refused("unexpected reference reply".to_string()))
    }

    /// Every catalog model's multipliers, `None` while no reference is set or on an older
    /// gateway.
    pub async fn points_models(&self) -> Result<Option<Vec<ModelPoints>>, AdminError> {
        match self
            .send(reqwest::Method::GET, "/admin/api/points/models", None)
            .await
        {
            Ok(value) => serde_json::from_value(value)
                .map(Some)
                .map_err(|error| AdminError::Refused(format!("unexpected models reply: {error}"))),
            Err(AdminError::Refused(detail))
                if detail.starts_with("HTTP 404") || detail.contains("no reference price") =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    /// One key's usage inside a window, per model. `None` when the gateway no longer knows the
    /// key; an older gateway (no such door) is a refusal the caller reports.
    pub async fn key_usage_models(
        &self,
        key_id: &str,
        window: &str,
    ) -> Result<Option<Vec<ModelUsage>>, AdminError> {
        match self
            .send(
                reqwest::Method::GET,
                &format!("/admin/api/keys/{key_id}/usage/models?window={window}"),
                None,
            )
            .await
        {
            Ok(value) => serde_json::from_value(value)
                .map(Some)
                .map_err(|error| AdminError::Refused(format!("unexpected usage reply: {error}"))),
            Err(AdminError::Refused(detail)) if detail.contains("no key") => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Points spent inside a window by each of several keys, in one read — the pool read a
    /// member's coworkers share. `Ok(None)` while no reference price is set (the gateway's
    /// 404 with the reason), which the guard treats as "cannot be metered".
    pub async fn points_for_keys_within(
        &self,
        keys: &[String],
        window: &str,
        timeout: std::time::Duration,
    ) -> Result<Option<std::collections::HashMap<String, i64>>, AdminError> {
        match self
            .send_within(
                reqwest::Method::POST,
                "/admin/api/usage/points",
                Some(serde_json::json!({ "keys": keys, "window": window })),
                timeout,
            )
            .await
        {
            Ok(value) => {
                let keys = value
                    .get("keys")
                    .and_then(|k| k.as_object())
                    .map(|map| {
                        map.iter()
                            .map(|(key, points)| (key.clone(), points.as_i64().unwrap_or(0)))
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(Some(keys))
            }
            Err(AdminError::Refused(detail)) if detail.contains("no reference price") => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// The org's budget and month-to-date spend. `None` when the org has no principal yet — which
    /// is not an error, it just means nobody has been given a key.
    pub async fn org_usage(&self, org_id: &str) -> Result<Option<PrincipalUsage>, AdminError> {
        let email = encode_path_segment(&Self::org_principal_email(org_id));
        match self
            .send(
                reqwest::Method::GET,
                &format!("/admin/api/principals/{email}/usage"),
                None,
            )
            .await
        {
            Ok(value) => serde_json::from_value(value)
                .map(Some)
                .map_err(|error| AdminError::Refused(format!("unexpected usage reply: {error}"))),
            // "no principal with that email" is the not-yet-provisioned case, not a failure.
            Err(AdminError::Refused(detail)) if detail.contains("no principal") => Ok(None),
            Err(error) => Err(error),
        }
    }
}

/// RFC 3986 unreserved stays; `@` in the derived principal address must not collapse a
/// per-email path into `GET /admin/api/principals` (the collection, which is 405).
fn encode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char);
            }
            _ => {
                out.push_str(&format!("%{byte:02X}"));
            }
        }
    }
    out
}

fn classify_admin_reply(
    path: &str,
    status: reqwest::StatusCode,
    text: &str,
) -> Result<serde_json::Value, AdminError> {
    if status.is_success() {
        return Ok(serde_json::from_str(text).unwrap_or(serde_json::Value::Null));
    }
    // The gateway's own message, which names the actual problem ("no principal with that
    // email"), beats a status code we would have to guess a sentence for.
    let detail = serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|body| {
            body.get("error")
                .and_then(|e| e.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| format!("HTTP {status}"));
    // METHOD NOT ALLOWED IS AN ANSWER. GET /admin/api/principals is 405 by design (no list).
    // Transport failure is Unreachable; this is Refused, so the console cannot read
    // "gateway down" for a wrong method.
    if status == reqwest::StatusCode::METHOD_NOT_ALLOWED {
        let collection = path
            .split_once('?')
            .map_or(path, |(p, _)| p)
            .trim_end_matches('/');
        let detail = if collection == "/admin/api/principals" {
            format!("{detail}; no GET list of principals — upsert with POST /admin/api/principals")
        } else {
            detail
        };
        return Err(AdminError::Refused(detail));
    }
    Err(AdminError::Refused(detail))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use axum::routing::{get, patch, post};
    use axum::{Json, Router};
    use serde_json::{Value, json};

    #[derive(Default)]
    struct PrincipalLog {
        /// `(method, path, body)` for every principals call. GET of the collection must stay empty.
        calls: Vec<(String, String, Option<Value>)>,
    }

    type SharedLog = Arc<Mutex<PrincipalLog>>;

    /// OAG's principals surface: no GET list (405), POST upsert, PATCH budget, GET usage by email.
    async fn spawn_oag_principals(log: SharedLog) -> String {
        let app = Router::new()
            .route(
                "/admin/api/principals",
                post(
                    |State(log): State<SharedLog>, Json(body): Json<Value>| async move {
                        log.lock().unwrap().calls.push((
                            "POST".into(),
                            "/admin/api/principals".into(),
                            Some(body.clone()),
                        ));
                        (StatusCode::OK, Json(body))
                    },
                )
                .get(|State(log): State<SharedLog>| async move {
                    log.lock().unwrap().calls.push((
                        "GET".into(),
                        "/admin/api/principals".into(),
                        None,
                    ));
                    (
                        StatusCode::METHOD_NOT_ALLOWED,
                        Json(json!({"error": "method not allowed"})),
                    )
                }),
            )
            .route(
                "/admin/api/principals/{email}/budget",
                patch(
                    |State(log): State<SharedLog>,
                     Path(email): Path<String>,
                     Json(body): Json<Value>| async move {
                        log.lock().unwrap().calls.push((
                            "PATCH".into(),
                            format!("/admin/api/principals/{email}/budget"),
                            Some(body.clone()),
                        ));
                        (StatusCode::OK, Json(json!({ "email": email })))
                    },
                ),
            )
            .route(
                "/admin/api/principals/{email}/usage",
                get(
                    |State(log): State<SharedLog>, Path(email): Path<String>| async move {
                        log.lock().unwrap().calls.push((
                            "GET".into(),
                            format!("/admin/api/principals/{email}/usage"),
                            None,
                        ));
                        (
                            StatusCode::OK,
                            Json(json!({
                                "email": email,
                                "monthly_budget_usd": "100.000000",
                                "month_to_date_usd": "1.250000",
                                "requests": 3,
                            })),
                        )
                    },
                ),
            )
            .with_state(log);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        format!("http://127.0.0.1:{}", addr.port())
    }

    #[test]
    fn the_org_principal_address_is_derived_not_stored() {
        let email = GatewayAdmin::org_principal_email("org_01a05");
        assert_eq!(email, "org-org_01a05@gateway.local");
        // Deterministic: the same org resolves to the same principal across restarts, which is why
        // we keep no gateway ids of our own.
        assert_eq!(email, GatewayAdmin::org_principal_email("org_01a05"));
        assert_ne!(email, GatewayAdmin::org_principal_email("org_other"));
    }

    #[test]
    fn debug_never_prints_the_admin_token() {
        let admin = GatewayAdmin {
            base_url: "http://gateway.local:29081".to_string(),
            token: "oag_live_supersecret".to_string(),
            http: reqwest::Client::new(),
        };
        let rendered = format!("{admin:?}");
        assert!(!rendered.contains("supersecret"), "{rendered}");
        assert!(rendered.contains("«redacted»"), "{rendered}");
    }

    #[test]
    fn the_principal_email_is_encoded_in_the_path() {
        assert_eq!(
            encode_path_segment("org-org_01a05@gateway.local"),
            "org-org_01a05%40gateway.local"
        );
    }

    #[test]
    fn a_405_on_the_principals_collection_is_refused_not_unreachable() {
        let error = classify_admin_reply(
            "/admin/api/principals",
            reqwest::StatusCode::METHOD_NOT_ALLOWED,
            r#"{"error":"method not allowed"}"#,
        )
        .expect_err("405 is not success");
        match error {
            AdminError::Refused(detail) => {
                assert!(detail.contains("POST /admin/api/principals"), "{detail}");
                assert!(!detail.to_lowercase().contains("unreachable"), "{detail}");
            }
            AdminError::Unreachable(detail) => {
                panic!("405 must not mean unreachable: {detail}");
            }
        }
        let rendered = format!(
            "{}",
            classify_admin_reply(
                "/admin/api/principals",
                reqwest::StatusCode::METHOD_NOT_ALLOWED,
                "",
            )
            .expect_err("empty 405")
        );
        assert!(rendered.starts_with("the gateway refused:"), "{rendered}");
        assert!(
            !rendered.contains("the gateway is unreachable"),
            "{rendered}"
        );
    }

    #[tokio::test]
    async fn ensure_org_principal_posts_upsert_and_never_gets_the_collection() {
        let log: SharedLog = Arc::new(Mutex::new(PrincipalLog::default()));
        let base = spawn_oag_principals(log.clone()).await;
        let admin = GatewayAdmin::new(&base, "admin-token");

        admin
            .ensure_org_principal("org_01a05", None)
            .await
            .expect("upsert");

        let calls = log.lock().unwrap().calls.clone();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0].0, "POST");
        assert_eq!(calls[0].1, "/admin/api/principals");
        let body = calls[0].2.as_ref().expect("json body");
        assert_eq!(body["email"], json!("org-org_01a05@gateway.local"));
        assert_eq!(body["role"], json!("member"));
        assert!(
            body.get("monthly_budget_usd").is_none(),
            "omit the budget to leave it (COALESCE): {body}"
        );
        assert!(
            calls.iter().all(|(method, path, _)| {
                !(method == "GET" && path.trim_end_matches('/') == "/admin/api/principals")
            }),
            "never GET the collection: {calls:?}"
        );
    }

    #[tokio::test]
    async fn ensure_org_principal_sends_the_budget_when_set() {
        let log: SharedLog = Arc::new(Mutex::new(PrincipalLog::default()));
        let base = spawn_oag_principals(log.clone()).await;
        let admin = GatewayAdmin::new(&base, "admin-token");

        admin
            .ensure_org_principal("org_01a05", Some("100"))
            .await
            .expect("upsert");

        let body = log.lock().unwrap().calls[0].2.clone().expect("body");
        assert_eq!(body["monthly_budget_usd"], json!("100"));
        assert_eq!(body["role"], json!("member"));
    }

    #[tokio::test]
    async fn org_usage_and_budget_are_by_email_not_the_collection() {
        let log: SharedLog = Arc::new(Mutex::new(PrincipalLog::default()));
        let base = spawn_oag_principals(log.clone()).await;
        let admin = GatewayAdmin::new(&base, "admin-token");

        let usage = admin.org_usage("org_01a05").await.expect("usage");
        let usage = usage.expect("provisioned");
        assert_eq!(usage.email, "org-org_01a05@gateway.local");
        assert_eq!(usage.month_to_date_usd, "1.250000");

        admin
            .set_org_budget("org_01a05", Some("50"))
            .await
            .expect("budget");

        let calls = log.lock().unwrap().calls.clone();
        assert_eq!(calls[0].0, "GET");
        assert_eq!(
            calls[0].1,
            "/admin/api/principals/org-org_01a05@gateway.local/usage"
        );
        assert_eq!(calls[1].0, "PATCH");
        assert_eq!(
            calls[1].1,
            "/admin/api/principals/org-org_01a05@gateway.local/budget"
        );
        assert!(
            calls.iter().all(|(method, path, _)| {
                !(method == "GET" && path.trim_end_matches('/') == "/admin/api/principals")
            }),
            "never GET the collection: {calls:?}"
        );
    }

    #[tokio::test]
    async fn a_get_of_the_principals_collection_is_refused_not_unreachable() {
        let log: SharedLog = Arc::new(Mutex::new(PrincipalLog::default()));
        let base = spawn_oag_principals(log.clone()).await;
        let admin = GatewayAdmin::new(&base, "admin-token");

        let error = admin
            .get_principals_collection()
            .await
            .expect_err("GET collection is 405");
        let rendered = format!("{error}");
        match error {
            AdminError::Refused(detail) => {
                assert!(detail.contains("POST /admin/api/principals"), "{detail}");
            }
            AdminError::Unreachable(detail) => {
                panic!("405 must not mean unreachable: {detail}");
            }
        }
        assert!(rendered.starts_with("the gateway refused:"), "{rendered}");
        assert!(
            !rendered.contains("the gateway is unreachable"),
            "{rendered}"
        );
        assert_eq!(log.lock().unwrap().calls[0].0, "GET");
    }

    #[tokio::test]
    async fn a_closed_port_is_unreachable() {
        let admin = GatewayAdmin::new("http://127.0.0.1:1", "admin-token");
        let error = admin
            .ensure_org_principal("org_01a05", None)
            .await
            .expect_err("nothing listens on :1");
        assert!(matches!(error, AdminError::Unreachable(_)), "{error:?}");
        let rendered = format!("{error}");
        assert!(
            rendered.starts_with("the gateway is unreachable:"),
            "{rendered}"
        );
    }
}
