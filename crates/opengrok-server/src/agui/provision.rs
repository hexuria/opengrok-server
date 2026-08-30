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
use opengrok_store::PgStore;

/// The outcome of assigning the account's computer to a freshly-hired coworker.
pub struct Provisioned {
    /// The `ComputerAssigned` events to persist alongside the hire (empty when none/failed).
    pub events: Vec<CoworkerEvent>,
    /// The account's box id, for the coworker view (`None` when none/failed).
    pub box_id: Option<BoxId>,
    /// A client-readable reason the box could not be given — never fatal to the hire.
    pub error: Option<String>,
}

fn failed(error: impl Into<String>) -> Provisioned {
    Provisioned {
        events: Vec::new(),
        box_id: None,
        error: Some(error.into()),
    }
}

/// Ensure the account has its one computer and assign it (shared) to `coworker`. The account's
/// first agent creates the box and records it; every later agent reuses the same box. Applies the
/// assignment to `coworker`; returns the events to persist, the box id, and any error. Never raises.
pub async fn ensure_account_computer(
    computer: Option<&Arc<dyn Computer>>,
    store: &PgStore,
    account_id: &AccountId,
    coworker: &mut Coworker,
    at_ms: i64,
) -> Provisioned {
    // Reuse the account's existing computer, or create it on this — the account's first — agent.
    let box_id = match store.account_computer(account_id.as_str()).await {
        Ok(Some((box_id, _kind))) => box_id,
        Ok(None) => {
            let Some(computer) = computer else {
                return failed("this server has no computer provider configured");
            };
            match computer.create(None).await {
                Ok(box_id) => {
                    if let Err(error) = store
                        .set_account_computer(account_id.as_str(), &box_id, computer.kind(), at_ms)
                        .await
                    {
                        return failed(error.to_string());
                    }
                    box_id
                }
                Err(error) => return failed(error.to_string()),
            }
        }
        Err(error) => return failed(error.to_string()),
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
            Provisioned {
                events,
                box_id: Some(box_id),
                error: None,
            }
        }
        Err(error) => failed(error.to_string()),
    }
}

/// Tear down the account's computer once its LAST agent is gone: if no non-retired coworkers remain
/// for the account, destroy the box (best-effort) and clear the mapping. A no-op while any agent
/// still shares it. Call AFTER the deleted agent's retirement is persisted, so the count is current.
pub async fn teardown_account_computer_if_last(
    computer: Option<&Arc<dyn Computer>>,
    store: &PgStore,
    account_id: &AccountId,
) {
    match store.coworkers_for(account_id).await {
        // Still has at least one agent — keep the shared box.
        Ok(rows) if !rows.is_empty() => return,
        // Cannot tell (read error) — leave the box rather than destroy on uncertainty.
        Err(_) => return,
        Ok(_) => {}
    }
    let Ok(Some((box_id, _kind))) = store.account_computer(account_id.as_str()).await else {
        return;
    };
    // Best-effort destroy: a box that will not die must not block clearing the mapping, but we log
    // it so a leak is visible.
    if let Some(computer) = computer
        && let Err(error) = computer.destroy(&box_id).await
    {
        tracing::warn!(%error, box_id, "could not destroy the account's box on last-agent delete; clearing the mapping anyway");
    }
    let _ = store.clear_account_computer(account_id.as_str()).await;
}
