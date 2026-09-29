//! Save-for-next-time after a site login: the `credential.offer_save` CUSTOM.
//!
//! The frame carries origin, username and the form's entry id, never a password. A saved
//! login lives in the person's vault (`site_login`, sealed in `secret_store`, opened only for
//! their own app) and in their Mac's keychain; NativeChat offers it on the `request_user_form`
//! card and the fill types it into the page out of the model's view. Nothing about it is
//! journaled or shown to the model.

use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_tools::credential::{origin_from_form, username_from_shared};
use opengrok_tools::user_form::FormRequest;
use std::collections::BTreeMap;

use super::user_form::journal_agui_custom;
use crate::host_state::HostState;

/// After a successful user-form fill, ask NativeChat to save origin+username. Never a password.
pub async fn offer_save_after_submit(
    state: &HostState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    form_call_id: Option<&str>,
    form_entry_id: &str,
    form: &FormRequest,
    shared: &BTreeMap<String, String>,
) {
    let Some(origin) = origin_from_form(form) else {
        return;
    };
    let username = username_from_shared(shared);
    let frame = opengrok_tools::credential::offer_save_frame(&origin, &username, form_entry_id);
    journal_agui_custom(
        state,
        account_id,
        coworker_id,
        opengrok_core::run::SuspendReason::UserForm,
        form_call_id,
        frame,
    )
    .await;
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
#[path = "../../tests/unit/credential.rs"]
mod tests;
