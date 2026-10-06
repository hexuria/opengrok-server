//! The desk behind a Bot's plugin tools (#359, `opengrok_tools::plugin_desk`): the person's
//! installs, accounts and switches, through the same functions `/plugins/*`, `/connections/*`,
//! the pins and the ceiling routes use, so a tool may do what those may, refused in the same words.
//!
//! EVERYTHING IS THE CONTEXT ACCOUNT'S. Installs, accounts and pins are read for the account the
//! turn's bearer named; a Bot a call names is found only among that account's own Bots, and one
//! that is not theirs is refused as one that does not exist. On a shared Bot a member's call acts
//! on the member's plugins, never the owner's, and cannot switch the Bot itself: it is not theirs.

use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_integrations::{accounts, attempts, installed, registry::Registry};
use opengrok_tools::ToolContext;
use opengrok_tools::plugin_desk::{Ask, Filter, PluginDesk};
use serde_json::{Value, json};

use crate::agui::AgUiState;
use crate::now_ms;

/// The most rows `list_plugins` answers with: a catalog can be long, and a model reads every row.
const MAX_LISTED: usize = 50;

/// The plugin tools' desk, as the context's account.
pub struct Tools {
    pub state: AgUiState,
    /// The registry the Plugins routes serve (`plugin_registry::registry`); `None` lists and
    /// manages what is installed, and refuses an install in the routes' words.
    pub registry: Option<Registry>,
}

/// What a store that did not answer is told: the model passes it on, and tries again later.
fn unavailable(error: impl std::fmt::Display) -> String {
    tracing::warn!(%error, "a plugin tool could not reach the store");
    "your plugins could not be read now; try again in a moment.".to_string()
}

fn not_installed(plugin: &str) -> String {
    format!("{plugin} is not installed; call list_plugins.")
}

fn not_an_account(id: &str) -> String {
    format!("no plugin account {id} is your person's; call list_plugin_accounts.")
}

/// One of the person's own Bots: its id and name.
type Bot = (CoworkerId, String);

/// One plugin account as the tools carry it, from `installed::bindings`.
struct Account {
    id: String,
    label: String,
    plugin: String,
    connector: String,
    kind: String,
}

impl Tools {
    fn store(&self) -> &opengrok_store::PgStore {
        &self.state.auth.store
    }

    /// The person's own Bots: theirs, hired, not a group.
    async fn bots(&self, account: &AccountId) -> Result<Vec<Bot>, String> {
        let owned = self
            .store()
            .coworkers_for(account)
            .await
            .map_err(unavailable)?;
        let bots = owned.into_iter().filter(|bot| bot.members.is_empty());
        Ok(bots.map(|bot| (bot.id, bot.name)).collect())
    }

    /// The Bot a call is about: `named` among the person's own, by id or name, or the session's
    /// own when none is named. The second half says whether it is another Bot than this one.
    async fn bot(&self, context: &ToolContext, named: Option<&str>) -> Result<(Bot, bool), String> {
        let bots = self.bots(&context.account_id).await?;
        let Some(named) = named else {
            let found = bots.into_iter().find(|(id, _)| *id == context.coworker_id);
            let why = "this Bot is not one of your person's own Bots, so its plugins are not \
                       theirs to change.";
            return found.map(|bot| (bot, false)).ok_or_else(|| why.to_string());
        };
        if let Some(bot) = bots.iter().find(|(id, _)| id.as_str() == named) {
            let other = bot.0 != context.coworker_id;
            return Ok((bot.clone(), other));
        }
        let called: Vec<&Bot> = bots
            .iter()
            .filter(|(_, name)| name.eq_ignore_ascii_case(named))
            .collect();
        match called[..] {
            [bot] => Ok((bot.clone(), bot.0 != context.coworker_id)),
            [] => Err(format!("no Bot called {named} is your person's.")),
            _ => {
                let ids: Vec<&str> = called.iter().map(|(id, _)| id.as_str()).collect();
                let ids = ids.join(", ");
                Err(format!(
                    "more than one of your person's Bots is called {named} ({ids}); name one by its id."
                ))
            }
        }
    }

    async fn installs(&self, account: &AccountId) -> Result<Vec<installed::Installation>, String> {
        installed::list(self.store(), account)
            .await
            .map_err(unavailable)
    }

