//! Bridges server-owned AG-UI state because integrations cannot depend on it.

use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_integrations::plugin_desk::PluginCeiling;
use opengrok_integrations::registry::Registry;

use crate::agui::AgUiState;

pub fn configured(
    state: &AgUiState,
    registry: Option<Registry>,
) -> opengrok_integrations::plugin_desk::Tools {
    opengrok_integrations::plugin_desk::Tools {
        store: state.auth.store.clone(),
        reserved_names: crate::plugin_registry::reserved_names(state),
        ceiling: std::sync::Arc::new(Ceiling(state.clone())),
        registry,
    }
}

pub struct Ceiling(pub AgUiState);

#[async_trait::async_trait]
impl PluginCeiling for Ceiling {
    async fn set_plugin(
        &self,
        account: &AccountId,
        coworker: &CoworkerId,
        plugin: &str,
        on: bool,
    ) -> Result<bool, String> {
        crate::agui::ceiling::set_plugin(&self.0, account, coworker, plugin, on).await
    }
}
