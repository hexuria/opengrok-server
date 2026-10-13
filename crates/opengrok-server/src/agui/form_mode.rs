//! Form mode (#220, Tier 1): a card the chat box drives, whose boxes a plugin holds.
//!
//! The plugin's `extensions.formMode` names its tools (`opengrok_plugins::form_mode`); this module
//! is where the server acts on them: the tools that always ask, the card a `request_user_form`
//! with `formMode` mints without parking its run, and the card's actions, which a person's press
//! runs through the same executor and gates as a model's call, with no turn in between.

use opengrok_core::id::{AccountId, CoworkerId};

use super::AgUiState;

/// `policy` with every form-mode plugin's `alwaysAsk` tools held for a person's yes. Read on
/// every turn and every door, like the grant it narrows, so a Bot's model can never send a tax
/// return, or a queued one early, without somebody saying yes.
pub(crate) async fn always_asking(
    state: &AgUiState,
    account: &AccountId,
    coworker: &CoworkerId,
    policy: opengrok_policy::Context,
) -> opengrok_policy::Context {
    let store = &state.auth.store;
    let plugins =
        opengrok_integrations::turn::form_mode_plugins(store, account, coworker, &state.plugins);
    let tools: Vec<String> = plugins
        .await
        .iter()
        .flat_map(|(plugin, tools)| tools.always_ask_tools(plugin))
        .collect();
    opengrok_policy::with_always_ask(policy, &tools)
}
