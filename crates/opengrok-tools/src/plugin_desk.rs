//! The plugin tools (#359): a Bot finds, installs and removes its person's plugins, adds and
//! manages their accounts, and switches a plugin on for itself or another of its person's Bots,
//! when the person asks in chat, through the same functions the Plugins routes use (opengrok-server
//! `plugin_desk.rs`), so a tool may do what the routes may, refused in the same words.
//!
//! RUN INSIDE `Executor::execute`, LIKE THE ROUTINE TOOLS: the ceiling, the grant and an ask apply
//! as to every built-in, and none of them touches the box. One ceiling row switches all of them
//! (`ROW`). The owner's rules (6 Oct 2026): only what cannot be undone asks first, which is an
//! uninstall and removing an account; anything aimed at ANOTHER of the person's Bots asks first
//! too, naming that Bot as stored. Everything else just happens and is reported.
//!
//! THE ACCOUNT IS NEVER AN ARGUMENT: the desk answers as the `ToolContext`'s account. A `bot` is
//! read only as one of that person's own Bots, and one that is not theirs is refused in the same
//! words as one that does not exist, so an id cannot be probed for.
//!
//! NO SECRET IS EVER AN ARGUMENT OR A RESULT. Adding an account shows the person a card in chat
//! (sign in at the service, or paste a key into a masked field that posts straight to the server's
//! vault); the model is told only that the card is up. A key typed into chat would sit in the
//! transcript and the model's context for good.

use serde_json::{Value, json};

use crate::{ToolContext, ToolResult};

pub const LIST_PLUGINS: &str = "list_plugins";
pub const PLUGIN_DETAILS: &str = "plugin_details";
pub const INSTALL_PLUGIN: &str = "install_plugin";
pub const UNINSTALL_PLUGIN: &str = "uninstall_plugin";
pub const LIST_PLUGIN_ACCOUNTS: &str = "list_plugin_accounts";
pub const ADD_PLUGIN_ACCOUNT: &str = "add_plugin_account";
pub const RENAME_PLUGIN_ACCOUNT: &str = "rename_plugin_account";
pub const REMOVE_PLUGIN_ACCOUNT: &str = "remove_plugin_account";
pub const SET_PLUGIN_FOR_BOT: &str = "set_plugin_for_bot";
pub const PICK_PLUGIN_ACCOUNT: &str = "pick_plugin_account";

/// Every plugin tool, in the order they are offered: finding first, then installing, accounts,
/// and which Bot uses what.
pub const TOOLS: [&str; 10] = [
    LIST_PLUGINS,
    PLUGIN_DETAILS,
    INSTALL_PLUGIN,
    UNINSTALL_PLUGIN,
    LIST_PLUGIN_ACCOUNTS,
    ADD_PLUGIN_ACCOUNT,
    RENAME_PLUGIN_ACCOUNT,
    REMOVE_PLUGIN_ACCOUNT,
    SET_PLUGIN_FOR_BOT,
    PICK_PLUGIN_ACCOUNT,
];

/// The ceiling row that switches them all (`GET`/`PUT /coworkers/{id}/ceiling`).
pub const ROW: &str = "plugins";
pub const ROW_LABEL: &str = "Plugins";
pub const ROW_DESCRIPTION: &str = "Find, install and remove plugins, add and manage their accounts, \
     and switch them on for your Bots when you ask in chat. Uninstalling, removing an account, or \
     changing another Bot always asks you first.";

pub fn is_plugin_desk_tool(name: &str) -> bool {
    TOOLS.contains(&name)
}

/// Which plugins `list_plugins` lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Filter {
    #[default]
    All,
    Installed,
    Available,
}

/// One call, read and checked as far as its arguments go. `bot` is a name or id as the person or
/// a listing gave it; `None` is this Bot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ask {
    List {
        filter: Filter,
        query: Option<String>,
        category: Option<String>,
    },
    Details {
        plugin: String,
    },
    Install {
        plugin: String,
    },
    Uninstall {
        plugin: String,
    },
    Accounts {
        plugin: Option<String>,
    },
    AddAccount {
        plugin: String,
        connector: Option<String>,
    },
    Rename {
        account: String,
        label: String,
    },
    Remove {
        account: String,
    },
    SetForBot {
        plugin: String,
        on: bool,
        bot: Option<String>,
    },
    /// `account: None` is "ask each time": the Bot's pin is cleared.
    Pick {
        plugin: String,
        connector: Option<String>,
        account: Option<String>,
        bot: Option<String>,
    },
}

/// The person's plugins and accounts, as the server keeps them. Every call is answered as
/// `context`'s account; a refusal is a sentence the model can act on.
#[async_trait::async_trait]
pub trait PluginDesk: Send + Sync {
    /// Carry out `ask`: what the model is told back, or why not.
    async fn answer(&self, context: &ToolContext, ask: Ask) -> Result<Value, String>;
    /// For a call that asks first, the card's sentence, naming what it acts on as stored; `None`
    /// for one that just happens. A call that could never run is refused here, before any card.
    async fn ask_first(&self, context: &ToolContext, ask: &Ask) -> Result<Option<String>, String>;
}

