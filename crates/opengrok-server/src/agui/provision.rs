//! The account's ONE shared computer, and its teardown.
//!
//! 1 account = 1 computer (`docs/plan-bots-computers-channels.md`). The account's first agent
//! creates the box; every later agent of that account SHARES it — provisioning is automatic, with
//! no per-agent choice and no "connect" step. Deleting an agent does not destroy the box (others
//! may share it); when the account's LAST agent is deleted, the box is destroyed and the mapping
//! cleared, so nothing runs unused.
//!
//! A provisioning failure is never fatal to the hire: a boxless coworker is still a coworker, and
//! the reason is reported for the client to show.

use std::sync::Arc;

use opengrok_box::Computer;
use opengrok_core::coworker::{BoxMode, Coworker, CoworkerCommand, CoworkerEvent};
use opengrok_core::id::{AccountId, BoxId};

use serde_json::{Value, json};

use crate::agui::AgUiState;

/// The org that owns an account, if any — the key to which computer credentials apply.
async fn account_org(state: &AgUiState, account_id: &AccountId) -> Option<String> {
    let (account, _) = state.auth.store.load_account(account_id).await.ok()?;
    account.org_id
}

/// The provider for a computer of `kind` in this org: an AsciiBoxes built from the org's sealed
/// box.ascii.dev key for `"ascii"`, or a fresh server-host Docker for `"local-docker"`. `None`
/// when the kind cannot be served (e.g. `"ascii"` but the org has no key or the vault is absent).
/// The SAME provider must create and run a box, so both paths call this.
pub async fn provider_for(
    state: &AgUiState,
    org_id: Option<&str>,
    kind: &str,
) -> Option<Arc<dyn Computer>> {
    match kind {
        "ascii" => {
            let vault = state.vault.as_ref()?;
            let org = org_id?;
            let key = state
                .auth
                .store
                .org_computer_secret(vault, org, "ascii")
                .await
                .ok()
                .flatten()?;
            Some(Arc::new(opengrok_box::AsciiBoxes::new(key)))
        }
        "local-docker" => Some(Arc::new(opengrok_box::DockerComputer::new())),
        _ => None,
    }
}

/// The provider for an account's existing computer of `kind`, resolving the account's org itself.
/// The run path uses this so tools execute on the same provider that created the box.
pub async fn provider_for_account(
    state: &AgUiState,
    account_id: &AccountId,
    kind: &str,
) -> Option<Arc<dyn Computer>> {
    let org_id = account_org(state, account_id).await;
    provider_for(state, org_id.as_deref(), kind).await
}

/// Local VM (server-host Docker) is a SELF-HOST / dev convenience only. A hosted, multi-tenant
/// deployment (`OG_HOSTED=1`) must never run untrusted bot containers on the API host — a container
/// escape lands on the machine holding the token secret and the org vault — so it is neither
/// advertised nor used there; production computers are box.ascii.dev / Windows 365 / (later) cloud.
pub fn local_docker_allowed() -> bool {
    std::env::var("OG_HOSTED").as_deref() != Ok("1")
}

/// The kind a NEW account computer should be, from the org's CURRENT config: a box.ascii.dev box
/// when the org has configured a key; else a Local VM on the server host when allowed (dev /
/// self-host); else `"none"` — no provider, and the hire says so readably.
pub async fn kind_for_new(state: &AgUiState, org_id: Option<&str>) -> &'static str {
    if let (Some(vault), Some(org)) = (state.vault.as_ref(), org_id)
        && state
            .auth
            .store
            .org_computer_secret(vault, org, "ascii")
            .await
            .ok()
            .flatten()
            .is_some()
    {
        "ascii"
    } else if local_docker_allowed() {
        "local-docker"
    } else {
        "none"
    }
}

/// The outcome of assigning the account's computer to a freshly-hired coworker.
pub struct Provisioned {
    /// The `ComputerAssigned` events to persist alongside the hire (empty when none/failed).
    pub events: Vec<CoworkerEvent>,
    /// The account's box id, for the coworker view (`None` when none/failed).
    pub box_id: Option<BoxId>,
    /// Why the box could not be given, as (code, message) — never fatal to the hire. `code` is one
    /// of the seven stable codes; `message` is the human-readable reason.
    pub error: Option<(String, String)>,
}

/// Record a provisioning failure at the ACCOUNT level (so a boxless account can say why before any
/// agent exists) and return it. Never fatal — the hire stands, boxless.
async fn record_error(
    state: &AgUiState,
    account_id: &AccountId,
    code: &str,
    message: &str,
    at_ms: i64,
) -> Provisioned {
    let _ = state
        .auth
        .store
        .set_account_computer_error(account_id.as_str(), code, message, at_ms)
        .await;
    Provisioned {
        events: Vec::new(),
        box_id: None,
        error: Some((code.to_string(), message.to_string())),
    }
}