    async fn installation(
        &self,
        account: &AccountId,
        plugin: &str,
    ) -> Result<installed::Installation, String> {
        let installs = self.installs(account).await?;
        let found = installs.into_iter().find(|install| install.name == plugin);
        found.ok_or_else(|| not_installed(plugin))
    }

    /// The service a call means: the one it names, which must be the plugin's, or the plugin's
    /// only one.
    fn connector_of(
        install: &installed::Installation,
        named: Option<&str>,
    ) -> Result<String, String> {
        let connectors = install.bundle.connectors();
        let plugin = &install.name;
        match (named, &connectors[..]) {
            (Some(named), _) if connectors.iter().any(|c| c == named) => Ok(named.to_string()),
            (Some(named), _) => Err(format!("{plugin} has no service called {named}.")),
            (None, [only]) => Ok(only.clone()),
            (None, []) => Err(format!("{plugin} needs no account.")),
            (None, several) => Err(format!(
                "{plugin} has accounts for several services ({}); name one as connector.",
                several.join(", ")
            )),
        }
    }

    /// The person's plugin accounts, each with the plugin and service it is for.
    async fn accounts(&self, account: &AccountId) -> Result<Vec<Account>, String> {
        let store = self.store();
        let bound = installed::bindings(store, account)
            .await
            .map_err(unavailable)?;
        let views = store
            .connections_owned_by(account)
            .await
            .map_err(unavailable)?;
        let accounts = bound.into_iter().filter_map(|(plugin, connector, id)| {
            let view = views.iter().find(|view| view.id == id)?;
            Some(Account {
                label: view.label.clone(),
                kind: view.kind.word().to_string(),
                id,
                plugin,
                connector,
            })
        });
        Ok(accounts.collect())
    }

    async fn account(&self, account: &AccountId, id: &str) -> Result<Account, String> {
        let accounts = self.accounts(account).await?;
        let found = accounts.into_iter().find(|candidate| candidate.id == id);
        found.ok_or_else(|| not_an_account(id))
    }

    /// The names of the person's Bots pinned to `id`.
    async fn pinned_to(&self, account: &AccountId, id: &str) -> Result<Vec<String>, String> {
        let pins = accounts::pins(self.store(), account)
            .await
            .map_err(unavailable)?;
        let bots = self.bots(account).await?;
        let names = pins
            .iter()
            .filter(|pin| pin.connection_id == id)
            .filter_map(|pin| {
                let bot = bots
                    .iter()
                    .find(|(bot, _)| bot.as_str() == pin.coworker_id)?;
                Some(bot.1.clone())
            });
        Ok(names.collect())
    }

    /// The person's Bots that have `plugin` switched on.
    async fn on_for(&self, account: &AccountId, plugin: &str) -> Result<Vec<String>, String> {
        let mut names = Vec::new();
        for (bot, name) in self.bots(account).await? {
            let policy = self.store().policy_to_use(account, &bot).await;
            let policy = policy.map_err(unavailable)?;
            if opengrok_integrations::turn::switched_on(account, &bot, plugin, &policy) {
                names.push(name);
            }
        }
        Ok(names)
    }

    /// The accounts as `list_plugin_accounts` lists them, and the sign-ins still waiting.
    async fn account_rows(
        &self,
        account: &AccountId,
        plugin: Option<&str>,
    ) -> Result<Vec<Value>, String> {
        let wanted = |name: &str| plugin.is_none_or(|plugin| plugin == name);
        let mut rows = Vec::new();
        for found in self.accounts(account).await? {
            if !wanted(&found.plugin) {
                continue;
            }
            let pinned = self.pinned_to(account, &found.id).await?;
            rows.push(
                json!({ "id": found.id, "label": found.label, "plugin": found.plugin,
                "service": found.connector, "kind": found.kind, "status": "connected",
                "pinnedBy": pinned }),
            );
        }
        let waiting = attempts::list(self.store(), account)
            .await
            .map_err(unavailable)?;
        for attempt in waiting {
            let Some(name) = attempt.plugin.as_deref().filter(|name| wanted(name)) else {
                continue;
            };
            let status = match attempt.error.as_deref() {
                Some(why) => format!("sign-in failed: {why}"),
                None => "waiting for sign-in".to_string(),
            };
            rows.push(
                json!({ "id": attempt.id, "label": attempt.label, "plugin": name,
                "service": attempt.connector, "status": status }),
            );
        }
        Ok(rows)
    }

