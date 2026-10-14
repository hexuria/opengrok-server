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

use crate::bundle::Bundle;
use crate::{McpConfig, Plugin};

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
    /// block: a card with a Save that goes nowhere is worse than no card. Its `server` must be
    /// one of the plugin's own (`mcp`), or every tool it names would be dialled nowhere.
    #[must_use]
    pub fn from_extensions(extensions: Option<&Value>, mcp: &McpConfig) -> Option<Self> {
        let block = extensions?.get("formMode")?;
        let read = || {
            let server = text(block, "server").filter(|name| mcp.servers.contains_key(name))?;
            let card = text(block, "card")?;
            let mut actions = BTreeMap::new();
            for (action, tool) in block.get("actions")?.as_object()? {
                if ACTIONS.contains(&action.as_str()) {
                    let tool = tool.as_str().filter(|name| usable(name))?;
                    actions.insert(action.clone(), tool.to_string());
                }
            }
            Some(Self {
                server,
                card,
                actions,
            })
        };
        let tools = read();
        if tools.is_none() {
            tracing::warn!("a plugin's formMode block is unusable; it opens no card");
        }
        tools
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

    fn qualified(&self, plugin: &str, tool: &str) -> String {
        format!("{plugin}.{}.{tool}", self.server)
    }
}

fn text(block: &Value, key: &str) -> Option<String> {
    let name = block.get(key).and_then(Value::as_str);
    name.filter(|name| usable(name)).map(str::to_string)
}

/// The tools of `plugin` that always ask a person first, by internal name, whatever its trust or
/// the Bot's grant says: `alwaysAsk`, and the submit action's tool whether or not it is listed
/// (a press that sends a return is the case the yes is for).
///
/// READ APART FROM THE CARD BLOCK, AND FAILING CLOSED. A broken `actions` or `card` costs the
/// plugin its card, never its gate: the tools are still its tools. A `server` that is missing or
/// not one of the plugin's own gates the names on every server it has.
#[must_use]
pub fn always_ask_tools(plugin: &str, extensions: Option<&Value>, mcp: &McpConfig) -> Vec<String> {
    let Some(block) = extensions.and_then(|e| e.get("formMode")) else {
        return Vec::new();
    };
    let listed = block.get("alwaysAsk").and_then(Value::as_array);
    let submit = block.pointer("/actions/submit");
    let names: Vec<&str> = listed
        .into_iter()
        .flatten()
        .chain(submit)
        .filter_map(Value::as_str)
        .collect();
    let mut tools: Vec<&str> = names.iter().copied().filter(|name| usable(name)).collect();
    // NOTHING NAMED TO ASK ABOUT IS NOT "NOTHING ASKS". A form-mode plugin whose actions cannot
    // be read and that lists no alwaysAsk could have its submit tool under any name: every one of
    // its tools asks (`<plugin>.*`), until the manifest says which.
    if tools.is_empty() {
        tracing::warn!(
            plugin,
            "a plugin's formMode names no tool to ask about; every one of its tools asks"
        );
        return vec![format!("{plugin}.*")];
    }
    if tools.len() != names.len() || (listed.is_none() && block.get("alwaysAsk").is_some()) {
        tracing::warn!(
            plugin,
            "a plugin's formMode alwaysAsk names a tool that is not one; it is skipped"
        );
    }
    let servers: Vec<&String> = match text(block, "server") {
        Some(server) if mcp.servers.contains_key(&server) => {
            mcp.servers.keys().filter(|name| **name == server).collect()
        }
        _ => {
            tracing::warn!(
                plugin,
                "a plugin's formMode names no server of its own; its alwaysAsk tools ask on every server"
            );
            mcp.servers.keys().collect()
        }
    };
    tools.sort_unstable();
    tools.dedup();
    let qualified = servers.iter().flat_map(|server| {
        tools
            .iter()
            .map(move |tool| format!("{plugin}.{server}.{tool}"))
    });
    let mut qualified: Vec<String> = qualified.collect();
    qualified.sort();
    qualified
}

impl Plugin {
    /// This plugin's `extensions.formMode`, when it declares a usable one.
    #[must_use]
    pub fn form_mode(&self) -> Option<FormModeTools> {
        FormModeTools::from_extensions(self.manifest.extensions.as_ref(), &self.mcp)
    }

    /// This plugin's tools that always ask (`always_ask_tools`), named as `plugin`.
    #[must_use]
    pub fn always_ask_tools(&self, plugin: &str) -> Vec<String> {
        always_ask_tools(plugin, self.manifest.extensions.as_ref(), &self.mcp)
    }
}

