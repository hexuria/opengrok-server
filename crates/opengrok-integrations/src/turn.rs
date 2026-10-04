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

/// Whether `plugin` is switched on for this turn: the ceiling and the grant both admit it whole,
/// which is what its row on the ceiling screen shows as on. Every missing piece answers no.
pub fn switched_on(account: &AccountId, bot: &CoworkerId, plugin: &str, policy: &Context) -> bool {
    let action = opengrok_policy::Action::UseCoworker;
    if !opengrok_policy::decide(account, bot, action, policy).is_allowed() {
        return false;
    }
    let (Some(grant), Some(ceiling)) = (&policy.grant, &policy.ceiling) else {
        return false;
    };
    &ceiling.coworker == bot
        && ceiling
            .tools
            .intersect(&grant.profile)
            .allows_all_of(plugin)
}

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

/// Every server of an installed plugin this turn may reach, hardened (`crate::net`). `operator`
/// names a deployment plugin, which keeps its name: account bundles never shadow one.
pub async fn endpoints(
    store: &PgStore,
    vault: Option<&Vault>,
    account: &AccountId,
    bot: &CoworkerId,
    policy: &Context,
    operator: impl Fn(&str) -> bool,
) -> Vec<Endpoint> {
    let mut endpoints = Vec::new();
    // Account bundles never inherit deployment-wide or bot-lent tokens. Each plugin's own
    // credential namespace is resolved only after the driving account owns this bot.
    for installation in installed(store, account, bot).await {
        if operator(&installation.name) {
            continue;
        }
        let values = match vault {
            Some(vault) => {
                match installed::values_for_installation(store, vault, account, &installation).await
                {
                    Ok(values) => values,
                    Err(error) => {
                        let plugin = &installation.name;
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
                    crate::net::public_url(&endpoint.url)
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

/// The skills of every installed plugin switched on for this Bot, named `<plugin>.<skill>`.
pub async fn skill_offers(
    store: &PgStore,
    account: &AccountId,
    bot: &CoworkerId,
) -> Vec<SkillOffer> {
    let installs = installed(store, account, bot).await;
    if installs.is_empty() {
        return Vec::new();
    }
    let policy = match store.policy_for(account, bot).await {
        Ok(policy) => policy,
        Err(error) => {
            tracing::warn!(%error, %bot, "a Bot's policy could not be read; no plugin skills offered");
            return Vec::new();
        }
    };
    let mut offers = Vec::new();
    for installation in installs
        .iter()
        .filter(|i| switched_on(account, bot, &i.name, &policy))
    {
        for (skill, text) in &installation.bundle.skills {
            let (plugin, revision) = (&installation.name, &installation.revision);
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
    let policy = store.policy_for(account, &bot).await.ok()?;
    if !switched_on(account, &bot, name, &policy) {
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