    async fn list(
        &self,
        context: &ToolContext,
        (filter, query, category): (Filter, Option<String>, Option<String>),
    ) -> Result<Value, String> {
        let account = &context.account_id;
        let installs = self.installs(account).await?;
        let policy = self
            .store()
            .policy_to_use(account, &context.coworker_id)
            .await;
        let on = |name: &str| {
            policy.as_ref().is_ok_and(|policy| {
                opengrok_integrations::turn::switched_on(
                    account,
                    &context.coworker_id,
                    name,
                    policy,
                )
            })
        };
        let (catalog, missing) = match &self.registry {
            Some(registry) => match registry.catalog(None).await {
                Ok(catalog) => (catalog.plugins, None),
                Err(error) => (Vec::new(), Some(error.to_string())),
            },
            None => (
                Vec::new(),
                Some("this server has no plugin registry".to_string()),
            ),
        };
        let query = query.map(|query| query.to_lowercase());
        let matches = |name: &str, description: &str, of: Option<&str>| {
            let said = query.as_deref().is_none_or(|query| {
                name.to_lowercase().contains(query) || description.to_lowercase().contains(query)
            });
            let filed = category
                .as_deref()
                .is_none_or(|wanted| of.is_some_and(|of| of.eq_ignore_ascii_case(wanted)));
            said && filed
        };
        let mut rows = Vec::new();
        for entry in &catalog {
            let installed = installs.iter().any(|install| install.name == entry.name);
            let shown = match filter {
                Filter::All => true,
                Filter::Installed => installed,
                Filter::Available => !installed,
            };
            if shown && matches(&entry.name, &entry.description, entry.category.as_deref()) {
                let mut row = json!({ "name": entry.name, "description": entry.description,
                    "category": entry.category, "installed": installed });
                if installed {
                    row["onForThisBot"] = json!(on(&entry.name));
                }
                rows.push(row);
            }
        }
        // An install the catalog no longer lists is still the person's.
        if filter != Filter::Available {
            for install in &installs {
                let listed = catalog.iter().any(|entry| entry.name == install.name);
                let description = install
                    .bundle
                    .manifest
                    .description
                    .clone()
                    .unwrap_or_default();
                if !listed && matches(&install.name, &description, None) {
                    rows.push(json!({ "name": install.name, "description": description,
                        "installed": true, "onForThisBot": on(&install.name) }));
                }
            }
        }
        let more = rows.len().saturating_sub(MAX_LISTED);
        rows.truncate(MAX_LISTED);
        let mut answer = json!({ "plugins": rows });
        if more > 0 {
            answer["more"] = json!(format!("{more} more; narrow it with query or category"));
        }
        if let Some(why) = missing {
            answer["catalog"] = json!(format!(
                "the catalog could not be read ({why}); installed plugins only"
            ));
        }
        Ok(answer)
    }

    async fn details(&self, context: &ToolContext, plugin: &str) -> Result<Value, String> {
        let account = &context.account_id;
        let install = self
            .installs(account)
            .await?
            .into_iter()
            .find(|i| i.name == plugin);
        let entry = match &self.registry {
            Some(registry) => match registry.catalog(None).await {
                Ok(catalog) => catalog
                    .plugins
                    .into_iter()
                    .find(|entry| entry.name == plugin),
                Err(_) => None,
            },
            None => None,
        };
        let bundle = match (&install, &entry, &self.registry) {
            (Some(install), ..) => install.bundle.clone(),
            (None, Some(entry), Some(registry)) => registry
                .bundle(entry)
                .await
                .map_err(|error| error.to_string())?,
            _ => return Err(format!("no plugin called {plugin}; call list_plugins.")),
        };
        let parts: Vec<Value> = bundle
            .parts
            .iter()
            .map(
                |part| json!({ "kind": part.kind, "name": part.name, "supported": part.supported }),
            )
            .collect();
        let mut answer = json!({ "name": plugin, "installed": install.is_some(), "parts": parts,
            "services": bundle.connectors() });
        if let Some(entry) = &entry {
            answer["description"] = json!(entry.description);
            answer["category"] = json!(entry.category);
            answer["homepage"] = json!(entry.homepage);
        }
        if install.is_some() {
            answer["accounts"] = json!(self.account_rows(account, Some(plugin)).await?);
            answer["onFor"] = json!(self.on_for(account, plugin).await?);
        }
        Ok(answer)
    }

