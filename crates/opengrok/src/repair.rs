//! The boot pass over the boxes #302's race left running: reported on every boot, destroyed
//! only when asked. A boot chore like `kek::check_at_boot`, so it lives with the boot.

use opengrok_core::coworker::BoxMode;
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_server::agui::AgUiState;
use opengrok_server::agui::provision::{ScopedBoxRow, provider_for, scoped_box_row_for};

/// Whether the pass destroys what it finds: only when `OG_REPAIR_STRAY_BOXES` is exactly
/// `destroy`. A destroy takes a box's files with it, so a first boot on a real database reports.
pub fn destroys(setting: Option<&str>) -> bool {
    setting == Some("destroy")
}

/// The boxes #302's race left running, and nothing else: a box no computer row records, every
/// coworker naming it on a SHARED box of an account or org scope whose row names another. A box
/// recorded only on coworker rows is not one: from `817ec33` to `5c0d141` (29-31 Aug 2026) a
/// hire's box, dedicated or named by the client, was recorded nowhere else, and no
/// `account_computer` row was carried into `scoped_computer`. Each is asked of its scope's
/// provider and, while that provider still has it, logged at warn, and destroyed only when
/// `destroy` (`destroys`). Answers the boxes still there.
pub async fn stray_boxes(state: &AgUiState, destroy: bool) -> Vec<String> {
    let unrecorded = state.auth.store.unrecorded_coworker_boxes().await;
    let unrecorded = unrecorded.unwrap_or_else(|error| {
        tracing::warn!(%error, "computer: could not look for boxes no record names");
        Vec::new()
    });
    let mut found = Vec::new();
    for namers in unrecorded.chunk_by(|one, two| one.0 == two.0) {
        let box_id = &namers[0].0;
        let mut row = None;
        for (_, coworker, account) in namers {
            row = shaped_like_302(state, account, coworker, box_id).await;
            if row.is_none() {
                break;
            }
        }
        let Some(row) = row else { continue };
        let Some(provider) = provider_for(state, row.org_id.as_deref(), &row.kind).await else {
            continue;
        };
        match provider.state(box_id).await.as_deref() {
            Ok("absent") => continue,
            Ok(_) => found.push(box_id.clone()),
            Err(error) => {
                tracing::warn!(box_id, %error, "computer: could not ask after a box no record names");
                continue;
            }
        }
        let (scope, scope_id, kept) = (row.scope, &row.scope_id, &row.box_id);
        if !destroy {
            tracing::warn!(box_id, scope, scope_id, kept = %kept, "computer: a box no record names is running; OG_REPAIR_STRAY_BOXES=destroy destroys it");
        } else if let Err(error) = provider.destroy(box_id).await {
            tracing::warn!(box_id, scope, scope_id, %error, "computer: a box no record names could not be destroyed");
        } else {
            tracing::warn!(box_id, scope, scope_id, kept = %kept, "computer: destroyed a box no record names");
        }
    }
    found
}

/// The scope row of a coworker standing where #302 left one: on a SHARED box of an account or org
/// scope whose row names a box other than `stray`. Any other coworker is `None`.
async fn shaped_like_302(
    state: &AgUiState,
    account: &str,
    coworker: &str,
    stray: &str,
) -> Option<ScopedBoxRow> {
    let coworker = CoworkerId::from_stored(coworker.to_string());
    let (loaded, _) = state.auth.store.load_coworker(&coworker).await.ok()?;
    let account = AccountId::from_stored(account.to_string());
    let row = scoped_box_row_for(state, &account, &coworker).await?;
    let shared = loaded.box_mode() == Some(BoxMode::Shared);
    (shared && matches!(row.scope, "account" | "org") && row.box_id != stray).then_some(row)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
#[path = "../tests/unit/repair.rs"]
mod tests;