/// What `Executor::execute` holds before its gates, for a plugin tool: the call read, and the
/// card's sentence when it asks first; or the refusal it earns now.
pub async fn admit(
    desk: &dyn PluginDesk,
    context: &ToolContext,
    (name, arguments): (&str, &Value),
) -> Result<(Ask, Option<String>), String> {
    let ask = read(name, arguments)?;
    let card = desk.ask_first(context, &ask).await?;
    Ok((ask, card))
}

/// Carry out an admitted call: its JSON, or the refusal, as a result either way (CLAUDE.md #8).
pub async fn run(
    desk: &dyn PluginDesk,
    context: &ToolContext,
    call_id: &str,
    ask: Ask,
) -> ToolResult {
    match desk.answer(context, ask).await {
        Ok(answer) => ToolResult::ok(call_id, answer.to_string()),
        Err(why) => ToolResult::refused(call_id, why),
    }
}

/// The keys a call may never carry, under any spelling: a secret has no business in a tool call.
const SECRET_KEYS: [&str; 6] = ["token", "key", "apiKey", "api_key", "password", "secret"];

/// What a call that carried a secret is told: nothing was read, and where the key goes instead.
pub const NO_SECRETS: &str = "never pass a key, token or password to a tool: call \
     add_plugin_account and the person pastes it into the card, which keeps it out of chat.";

/// A call's arguments, read. The keys `overwrite_identity` writes are never read.
fn read(name: &str, arguments: &Value) -> Result<Ask, String> {
    if SECRET_KEYS.iter().any(|key| arguments.get(*key).is_some()) {
        return Err(NO_SECRETS.to_string());
    }
    let text = |key: &str| -> Result<Option<String>, String> {
        match arguments.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(text)) => {
                Ok(Some(text.trim().to_string()).filter(|t| !t.is_empty()))
            }
            Some(_) => Err(format!("bad arguments: {key} must be a string")),
        }
    };
    let plugin = || {
        text("plugin")?
            .ok_or_else(|| format!("bad arguments: name the plugin as {LIST_PLUGINS} gives it"))
    };
    let account = || {
        text("account")?.ok_or_else(|| {
            format!("bad arguments: name the account by its id, as {LIST_PLUGIN_ACCOUNTS} gives it")
        })
    };
    match name {
        LIST_PLUGINS => {
            let filter = match text("filter")?.as_deref() {
                None | Some("all") => Filter::All,
                Some("installed") => Filter::Installed,
                Some("available") => Filter::Available,
                Some(_) => {
                    return Err("bad arguments: filter is installed, available or all".to_string());
                }
            };
            Ok(Ask::List {
                filter,
                query: text("query")?,
                category: text("category")?,
            })
        }
        PLUGIN_DETAILS => Ok(Ask::Details { plugin: plugin()? }),
        INSTALL_PLUGIN => Ok(Ask::Install { plugin: plugin()? }),
        UNINSTALL_PLUGIN => Ok(Ask::Uninstall { plugin: plugin()? }),
        LIST_PLUGIN_ACCOUNTS => Ok(Ask::Accounts {
            plugin: text("plugin")?,
        }),
        ADD_PLUGIN_ACCOUNT => Ok(Ask::AddAccount {
            plugin: plugin()?,
            connector: text("connector")?,
        }),
        RENAME_PLUGIN_ACCOUNT => Ok(Ask::Rename {
            account: account()?,
            label: text("label")?
                .ok_or_else(|| "bad arguments: label is the account's new name".to_string())?,
        }),
        REMOVE_PLUGIN_ACCOUNT => Ok(Ask::Remove {
            account: account()?,
        }),
        SET_PLUGIN_FOR_BOT => Ok(Ask::SetForBot {
            plugin: plugin()?,
            on: match arguments.get("on") {
                Some(Value::Bool(on)) => *on,
                _ => return Err("bad arguments: on is true or false".to_string()),
            },
            bot: text("bot")?,
        }),
        PICK_PLUGIN_ACCOUNT => {
            let each_time = matches!(arguments.get("ask_each_time"), Some(Value::Bool(true)));
            let account = text("account")?;
            if each_time == account.is_some() {
                return Err(
                    "bad arguments: give account, or ask_each_time true, and not both".to_string(),
                );
            }
            Ok(Ask::Pick {
                plugin: plugin()?,
                connector: text("connector")?,
                account,
                bot: text("bot")?,
            })
        }
        other => Err(format!("there is no plugin tool called {other}")),
    }
}