    async fn install(&self, context: &ToolContext, plugin: &str) -> Result<Value, String> {
        let account = &context.account_id;
        if crate::plugin_registry::reserved_names(&self.state).contains(plugin) {
            return Err("plugin name is reserved by this deployment".to_string());
        }
        let Some(registry) = &self.registry else {
            return Err("plugin registry is unavailable".to_string());
        };
        let head = registry
            .catalog(None)
            .await
            .map_err(|error| error.to_string())?;
        let found = registry.installable(&head.revision, plugin).await;
        let (catalog, entry) = match found.map_err(|error| error.to_string())? {
            Some(found) => found,
            None => {
                return Err(format!(
                    "no plugin called {plugin} in the catalog; call list_plugins."
                ));
            }
        };
        let bundle = registry
            .bundle(&entry)
            .await
            .map_err(|error| error.to_string())?;
        let at_ms = now_ms();
        match installed::install(self.store(), account, &catalog, &entry, &bundle, at_ms).await {
            Ok(()) => Ok(
                json!({ "installed": entry.name, "services": bundle.connectors(),
                "next": "It is on for no Bot and has no account yet: offer to add one \
                         (add_plugin_account) and to switch it on (set_plugin_for_bot)." }),
            ),
            Err(opengrok_store::StoreError::Conflict) => {
                Err(format!("{plugin} is already installed."))
            }
            Err(error) => Err(unavailable(error)),
        }
    }

    async fn remove(&self, context: &ToolContext, id: &str) -> Result<Value, String> {
        let account = &context.account_id;
        let found = self.account(account, id).await?;
        let lost = self.pinned_to(account, id).await?;
        let store = self.store();
        let mut tx = store.pool().begin().await.map_err(unavailable)?;
        accounts::remove_in(store, &mut tx, id, now_ms())
            .await
            .map_err(unavailable)?;
        tx.commit().await.map_err(unavailable)?;
        Ok(
            json!({ "removed": id, "label": found.label, "plugin": found.plugin,
            "botsThatLostIt": lost }),
        )
    }

    async fn pick(
        &self,
        context: &ToolContext,
        (plugin, connector, chosen, bot): (String, Option<String>, Option<String>, Option<String>),
    ) -> Result<Value, String> {
        let account = &context.account_id;
        let ((bot, name), _) = self.bot(context, bot.as_deref()).await?;
        let install = self.installation(account, &plugin).await?;
        let connector = Self::connector_of(&install, connector.as_deref())?;
        let store = self.store();
        let Some(id) = chosen else {
            accounts::unpin(store, account, &bot, &connector)
                .await
                .map_err(|error| error.to_string())?;
            return Ok(json!({ "plugin": plugin, "bot": name, "account": null,
                "note": "the person is asked which account each time" }));
        };
        let found = self.account(account, &id).await?;
        if found.plugin != plugin || found.connector != connector {
            return Err(format!("{id} is not a {plugin} account for {connector}."));
        }
        accounts::pin(store, account, &bot, &connector, &id)
            .await
            .map_err(|error| error.to_string())?;
        Ok(json!({ "plugin": plugin, "bot": name, "account": id, "label": found.label }))
    }
}

