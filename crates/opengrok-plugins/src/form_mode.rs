//! `extensions.formMode` in a plugin's `plugin.json`: which of its tools build and answer a
//! form-mode card, and which of its tools always need a person's yes.
//!
//! THE MANIFEST NAMES THE TOOLS, NEVER THE MODEL. A card's Save runs on every blur; if the model
//! could say which tool "save" is, it could map it to submit, and a blur would send a tax return.
//! So the card tool and each action's tool are read from here and nowhere else.
//!
//! Transcribed from the contract agreed for hexuria/opengrok #220 (Tier 1), which the BIR plugin
//! (hexuria/buwiz-forms `integrations/opengrok/bir/plugin.json`) ships:
//!
//! ```json
//! { "extensions": { "formMode": {
//!     "server": "bir-mcp",
//!     "card":   "bir_form_card",
//!     "actions": { "save": "bir_form_save", "submit": "bir_form_submit",
//!                  "view": "bir_form_preview", "focus": "bir_form_focus",
//!                  "refresh": "bir_form_card" },
//!     "alwaysAsk": ["bir_form_submit", "bir_queue_send_now"]
//! } } }
//! ```

use std::collections::BTreeMap;

use serde_json::Value;

use crate::Manifest;

/// The card actions a person's press can ask for. Anything else in `actions` is ignored, so a
/// manifest cannot add a verb the server never reviewed.
pub const ACTIONS: [&str; 5] = ["save", "submit", "view", "focus", "refresh"];

/// A plugin's `extensions.formMode`, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormModeTools {
    /// The plugin's MCP server the tools below are on.
    pub server: String,
    /// The tool that answers a CardReply for a form.
    pub card: String,
    /// By action (`ACTIONS`): the tool a press runs.
    pub actions: BTreeMap<String, String>,
    /// Tools that ask a person first, whatever the plugin's trust or the Bot's grant says.
    pub always_ask: Vec<String>,
}

/// A remote tool or server name we will put into `<plugin>.<server>.<tool>`: a dot would read
/// as another server's, and anything outside this set is not a name an MCP server gives.
fn usable(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

impl FormModeTools {
    /// `extensions.formMode`, or `None` when it is absent or unusable. A half-usable block is no
    /// block: a card with a Save that goes nowhere is worse than no card.
    #[must_use]
    pub fn from_extensions(extensions: Option<&Value>) -> Option<Self> {
        let block = extensions?.get("formMode")?;
        let text = |key: &str| {
            block
                .get(key)
                .and_then(Value::as_str)
                .filter(|name| usable(name))
                .map(str::to_string)
        };
        let server = text("server")?;
        let card = text("card")?;
        let mut actions = BTreeMap::new();
        for (action, tool) in block.get("actions")?.as_object()? {
            if !ACTIONS.contains(&action.as_str()) {
                continue;
            }
            let tool = tool.as_str().filter(|name| usable(name))?;
            actions.insert(action.clone(), tool.to_string());
        }
        let always_ask = block
            .get("alwaysAsk")
            .and_then(Value::as_array)
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|name| usable(name))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Some(Self {
            server,
            card,
            actions,
            always_ask,
        })
    }

    /// The tool an action runs, by the internal name the executor's policy decides on.
    #[must_use]
    pub fn action_tool(&self, plugin: &str, action: &str) -> Option<String> {
        self.actions
            .get(action)
            .map(|tool| self.qualified(plugin, tool))
    }

    /// The card tool, by its internal name.
    #[must_use]
    pub fn card_tool(&self, plugin: &str) -> String {
        self.qualified(plugin, &self.card)
    }

    /// The tools that always ask, by internal name. The submit action's tool is among them
    /// whether or not the manifest listed it: a press that sends a return is the case the yes is
    /// for, and a manifest that forgot it must not make a model's call to it silent.
    #[must_use]
    pub fn always_ask_tools(&self, plugin: &str) -> Vec<String> {
        let submit = self.actions.get("submit");
        let mut tools: Vec<String> = self
            .always_ask
            .iter()
            .chain(submit)
            .map(|tool| self.qualified(plugin, tool))
            .collect();
        tools.sort();
        tools.dedup();
        tools
    }

    fn qualified(&self, plugin: &str, tool: &str) -> String {
        format!("{plugin}.{}.{tool}", self.server)
    }
}

impl Manifest {
    /// This plugin's `extensions.formMode`, when it declares a usable one.
    #[must_use]
    pub fn form_mode(&self) -> Option<FormModeTools> {
        FormModeTools::from_extensions(self.extensions.as_ref())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bir() -> Value {
        json!({ "formMode": {
            "server": "bir-mcp",
            "card": "bir_form_card",
            "actions": { "save": "bir_form_save", "submit": "bir_form_submit",
                         "view": "bir_form_preview", "focus": "bir_form_focus",
                         "refresh": "bir_form_card", "pay": "bir_pay" },
            "alwaysAsk": ["bir_form_submit", "bir_queue_send_now"]
        }})
    }

    #[test]
    fn the_contracts_block_reads_into_internal_tool_names() {
        let tools = FormModeTools::from_extensions(Some(&bir())).expect("form mode");
        assert_eq!(tools.card_tool("bir"), "bir.bir-mcp.bir_form_card");
        assert_eq!(
            tools.action_tool("bir", "save").as_deref(),
            Some("bir.bir-mcp.bir_form_save")
        );
        // A verb the server never reviewed is not an action, whatever the manifest says.
        assert_eq!(tools.action_tool("bir", "pay"), None);
        assert_eq!(
            tools.always_ask_tools("bir"),
            vec![
                "bir.bir-mcp.bir_form_submit".to_string(),
                "bir.bir-mcp.bir_queue_send_now".to_string()
            ]
        );
    }

    #[test]
    fn the_submit_tool_asks_even_when_the_manifest_forgot_it() {
        let mut block = bir();
        block["formMode"]["alwaysAsk"] = json!([]);
        let tools = FormModeTools::from_extensions(Some(&block)).expect("form mode");
        assert_eq!(
            tools.always_ask_tools("bir"),
            vec!["bir.bir-mcp.bir_form_submit".to_string()]
        );
    }

    #[test]
    fn an_unusable_block_is_no_block() {
        assert!(FormModeTools::from_extensions(None).is_none());
        assert!(FormModeTools::from_extensions(Some(&json!({}))).is_none());
        let mut dotted = bir();
        dotted["formMode"]["server"] = json!("bir.mcp");
        assert!(FormModeTools::from_extensions(Some(&dotted)).is_none());
        let mut bad_action = bir();
        bad_action["formMode"]["actions"]["save"] = json!("other.server.tool");
        assert!(FormModeTools::from_extensions(Some(&bad_action)).is_none());
    }

    #[test]
    fn a_manifest_reads_its_own_block() {
        let manifest: Manifest = serde_json::from_value(json!({
            "$schema": crate::PLUGIN_SCHEMA, "name": "bir", "extensions": bir()
        }))
        .unwrap();
        assert!(manifest.form_mode().is_some());
    }
}
