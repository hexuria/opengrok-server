//! Bot keys: the credential that names a coworker.
//!
//! A bot key hard-binds one account to one coworker (`sub` + `coworker`) and is the bearer the
//! MCP door accepts. It is signed like everything else but LONG-lived: its real lifecycle is the
//! revocable row (`bot_key_view`), and the `exp` only bounds a leaked key whose row was somehow
//! lost with it. Minted in two places — `POST /coworkers/{id}/keys` (shown once to the person)
//! and the MCP door's OAuth token endpoint (handed to Claude Code as the access token) — so the
//! claims and the mint live here, once.

use opengrok_core::id::{AccountId, CoworkerId};
use serde::{Deserialize, Serialize};

use super::TokenMinter;

/// The claims a bot key carries. `aud` is set on keys the OAuth door mints (RFC 8707: the token
/// is for THIS resource) and absent on hand-minted ones, which predate it and are ours.
#[derive(Debug, Serialize, Deserialize)]
pub struct BotKeyClaims {
    #[serde(rename = "use")]
    pub purpose: String,
    pub sub: String,
    pub coworker: String,
    pub jti: String,
    pub exp: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aud: Option<String>,
}

/// Ten years: the row is the real lifecycle.
pub const HAND_MINTED_TTL_SECS: i64 = 10 * 365 * 24 * 60 * 60;

/// A minted bot key: the row's id and the token itself, which exists in a reply exactly once.
pub struct MintedBotKey {
    pub jti: String,
    pub token: String,
}

/// Mint and record a bot key for (account, coworker). The caller has already checked that the
/// account owns the coworker.
pub async fn mint(
    store: &opengrok_store::PgStore,
    minter: &TokenMinter,
    account: &AccountId,
    coworker: &CoworkerId,
    label: &str,
    aud: Option<&str>,
    ttl_secs: i64,
) -> Result<MintedBotKey, String> {
    let jti = format!("bk_{}", uuid::Uuid::now_v7());
    let at_ms = chrono::Utc::now().timestamp_millis();
    let claims = BotKeyClaims {
        purpose: "bot-key".to_string(),
        sub: account.as_str().to_string(),
        coworker: coworker.as_str().to_string(),
        jti: jti.clone(),
        exp: at_ms / 1_000 + ttl_secs,
        aud: aud.map(str::to_string),
    };
    let token = minter
        .mint_claims(&claims)
        .map_err(|error| format!("could not mint: {error}"))?;
    store
        .insert_bot_key(&jti, account, coworker, label, at_ms)
        .await
        .map_err(|error| format!("could not record the key: {error}"))?;
    Ok(MintedBotKey { jti, token })
}