/// Render a provisioning error as the client contract `{code, message}`, or null when there is
/// none. The same shape on every surface: create responses, listOpenGrokComputers, and agent rows.
pub fn error_json(error: &Option<(String, String)>) -> Value {
    match error {
        Some((code, message)) => json!({ "code": code, "message": message }),
        None => Value::Null,
    }
}

/// Ensure the account has its one computer and assign it (shared) to `coworker`. The account's
/// first agent creates the box and records it; every later agent reuses the same box. Applies the
/// assignment to `coworker`; returns the events to persist, the box id, and any error. Never raises.
pub async fn ensure_account_computer(
    state: &AgUiState,
    account_id: &AccountId,
    coworker: &mut Coworker,
    at_ms: i64,
) -> Provisioned {
    let store = &state.auth.store;
    // Reuse the account's existing computer, or create it on this — the account's first — agent,
    // as a box.ascii.dev box when the org has a key, else a Local VM on the server.
    let box_id = match store.account_computer(account_id.as_str()).await {
        Ok(Some((box_id, _kind))) => box_id,
        Ok(None) => {
            let org_id = account_org(state, account_id).await;
            let kind = kind_for_new(state, org_id.as_deref()).await;
            let Some(provider) = provider_for(state, org_id.as_deref(), kind).await else {
                let code = if kind == "none" {
                    "no_org_key"
                } else {
                    "not_supported"
                };
                return record_error(
                    state,
                    account_id,
                    code,
                    "no computer is configured for your organization — an admin must set up box.ascii.dev on the dashboard",
                    at_ms,
                )
                .await;
            };
            match provider.create(None).await {
                Ok(box_id) => {
                    if let Err(error) = store
                        .set_account_computer(account_id.as_str(), &box_id, kind, at_ms)
                        .await
                    {
                        return record_error(
                            state,
                            account_id,
                            "unknown",
                            &error.to_string(),
                            at_ms,
                        )
                        .await;
                    }
                    box_id
                }
                // The box provider named its own failure; map it to a stable code.
                Err(error) => {
                    return record_error(
                        state,
                        account_id,
                        error.code(),
                        &error.to_string(),
                        at_ms,
                    )
                    .await;
                }
            }
        }
        Err(error) => {
            return record_error(state, account_id, "unknown", &error.to_string(), at_ms).await;
        }
    };

    // Every agent shares the account's one box.
    let box_id = BoxId::from_stored(box_id);
    match coworker.decide(CoworkerCommand::AssignComputer {
        box_id: box_id.clone(),
        mode: BoxMode::Shared,
        at_ms,
    }) {
        Ok(events) => {
            for event in &events {
                coworker.apply(event);
            }
            // The account has a computer now — clear any stale provisioning error.
            let _ = store
                .clear_account_computer_error(account_id.as_str())
                .await;
            Provisioned {
                events,
                box_id: Some(box_id),
                error: None,
            }
        }
        Err(error) => record_error(state, account_id, "unknown", &error.to_string(), at_ms).await,
    }
}

/// Tear down the account's computer once its LAST agent is gone: if no non-retired coworkers remain
/// for the account, destroy the box (best-effort) and clear the mapping. A no-op while any agent
/// still shares it. Call AFTER the deleted agent's retirement is persisted, so the count is current.
pub async fn teardown_account_computer_if_last(state: &AgUiState, account_id: &AccountId) {
    let store = &state.auth.store;
    match store.coworkers_for(account_id).await {
        // Still has at least one agent — keep the shared box.
        Ok(rows) if !rows.is_empty() => return,
        // Cannot tell (read error) — leave the box rather than destroy on uncertainty.
        Err(_) => return,
        Ok(_) => {}
    }
    let Ok(Some((box_id, kind))) = store.account_computer(account_id.as_str()).await else {
        return;
    };
    // Destroy on the SAME provider that made it (by its recorded kind). Best-effort: a box that
    // will not die must not block clearing the mapping, but we log it so a leak is visible.
    let org_id = account_org(state, account_id).await;
    if let Some(provider) = provider_for(state, org_id.as_deref(), &kind).await
        && let Err(error) = provider.destroy(&box_id).await
    {
        tracing::warn!(%error, box_id, "could not destroy the account's box on last-agent delete; clearing the mapping anyway");
    }
    let _ = store.clear_account_computer(account_id.as_str()).await;
}