#[async_trait::async_trait]
impl PluginDesk for Tools {
    async fn answer(&self, context: &ToolContext, ask: Ask) -> Result<Value, String> {
        let account = &context.account_id;
        match ask {
            Ask::List {
                filter,
                query,
                category,
            } => self.list(context, (filter, query, category)).await,
            Ask::Details { plugin } => self.details(context, &plugin).await,
            Ask::Install { plugin } => self.install(context, &plugin).await,
            Ask::Uninstall { plugin } => {
                let at_ms = now_ms();
                match installed::uninstall(self.store(), account, &plugin, at_ms).await {
                    Ok(true) => Ok(json!({ "uninstalled": plugin })),
                    Ok(false) => Err(not_installed(&plugin)),
                    Err(error) => Err(unavailable(error)),
                }
            }
            Ask::Accounts { plugin } => {
                let rows = self.account_rows(account, plugin.as_deref()).await?;
                Ok(json!({ "accounts": rows }))
            }
            Ask::AddAccount { plugin, connector } => {
                let install = self.installation(account, &plugin).await?;
                let connector = Self::connector_of(&install, connector.as_deref())?;
                Ok(
                    json!({ "plugin": plugin, "connector": connector, "card": "shown",
                    "status": "The person was shown a card to add the account. Nothing is \
                               connected until they finish it: tell them the card is waiting, \
                               and stop." }),
                )
            }
            Ask::Rename { account: id, label } => {
                self.account(account, &id).await?;
                match accounts::rename(self.store(), account, &id, label, now_ms()).await {
                    Ok(view) => Ok(json!({ "id": view.id, "label": view.label })),
                    Err(accounts::RenameError::NotFound) => Err(not_an_account(&id)),
                    Err(accounts::RenameError::Refused(error)) => Err(error.to_string()),
                    Err(accounts::RenameError::Store(error)) => Err(unavailable(error)),
                }
            }
            Ask::Remove { account: id } => self.remove(context, &id).await,
            Ask::SetForBot { plugin, on, bot } => {
                let ((bot, name), _) = self.bot(context, bot.as_deref()).await?;
                let set = crate::agui::ceiling::set_plugin(&self.state, account, &bot, &plugin, on);
                let changed = set.await?;
                let mut answer =
                    json!({ "plugin": plugin, "bot": name, "on": on, "changed": changed });
                let accountless = self.account_rows(account, Some(&plugin)).await?.is_empty();
                let install = self.installation(account, &plugin).await?;
                if on && accountless && !install.bundle.connectors().is_empty() {
                    answer["note"] = json!("it has no account yet; offer add_plugin_account");
                }
                Ok(answer)
            }
            Ask::Pick {
                plugin,
                connector,
                account: chosen,
                bot,
            } => self.pick(context, (plugin, connector, chosen, bot)).await,
        }
    }

    async fn ask_first(&self, context: &ToolContext, ask: &Ask) -> Result<Option<String>, String> {
        let account = &context.account_id;
        match ask {
            Ask::Uninstall { plugin } => {
                self.installation(account, plugin).await?;
                let held = self.account_rows(account, Some(plugin)).await?.len();
                Ok(Some(match held {
                    0 => format!("Uninstall {plugin}?"),
                    1 => format!("Uninstall {plugin}? Its account is removed with it."),
                    n => format!("Uninstall {plugin}? Its {n} accounts are removed with it."),
                }))
            }
            Ask::Remove { account: id } => {
                let found = self.account(account, id).await?;
                let lost = self.pinned_to(account, id).await?;
                let label: String = found.label.chars().take(80).collect();
                let plugin = &found.plugin;
                Ok(Some(match &lost[..] {
                    [] => format!("Remove the {plugin} account \"{label}\"?"),
                    bots => format!(
                        "Remove the {plugin} account \"{label}\"? {} will stop using it.",
                        bots.join(", ")
                    ),
                }))
            }
            Ask::SetForBot { plugin, on, bot } => {
                let ((_, name), other) = self.bot(context, bot.as_deref()).await?;
                self.installation(account, plugin).await?;
                let state = if *on { "on" } else { "off" };
                Ok(other.then(|| format!("Turn {plugin} {state} for {name}?")))
            }
            Ask::Pick {
                plugin,
                account: chosen,
                bot,
                ..
            } => {
                let ((_, name), other) = self.bot(context, bot.as_deref()).await?;
                self.installation(account, plugin).await?;
                if !other {
                    return Ok(None);
                }
                Ok(Some(match chosen {
                    Some(id) => {
                        let found = self.account(account, id).await?;
                        format!("Make \"{}\" the {plugin} account {name} uses?", found.label)
                    }
                    None => format!("Have {name} ask which {plugin} account to use each time?"),
                }))
            }
            _ => Ok(None),
        }
    }
}
