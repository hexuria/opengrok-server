//! Save-for-next-time after a site login: the `credential.offer_save` CUSTOM.
//!
//! WHY THIS IS NOT THE VAULT. Connector credentials stay in `opengrok-store::Vault`. A site
//! password must never be stored, journaled, or shown to the model. The saved login lives in
//! NativeChat, which offers it on the `request_user_form` card and types it into the page
//! out of the model's view; the server only ever learns origin and username.

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
    _agent_id: &str,
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
        frame,
    )
    .await;
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use opengrok_tools::credential::offer_save_payload;

    #[test]
    fn offer_save_payload_is_origin_username_and_entry_only() {
        let payload = offer_save_payload("example.com", "ada", "e_1");
        assert_eq!(payload["origin"], "example.com");
        assert_eq!(payload["username"], "ada");
        assert_eq!(payload["formEntryId"], "e_1");
        assert_eq!(payload.as_object().map(|o| o.len()), Some(3));
    }
}
