//! What an account's installed plugins give one turn of a Bot that account owns: the MCP servers
//! it may dial and the skills it may read. Beside the storage it reads, so the per-turn rules for
//! account bundles are in one place rather than spread across the server's turn code.
//!
//! THE BOT CEILING DECIDES BOTH. A plugin's switch covered its MCP tools only, so a Bot with the
//! plugin switched off still listed its skills and handed the model their full third-party text
//! through `use_skill`. Skills are now offered only while the plugin is switched on, and checked
//! again when one is read, because a resumed run re-offers what its start captured (CLAUDE.md #6).
use crate::installed::{self, Installation};
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_policy::Context;
use opengrok_store::{PgStore, Vault};
use opengrok_tools::mcp::Endpoint;
use opengrok_tools::skill::SkillOffer;
use std::collections::BTreeMap;

/// Whether `plugin` is switched on for this turn (`opengrok_policy::names_plugin`).
pub use opengrok_policy::names_plugin as switched_on;

/// Fail closed and say why: a turn whose plugins could not be read goes on without them, and the
/// log says so, rather than an `if let Ok` dropping them without a word.
async fn installed(store: &PgStore, account: &AccountId, bot: &CoworkerId) -> Vec<Installation> {
    installed::for_turn(store, account, bot)
        .await
        .inspect_err(|error| {
            tracing::warn!(%error, %bot, "installed plugins could not be read; the turn goes on without them");
        })
        .unwrap_or_default()
}

/// What one message asked of its plugins: those it tagged (`forwardedProps.mentionedPlugins`,
/// kept by [`mentioned`]) and the account picked on a "Which account?" card for this turn, by
/// plugin then connector (`forwardedProps.pluginAccounts`). Nothing in it is stored.
#[derive(Debug, Clone, Default)]
pub struct TurnPlugins {
    pub mentioned: Vec<String>,
    pub chosen: BTreeMap<String, BTreeMap<String, String>>,
}

impl TurnPlugins {
    fn chosen_for(&self, plugin: &str) -> BTreeMap<String, String> {
        self.chosen.get(plugin).cloned().unwrap_or_default()
    }
}

/// The plugins named in this turn's message that it may use: installed by the driving account,
/// on a Bot that account owns (`installed::for_turn` checks the owner in SQL). Anything else is
/// dropped here, so a member's `@name` on a shared Bot, or a name nobody installed, can never
/// widen a ceiling (`opengrok_policy::with_mentioned`).
pub async fn mentioned(
    store: &PgStore,
    account: &AccountId,
    bot: &CoworkerId,
    asked: &[String],
) -> Vec<String> {
    if asked.is_empty() {
        return Vec::new();
    }
    let installs = installed(store, account, bot).await;
    asked
        .iter()
        .filter(|name| installs.iter().any(|install| &install.name == *name))
        .cloned()
        .collect()
}

/// The plugins the driving account installed that are off for this turn of its own `bot`: neither
/// switched on nor tagged in the message. The turn is told their names, so a Bot asked for one
/// says how to use it (a tag) instead of hunting for its credentials (6 Oct 2026: a Bot with
/// Cloudflare off asked for a Cloudflare key through a password form). Nothing for a Bot that is
/// not the account's: a member's turn on a shared Bot is told of no install of the owner's.
pub async fn switched_off(
    store: &PgStore,
    account: &AccountId,
    bot: &CoworkerId,
    mentioned: &[String],
) -> Vec<String> {
    let installs = installed(store, account, bot).await;
    if installs.is_empty() {
        return Vec::new();
    }
    let Some(policy) = turn_policy(store, account, bot).await else {
        return Vec::new();
    };
    installs
        .into_iter()
        .map(|install| install.name)
        .filter(|name| !mentioned.contains(name) && !switched_on(account, bot, name, &policy))
        .collect()
}

/// Whether `bot` is `account`'s own and live: only its owner is asked to install or sign in from
/// a tag, since a member's tag on a shared Bot gives nothing (`mentioned`).
async fn owns(store: &PgStore, account: &AccountId, bot: &CoworkerId) -> bool {
    let owned = sqlx::query_scalar::<_, bool>(
        "select exists(select 1 from coworker_view where id = $1 and account_id = $2 and not retired)",
    )
    .bind(bot.as_str())
    .bind(account.as_str())
    .fetch_one(store.pool())
    .await;
    owned
        .inspect_err(|error| tracing::warn!(%error, %bot, "a Bot's owner could not be read"))
        .unwrap_or(false)
}

/// The CUSTOM name a turn answers with, instead of asking the model, when a plugin the message
/// tagged cannot be used yet (#360).
pub const PLUGIN_NEEDS: &str = "opengrok.pluginNeeds";