/// What a tool says it is for, as a turn offers it and a ceiling describes it; the group's row
/// for `ROW`.
pub fn description(name: &str) -> Option<&'static str> {
    Some(match name {
        LIST_PLUGINS => {
            "List the plugins your person can use: each one's name, what it does, its category, \
             whether it is installed, and for an installed one whether it is on for this Bot. \
             `filter` is installed, available or all; `query` matches names and descriptions."
        }
        PLUGIN_DETAILS => {
            "One plugin in full: its skills, tools and apps, the services it needs an account \
             for, the accounts your person has for it, and which of their Bots have it on."
        }
        INSTALL_PLUGIN => {
            "Install a plugin for your person, at the registry's current version. Installing \
             switches it on for no Bot and adds no account: say so, and offer the next step."
        }
        UNINSTALL_PLUGIN => {
            "Uninstall one of your person's plugins. Its accounts are removed with it. It always \
             asks the person first."
        }
        LIST_PLUGIN_ACCOUNTS => {
            "List your person's plugin accounts: each one's id, label, plugin, service, whether \
             it is connected or needs sign-in, and which Bots use it. Never a key."
        }
        ADD_PLUGIN_ACCOUNT => {
            "Add an account for an installed plugin: the person is shown a card in chat to sign \
             in at the service, or to paste a key into a field that keeps it out of chat. Never \
             ask for a key or password in chat, and never sign in through your computer. After \
             calling it, tell the person the card is waiting and stop."
        }
        RENAME_PLUGIN_ACCOUNT => {
            "Rename one of your person's plugin accounts, by its id from list_plugin_accounts."
        }
        REMOVE_PLUGIN_ACCOUNT => {
            "Remove one of your person's plugin accounts for good, by its id from \
             list_plugin_accounts. Bots that used it lose it. It always asks the person first."
        }
        SET_PLUGIN_FOR_BOT => {
            "Switch an installed plugin on or off for this Bot, or for another of your person's \
             Bots named by `bot`, ONLY when the person asked for that switch in this conversation. \
             Never switch one on to get a job done: a plugin that is off is the person's choice, \
             and they use it for one message by tagging @name. Another Bot always asks first."
        }
        PICK_PLUGIN_ACCOUNT => {
            "Choose which of your person's accounts a Bot uses for a plugin's service, by its id \
             from list_plugin_accounts, or ask_each_time true to clear the choice, ONLY when the \
             person asked for that choice. This Bot unless `bot` names another of theirs, which \
             always asks the person first."
        }
        ROW => ROW_DESCRIPTION,
        _ => return None,
    })
}

/// The function definition a turn offers for `name`.
pub fn schema(name: &str) -> Option<Value> {
    let description = description(name)?;
    let string = |what: &str| json!({ "type": "string", "description": what });
    let plugin = string("The plugin's name, as list_plugins gives it.");
    let account = string("The account's id, as list_plugin_accounts gives it.");
    let bot = string("Another of your person's Bots, by name or id. Left out, this Bot.");
    let connector =
        string("The service, when the plugin has more than one, as plugin_details names them.");
    let (properties, required) = match name {
        LIST_PLUGINS => (
            json!({
                "filter": { "type": "string", "enum": ["installed", "available", "all"],
                    "description": "Which plugins. Left out, all." },
                "query": string("Words to match in names and descriptions."),
                "category": string("Only this category."),
            }),
            json!([]),
        ),
        PLUGIN_DETAILS | INSTALL_PLUGIN | UNINSTALL_PLUGIN => {
            (json!({ "plugin": plugin }), json!(["plugin"]))
        }
        LIST_PLUGIN_ACCOUNTS => (
            json!({ "plugin": string("Only this plugin's accounts.") }),
            json!([]),
        ),
        ADD_PLUGIN_ACCOUNT => (
            json!({ "plugin": plugin, "connector": connector }),
            json!(["plugin"]),
        ),
        RENAME_PLUGIN_ACCOUNT => (
            json!({ "account": account, "label": string("The new name, at most 80 characters.") }),
            json!(["account", "label"]),
        ),
        REMOVE_PLUGIN_ACCOUNT => (json!({ "account": account }), json!(["account"])),
        SET_PLUGIN_FOR_BOT => (
            json!({ "plugin": plugin, "on": { "type": "boolean",
                "description": "true switches it on, false off." }, "bot": bot }),
            json!(["plugin", "on"]),
        ),
        PICK_PLUGIN_ACCOUNT => (
            json!({ "plugin": plugin, "account": account, "connector": connector,
                "ask_each_time": { "type": "boolean",
                    "description": "true clears the choice, so the person is asked each time." },
                "bot": bot }),
            json!(["plugin"]),
        ),
        _ => return None,
    };
    let parameters = json!({ "type": "object", "properties": properties, "required": required });
    Some(json!({ "type": "function",
        "function": { "name": name, "description": description, "parameters": parameters } }))
}

#[cfg(test)]
#[path = "../tests/unit/plugin_desk.rs"]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