impl Bundle {
    /// This bundle's `extensions.formMode`, when it declares a usable one.
    #[must_use]
    pub fn form_mode(&self) -> Option<FormModeTools> {
        FormModeTools::from_extensions(self.manifest.extensions.as_ref(), &self.mcp)
    }

    /// This bundle's tools that always ask (`always_ask_tools`), named as `plugin`.
    #[must_use]
    pub fn always_ask_tools(&self, plugin: &str) -> Vec<String> {
        always_ask_tools(plugin, self.manifest.extensions.as_ref(), &self.mcp)
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

    fn servers(names: &[&str]) -> McpConfig {
        let servers: serde_json::Map<String, Value> = names
            .iter()
            .map(|name| {
                (
                    name.to_string(),
                    json!({ "type": "streamable-http", "url": "https://bir.example/mcp" }),
                )
            })
            .collect();
        serde_json::from_value(json!({ "mcpServers": servers })).unwrap()
    }

    #[test]
    fn the_contracts_block_reads_into_internal_tool_names() {
        let mcp = servers(&["bir-mcp"]);
        let tools = FormModeTools::from_extensions(Some(&bir()), &mcp).expect("form mode");
        assert_eq!(tools.card_tool("bir"), "bir.bir-mcp.bir_form_card");
        assert_eq!(
            tools.action_tool("bir", "save").as_deref(),
            Some("bir.bir-mcp.bir_form_save")
        );
        // A verb the server never reviewed is not an action, whatever the manifest says.
        assert_eq!(tools.action_tool("bir", "pay"), None);
        assert_eq!(
            always_ask_tools("bir", Some(&bir()), &mcp),
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
        assert_eq!(
            always_ask_tools("bir", Some(&block), &servers(&["bir-mcp"])),
            vec!["bir.bir-mcp.bir_form_submit".to_string()]
        );
    }

    /// A broken card block costs the plugin its card, never its gate.
    #[test]
    fn a_broken_block_still_gates_the_submit_tool() {
        let mcp = servers(&["bir-mcp"]);
        for broken in [
            json!("not an object"),
            json!({ "save": "other.server.tool" }),
        ] {
            let mut block = bir();
            block["formMode"]["actions"] = broken;
            assert!(FormModeTools::from_extensions(Some(&block), &mcp).is_none());
            assert!(
                always_ask_tools("bir", Some(&block), &mcp)
                    .contains(&"bir.bir-mcp.bir_form_submit".to_string()),
                "{block}"
            );
        }
    }

    /// A server the plugin does not have opens no card, and its tools ask on every server.
    #[test]
    fn a_server_that_is_not_the_plugins_fails_closed() {
        let mcp = servers(&["bir-mcp", "bir-other"]);
        for server in [json!("elsewhere"), json!("bir.mcp"), Value::Null] {
            let mut block = bir();
            block["formMode"]["server"] = server;
            assert!(FormModeTools::from_extensions(Some(&block), &mcp).is_none());
            let asks = always_ask_tools("bir", Some(&block), &mcp);
            assert!(
                asks.contains(&"bir.bir-mcp.bir_form_submit".to_string()),
                "{asks:?}"
            );
            assert!(
                asks.contains(&"bir.bir-other.bir_form_submit".to_string()),
                "{asks:?}"
            );
        }
    }

    #[test]
    fn an_unusable_block_is_no_block() {
        let mcp = servers(&["bir-mcp"]);
        assert!(FormModeTools::from_extensions(None, &mcp).is_none());
        assert!(FormModeTools::from_extensions(Some(&json!({})), &mcp).is_none());
        assert!(always_ask_tools("bir", Some(&json!({})), &mcp).is_empty());
    }

    /// A formMode block whose actions cannot be read and that lists nothing to ask about could
    /// have its submit under any name: all of the plugin's tools ask.
    #[test]
    fn a_block_that_names_nothing_to_ask_about_gates_the_whole_plugin() {
        let mcp = servers(&["bir-mcp"]);
        let mut block = bir();
        block["formMode"]["actions"] = json!("not an object");
        block["formMode"]["alwaysAsk"] = json!([]);
        assert_eq!(
            always_ask_tools("bir", Some(&block), &mcp),
            vec!["bir.*".to_string()]
        );
        block["formMode"]
            .as_object_mut()
            .unwrap()
            .remove("alwaysAsk");
        assert_eq!(
            always_ask_tools("bir", Some(&block), &mcp),
            vec!["bir.*".to_string()]
        );
    }
}