/// What each tagged plugin still needs before this turn can use it, as the card lists them; empty
/// when the turn can go ahead (`PLUGIN_NEEDS`):
///
/// - `{"plugin", "need": "install"}`: tagged but not installed;
/// - `{"plugin", "connector", "need": "choose", "accounts": [{"id", "label", "kind"}]}`: several
///   accounts and neither a pin nor this turn's pick says which. THE BOT NEVER GUESSES;
/// - `{"plugin", "connector", "need": "account"}`: no account, for a service that needs one.
///
/// Read before the model is asked, so a turn never starts on a plugin it would have to sit out,
/// and the answer re-sends the same message with the pick (`TurnPlugins::chosen`) or after the
/// install or sign-in. Nothing for a member's tag on a shared Bot: it gives nothing to ask about.
pub async fn needs(
    store: &PgStore,
    vault: Option<&Vault>,
    account: &AccountId,
    bot: &CoworkerId,
    asked: &[String],
    turn: &TurnPlugins,
) -> Vec<serde_json::Value> {
    if asked.is_empty() || !owns(store, account, bot).await {
        return Vec::new();
    }
    let mut needs = Vec::new();
    for plugin in asked.iter().filter(|name| !turn.mentioned.contains(name)) {
        needs.push(serde_json::json!({ "plugin": plugin, "need": "install" }));
    }
    let Some(vault) = vault else {
        return needs;
    };
    for installation in installed(store, account, bot).await {
        if !turn.mentioned.contains(&installation.name) {
            continue;
        }
        let plugin = &installation.name;
        let chosen = turn.chosen_for(plugin);
        let read = installed::values_choosing(store, vault, account, bot, &installation, &chosen);
        let credentials = match read.await {
            Ok(credentials) => credentials,
            Err(error) => {
                tracing::warn!(plugin, %error, "a tagged plugin's accounts could not be read");
                continue;
            }
        };
        for (connector, accounts) in credentials.choices {
            let accounts: Vec<serde_json::Value> = accounts
                .into_iter()
                .map(
                    |(id, label, kind)| serde_json::json!({"id": id, "label": label, "kind": kind}),
                )
                .collect();
            needs.push(serde_json::json!({
                "plugin": plugin, "connector": connector, "need": "choose", "accounts": accounts,
            }));
        }
        for connector in credentials.missing {
            // A placeholder needs a token outright. A server that declares no auth may be keyless,
            // and is asked whether it has a sign-in of its own (remembered a while).
            let required = installation.bundle.needs_a_token(&connector)
                || match crate::mcp_oauth::server_url(&installation.bundle, &connector) {
                    Some(url) => {
                        crate::mcp_oauth::offers_sign_in(&crate::mcp_oauth::http(), &url).await
                    }
                    None => false,
                };
            if required {
                needs.push(serde_json::json!({
                    "plugin": plugin, "connector": connector, "need": "account",
                }));
            }
        }
    }
    needs
}

/// Every server of an installed plugin this turn may reach, hardened (`crate::net`). `operator`
/// names a deployment plugin, which keeps its name: account bundles never shadow one. A plugin
/// `turn` tagged is dialled although its switch is off, since the person asked for it by name,
/// with the account the turn picked where it picked one.
pub async fn endpoints(
    store: &PgStore,
    vault: Option<&Vault>,
    account: &AccountId,
    bot: &CoworkerId,
    policy: &Context,
    turn: &TurnPlugins,
    operator: impl Fn(&str) -> bool,
) -> Vec<Endpoint> {
    let mut endpoints = Vec::new();
    // Account bundles never inherit deployment-wide or bot-lent tokens. Each plugin's own
    // credential namespace is resolved only after the driving account owns this bot.
    for installation in installed(store, account, bot).await {
        // Switched on by name, like its skills: `All` does not dial a bundle nobody chose.
        let asked = turn.mentioned.contains(&installation.name);
        if operator(&installation.name)
            || !(asked || switched_on(account, bot, &installation.name, policy))
        {
            continue;
        }
        let plugin = &installation.name;
        let values = match vault {
            Some(vault) => {
                // An MCP account about to lapse is refreshed first (#364), so the turn is not
                // handed a token its server refuses a minute later.
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis() as i64);
                let http = crate::mcp_oauth::http();
                let public = crate::mcp_oauth::PUBLIC;
                crate::mcp_oauth::refresh_due(&http, public, store, vault, account, plugin, now)
                    .await;
                let chosen = turn.chosen_for(plugin);
                match installed::values_choosing(store, vault, account, bot, &installation, &chosen)
                    .await
                {
                    Ok(turn) if turn.needs_choice.is_empty() => turn.values,
                    // Several accounts and no pin: nobody has said which one this Bot acts as, and
                    // a guess would act as the wrong one. The plugin sits this turn out, as one
                    // whose server will not connect does, until #360 asks the person.
                    Ok(turn) => {
                        let connectors = turn.needs_choice;
                        tracing::info!(plugin, ?connectors, %bot, "an installed plugin needs a choice of account; it is unavailable this turn");
                        continue;
                    }
                    Err(error) => {
                        tracing::warn!(plugin, %error, "installed plugin credentials unavailable");
                        continue;
                    }
                }
            }
            None => BTreeMap::new(),
        };
        let plugin = installation.bundle.plugin_with_values(&values);
        let (reachable, problems) = opengrok_tools::mcp::endpoints_for(&plugin, &values);
        for problem in problems {
            tracing::debug!(%problem, plugin = installation.name, "an installed plugin server is unavailable");
        }
        endpoints.extend(
            reachable
                .into_iter()
                .filter(|endpoint| {
                    let public = crate::net::public_url(&endpoint.url);
                    if !public {
                        // Fail closed and say why: install refuses these, so one here is a row
                        // stored before that check, and silence would read as "no tools".
                        let server = endpoint.key();
                        tracing::warn!(
                            server,
                            "an installed plugin server is not on a public host; not dialled"
                        );
                    }
                    public
                        && opengrok_policy::may_run_any_under(
                            account,
                            bot,
                            &endpoint.qualify(""),
                            policy,
                        )
                })
                .map(|mut endpoint| {
                    endpoint.harden = Some(crate::net::harden);
                    endpoint
                }),
        );
    }
    endpoints
}

