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
#[derive(Debug, Clone, Deserialize)]
pub struct PrincipalUsage {
    pub email: String,
    #[serde(rename = "monthly_budget_usd")]
    pub monthly_budget_usd: Option<String>,
    #[serde(rename = "month_to_date_usd")]
    pub month_to_date_usd: String,
    pub requests: i64,
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
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .timeout(std::time::Duration::from_secs(15));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(|error| AdminError::Unreachable(error.to_string()))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            // The gateway's own message, which names the actual problem ("no principal with that
            // email"), beats a status code we would have to guess a sentence for.
            let detail = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|body| {
                    body.get("error")
                        .and_then(|e| e.as_str())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| format!("HTTP {status}"));
            return Err(AdminError::Refused(detail));
        }
        Ok(serde_json::from_str(&text).unwrap_or(serde_json::Value::Null))
    }

    /// Bind the org to its principal, optionally setting the org's monthly budget. Idempotent, so
    /// it is safe (and correct) to call before every mint rather than tracking whether we have.
    pub async fn ensure_org_principal(
        &self,
        org_id: &str,
        monthly_budget_usd: Option<&str>,
    ) -> Result<(), AdminError> {
        let mut body = serde_json::json!({ "email": Self::org_principal_email(org_id) });
        if let Some(budget) = monthly_budget_usd {
            body["monthly_budget_usd"] = serde_json::Value::String(budget.to_string());
        }
        self.send(reqwest::Method::POST, "/admin/api/principals", Some(body))
            .await
            .map(|_| ())
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
        let email = Self::org_principal_email(org_id);
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

    /// The org's budget and month-to-date spend. `None` when the org has no principal yet — which
    /// is not an error, it just means nobody has been given a key.
    pub async fn org_usage(&self, org_id: &str) -> Result<Option<PrincipalUsage>, AdminError> {
        let email = Self::org_principal_email(org_id);
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

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
}
