//! The server adapter for the integrations plugin desk.

use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_integrations::plugin_desk::PluginCeiling;
use opengrok_integrations::registry::Registry;
use opengrok_tools::ToolContext;
use opengrok_tools::plugin_desk::{Ask, PluginDesk};
use serde_json::Value;

use crate::agui::AgUiState;

pub struct Tools {
    pub state: AgUiState,
    pub registry: Option<Registry>,
}

pub struct ConfiguredTools {
    inner: opengrok_integrations::plugin_desk::Tools,
}

pub fn configured(state: &AgUiState, registry: Option<Registry>) -> ConfiguredTools {
    ConfiguredTools {
        inner: tools(state, registry),
    }
}

fn tools(
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

#[async_trait::async_trait]
impl PluginDesk for ConfiguredTools {
    async fn answer(&self, context: &ToolContext, ask: Ask) -> Result<Value, String> {
        self.inner.answer(context, ask).await
    }

    async fn ask_first(&self, context: &ToolContext, ask: &Ask) -> Result<Option<String>, String> {
        self.inner.ask_first(context, ask).await
    }
}

#[async_trait::async_trait]
impl PluginDesk for Tools {
    async fn answer(&self, context: &ToolContext, ask: Ask) -> Result<Value, String> {
        tools(&self.state, self.registry.clone())
            .answer(context, ask)
            .await
    }

    async fn ask_first(&self, context: &ToolContext, ask: &Ask) -> Result<Option<String>, String> {
        tools(&self.state, self.registry.clone())
            .ask_first(context, ask)
            .await
    }
}