/// The policy a turn of `bot` runs under, as the turn reads it (`policy_to_use`), so a skill is
/// never judged by a different answer than the tools beside it.
async fn turn_policy(store: &PgStore, account: &AccountId, bot: &CoworkerId) -> Option<Context> {
    store
        .policy_to_use(account, bot)
        .await
        .inspect_err(|error| {
            tracing::warn!(%error, %bot, "a Bot's policy could not be read; no plugin skills");
        })
        .ok()
}

/// The skills of every installed plugin switched on for this Bot, named `<plugin>.<skill>`. Only
/// the skills are read: a bundle's files can be megabytes, and every turn asks this.
pub async fn skill_offers(
    store: &PgStore,
    account: &AccountId,
    bot: &CoworkerId,
) -> Vec<SkillOffer> {
    let installs = installed::skills_for_turn(store, account, bot)
        .await
        .inspect_err(|error| {
            tracing::warn!(%error, %bot, "installed plugin skills could not be read; none offered");
        })
        .unwrap_or_default();
    if installs.is_empty() {
        return Vec::new();
    }
    let Some(policy) = turn_policy(store, account, bot).await else {
        return Vec::new();
    };
    let mut offers = Vec::new();
    for (plugin, revision, skills) in &installs {
        if !switched_on(account, bot, plugin, &policy) {
            continue;
        }
        for (skill, text) in skills {
            if opengrok_policy::skill_switched_off(&policy, plugin, skill) {
                continue;
            }
            let name = format!("{plugin}.{skill}");
            if opengrok_plugins::is_valid_name(&name) {
                let description = opengrok_plugins::split_frontmatter(text).description;
                offers.push(SkillOffer {
                    id: format!("plugin/{bot}/{plugin}/{revision}/{skill}"),
                    name,
                    description: description.unwrap_or_default(),
                });
            }
        }
    }
    offers
}

/// A plugin skill as `use_skill` reads it, by the id `skill_offers` gave it.
pub struct PluginSkill {
    pub body: String,
    /// Its supporting files, by path under the skill's own folder.
    pub files: Vec<(String, Vec<u8>)>,
    /// Where on the coworker's computer they go: plugin, skill and revision, so two revisions
    /// never share a folder.
    pub dir: String,
}

/// Whether `id` is one `skill_offers` minted.
pub fn is_plugin_skill(id: &str) -> bool {
    id.starts_with("plugin/")
}

/// `None` when it is not this account's, no longer installed at that revision, or switched off
/// since it was offered.
pub async fn skill(store: &PgStore, account: &AccountId, id: &str) -> Option<PluginSkill> {
    let parts: Vec<&str> = id.split('/').collect();
    let ["plugin", bot, name, revision, skill] = parts.as_slice() else {
        return None;
    };
    let bot = CoworkerId::from_stored(*bot);
    let policy = turn_policy(store, account, &bot).await?;
    if !switched_on(account, &bot, name, &policy)
        || opengrok_policy::skill_switched_off(&policy, name, skill)
    {
        tracing::warn!(
            skill = id,
            "a plugin skill was asked for while its plugin is switched off"
        );
        return None;
    }
    let installation = installed(store, account, &bot)
        .await
        .into_iter()
        .find(|p| p.name == *name && p.revision == *revision)?;
    let text = installation.bundle.skills.get(*skill)?;
    let prefix = format!("skills/{skill}/");
    let files = installation
        .bundle
        .files
        .iter()
        .filter_map(|(path, text)| {
            let path = path.strip_prefix(&prefix)?;
            Some((path.to_string(), text.as_bytes().to_vec()))
        })
        .collect();
    Some(PluginSkill {
        body: opengrok_plugins::split_frontmatter(text).body,
        files,
        dir: format!(".skills/plugin-{name}/{skill}/{revision}"),
    })
}

#[cfg(test)]
#[path = "../tests/unit/turn.rs"]
mod tests;
