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
use opengrok_core::id::{AccountId, BoxId, CoworkerId};

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
/// The effective sharing mode for an account and its org: the account's OVERRIDE if set, else the
/// org DEFAULT, else the built-in default (per-account). Returns (mode, org_id).
pub async fn resolve_mode(state: &AgUiState, account_id: &AccountId) -> (String, Option<String>) {
    let org_id = account_org(state, account_id).await;
    if let Ok(Some(mode)) = state
        .auth
        .store
        .sharing_mode("account", account_id.as_str())
        .await
    {
        return (mode, org_id);
    }
    if let Some(org) = &org_id
        && let Ok(Some(mode)) = state.auth.store.sharing_mode("org", org).await
    {
        return (mode, org_id);
    }
    ("per-account".to_string(), org_id)
}

/// The (scope, scope_id, box mode) a mode maps to: per-org shares one org box, per-account one box
/// per member, per-bot a dedicated box each. An account with no org falls back to account scope.
pub fn scope_for(
    mode: &str,
    account_id: &str,
    org_id: Option<&str>,
    coworker_id: &str,
) -> (&'static str, String, BoxMode) {
    match mode {
        "per-org" => match org_id {
            Some(org) => ("org", org.to_string(), BoxMode::Shared),
            None => ("account", account_id.to_string(), BoxMode::Shared),
        },
        "per-bot" => ("bot", coworker_id.to_string(), BoxMode::Dedicated),
        _ => ("account", account_id.to_string(), BoxMode::Shared),
    }
}

/// Ensure the computer for a coworker under its account's effective sharing mode, and assign it.
/// per-org: the whole org shares one box; per-account: one box per member; per-bot: a dedicated box.
/// The box is created on first need for its scope and reused after. Non-fatal — a failure leaves a
/// boxless coworker carrying the account-level error.
pub async fn ensure_computer_for(
    state: &AgUiState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    coworker: &mut Coworker,
    at_ms: i64,
) -> Provisioned {
    let store = &state.auth.store;
    let (mode, org_id) = resolve_mode(state, account_id).await;
    let (scope, scope_id, box_mode) = scope_for(
        &mode,
        account_id.as_str(),
        org_id.as_deref(),
        coworker_id.as_str(),
    );

    let box_id = match store.scoped_computer(scope, &scope_id).await {
        Ok(Some((box_id, _kind))) => box_id,
        Ok(None) => {
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
                        .set_scoped_computer(scope, &scope_id, &box_id, kind, at_ms)
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

    let box_id = BoxId::from_stored(box_id);
    match coworker.decide(CoworkerCommand::AssignComputer {
        box_id: box_id.clone(),
        mode: box_mode,
        at_ms,
    }) {
        Ok(events) => {
            for event in &events {
                coworker.apply(event);
            }
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

/// Destroy a scope's box (best-effort, on the provider that made it) and clear its mapping.
async fn destroy_and_clear(
    state: &AgUiState,
    org_id: Option<&str>,
    scope: &str,
    scope_id: &str,
    box_id: &str,
    kind: &str,
) {
    if let Some(provider) = provider_for(state, org_id, kind).await
        && let Err(error) = provider.destroy(box_id).await
    {
        tracing::warn!(%error, box_id, "could not destroy a box on teardown; clearing the mapping anyway");
    }
    let _ = state
        .auth
        .store
        .clear_scoped_computer(scope, scope_id)
        .await;
}

/// Tear down a deleted coworker's computer according to its account's mode. per-bot: destroy the
/// bot's own box. per-account: destroy the account box once the account's last agent is gone.
/// per-org: leave the shared org box (idle-stop / an admin manages its lifetime). Call AFTER the
/// deletion is persisted.
pub async fn teardown_computer_for(
    state: &AgUiState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
) {
    let store = &state.auth.store;
    let (mode, org_id) = resolve_mode(state, account_id).await;
    match mode.as_str() {
        "per-bot" => {
            if let Ok(Some((box_id, kind))) =
                store.scoped_computer("bot", coworker_id.as_str()).await
            {
                destroy_and_clear(
                    state,
                    org_id.as_deref(),
                    "bot",
                    coworker_id.as_str(),
                    &box_id,
                    &kind,
                )
                .await;
            }
        }
        // The org box is shared org-wide; a single member's delete must not pull it out from under
        // everyone. Its lifetime is idle-stop and admin action, not agent deletion.
        "per-org" => {}
        _ => {
            let empty = store
                .coworkers_for(account_id)
                .await
                .map(|rows| rows.is_empty())
                .unwrap_or(false);
            if empty
                && let Ok(Some((box_id, kind))) =
                    store.scoped_computer("account", account_id.as_str()).await
            {
                destroy_and_clear(
                    state,
                    org_id.as_deref(),
                    "account",
                    account_id.as_str(),
                    &box_id,
                    &kind,
                )
                .await;
            }
        }
    }
}
