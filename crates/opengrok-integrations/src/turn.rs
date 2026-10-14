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
use opengrok_plugins::Plugin;
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
    /// Whether the app that sent this turn draws a plugin's form card (`forwardedProps.formMode`,
    /// #220); `None` when no app sent it (a routine, a run carried on after a card).
    pub form_mode: Option<bool>,
}

impl TurnPlugins {
    fn chosen_for(&self, plugin: &str) -> BTreeMap<String, String> {
        self.chosen.get(plugin).cloned().unwrap_or_default()
    }
}

/// The plugins named in this turn's message that it may use: installed by the driving account,
/// or in this server's own folder (`operator`), which every account has, on a Bot that account
/// owns (`installed::for_turn` checks the owner in SQL, `owns` for the server's plugins). Anything
/// else is dropped here, so a member's `@name` on a shared Bot, or a name nobody installed, can
/// never widen a ceiling (`opengrok_policy::with_mentioned`).
pub async fn mentioned(
    store: &PgStore,
    account: &AccountId,
    bot: &CoworkerId,
    asked: &[String],
    operator: &BTreeMap<String, Plugin>,
) -> Vec<String> {
    if asked.is_empty() {
        return Vec::new();
    }
    let installs = installed(store, account, bot).await;
    let server =
        asked.iter().any(|name| operator.contains_key(name)) && owns(store, account, bot).await;
    asked
        .iter()
        .filter(|name| {
            installs.iter().any(|install| &install.name == *name)
                || (server && operator.contains_key(*name))
        })
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

/// Every plugin this Bot could reach that declares `extensions.formMode` (#220), by name: this
/// server's own first, then the account's installs, less any a server plugin shadows, as their
/// servers are (`endpoints`). Whether one is switched on is not asked here: its `alwaysAsk`
/// only narrows, and a card's actions are widened to its plugin by `mentioned`'s own rule.
pub async fn form_mode_plugins(
    store: &PgStore,
    account: &AccountId,
    bot: &CoworkerId,
    operator: &BTreeMap<String, Plugin>,
) -> Vec<(String, opengrok_plugins::FormModeTools)> {
    let server = operator
        .values()
        .filter(|p| !p.manifest.name.contains('.'))
        .filter_map(|p| Some((p.manifest.name.clone(), p.form_mode()?)));
    let mut found: Vec<_> = server.collect();
    for installation in installed(store, account, bot).await {
        if operator.contains_key(&installation.name) || installation.name.contains('.') {
            continue;
        }
        if let Some(tools) = installation.bundle.form_mode() {
            found.push((installation.name, tools));
        }
    }
    found
}

/// `form_mode_plugins`, less those this turn may not reach of its own accord: switched off for
/// this Bot and not tagged in its message. What a model's call to open a card may be widened to;
/// a person's press on a card already open is not narrowed by it.
pub async fn form_mode_plugins_on(
    (store, operator): (&PgStore, &BTreeMap<String, Plugin>),
    account: &AccountId,
    bot: &CoworkerId,
    tagged: &[String],
) -> Vec<(String, opengrok_plugins::FormModeTools)> {
    let Some(policy) = turn_policy(store, account, bot).await else {
        return Vec::new();
    };
    let on = |name: &str| match operator.contains_key(name) {
        true => opengrok_policy::admits_plugin(account, bot, name, &policy),
        false => switched_on(account, bot, name, &policy),
    };
    let plugins = form_mode_plugins(store, account, bot, operator).await;
    plugins
        .into_iter()
        .filter(|(name, _)| tagged.contains(name) || on(name))
        .collect()
}

/// Every tool of a plugin this Bot could reach that always asks a person first (#220), by
/// internal name: `opengrok_plugins::form_mode::always_ask_tools`, over the plugins
/// `form_mode_plugins` reads, card block usable or not. It only narrows, so nothing is skipped
/// for being switched off.
pub async fn always_ask_tools(
    store: &PgStore,
    account: &AccountId,
    bot: &CoworkerId,
    operator: &BTreeMap<String, Plugin>,
) -> Vec<String> {
    let server = operator.values().filter(|p| !p.manifest.name.contains('.'));
    let mut tools: Vec<String> = server
        .flat_map(|p| p.always_ask_tools(&p.manifest.name))
        .collect();
    for installation in installed(store, account, bot).await {
        if !operator.contains_key(&installation.name) && !installation.name.contains('.') {
            tools.extend(installation.bundle.always_ask_tools(&installation.name));
        }
    }
    tools
}

/// The policy a Bot's tools run under on one turn, as every turn and the MCP door build it: the
/// Bot's own (`policy_to_use`), widened to the plugins this message tagged
/// (`opengrok_policy::with_mentioned`), and narrowed by every form-mode plugin's `alwaysAsk`
/// (`form_mode::always_asking`). `None` when the Bot's policy could not be read.
pub async fn tools_policy(
    (store, operator): (&PgStore, &BTreeMap<String, Plugin>),
    account: &AccountId,
    bot: &CoworkerId,
    mentioned: &[String],
) -> Option<Context> {
    let policy = store.policy_to_use(account, bot).await.ok()?;
    let policy = opengrok_policy::with_mentioned(policy, mentioned);
    let reach = (store, operator);
    Some(crate::form_mode::always_asking(reach, account, bot, policy).await)
}

/// Whether a Bot is offered `request_user_form` for a plugin's form (#220): the app that sent
/// the turn draws the card, and a plugin the model may reach (switched on, or tagged) has one.
pub async fn offers_form_cards(
    reach: (&PgStore, &BTreeMap<String, Plugin>),
    account: &AccountId,
    bot: &CoworkerId,
    turn: &TurnPlugins,
) -> bool {
    turn.form_mode == Some(true)
        && !form_mode_plugins_on(reach, account, bot, &turn.mentioned)
            .await
            .is_empty()
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

/// The revision a skill of a plugin in this server's own folder is offered at. Such a plugin has
/// no pinned commit, and an account's install always has a 40-hex one (`registry::revision_ok`),
/// so an id minted for one can never be read as the other's.
pub const SERVER_REVISION: &str = "server";

/// The plugins in this server's own folder (`OG_PLUGINS_DIR`) switched on for this turn of `bot`,
/// by the switch their ceiling row shows (`opengrok_policy::admits_plugin`). Operator-installed,
/// so every account's Bots have them; a name with a dot is never dialled, so never offered either.
pub fn server_plugins_on<'a>(
    operator: &'a BTreeMap<String, Plugin>,
    account: &AccountId,
    bot: &CoworkerId,
    policy: &Context,
) -> Vec<&'a Plugin> {
    operator
        .values()
        .filter(|p| !p.manifest.name.contains('.'))
        .filter(|p| opengrok_policy::admits_plugin(account, bot, &p.manifest.name, policy))
        .collect()
}

/// The skills of every plugin switched on for this Bot, named `<plugin>.<skill>`: those in this
/// server's own folder (`operator`), then the account's installs. An install named like one of the
/// server's is shadowed, as its servers are (`endpoints`). Only the skills are read: a bundle's
/// files can be megabytes, and every turn asks this.
pub async fn skill_offers(
    store: &PgStore,
    account: &AccountId,
    bot: &CoworkerId,
    operator: &BTreeMap<String, Plugin>,
) -> Vec<SkillOffer> {
    let installs = installed::skills_for_turn(store, account, bot)
        .await
        .inspect_err(|error| {
            tracing::warn!(%error, %bot, "installed plugin skills could not be read; none offered");
        })
        .unwrap_or_default();
    if installs.is_empty() && operator.is_empty() {
        return Vec::new();
    }
    let Some(policy) = turn_policy(store, account, bot).await else {
        return Vec::new();
    };
    let mut offers = Vec::new();
    for plugin in server_plugins_on(operator, account, bot, &policy) {
        let name = &plugin.manifest.name;
        offer(
            &mut offers,
            bot,
            name,
            SERVER_REVISION,
            &plugin.skills,
            &policy,
        );
    }
    for (plugin, revision, skills) in &installs {
        if operator.contains_key(plugin) || !switched_on(account, bot, plugin, &policy) {
            continue;
        }
        offer(&mut offers, bot, plugin, revision, skills, &policy);
    }
    offers
}

/// Each of `plugin`'s skills not switched off on its own, as an offer `skill` can read back.
fn offer(
    offers: &mut Vec<SkillOffer>,
    bot: &CoworkerId,
    plugin: &str,
    revision: &str,
    skills: &BTreeMap<String, String>,
    policy: &Context,
) {
    for (skill, text) in skills {
        if opengrok_policy::skill_switched_off(policy, plugin, skill) {
            continue;
        }
        let name = format!("{plugin}.{skill}");
        if opengrok_plugins::is_valid_name(&name) {
            let description = opengrok_plugins::split_frontmatter(text).description;
            offers.push(SkillOffer {
                id: plugin_skill_id(bot, plugin, revision, skill),
                name,
                description: description.unwrap_or_default(),
            });
        }
    }
}

/// A Bot's `plugin-skills` rows, each with the id a turn takes it by (`forwardedProps.skill`), as
/// `use_skill` is offered it: this server's own plugins while switched on, since every account
/// has them, then the account's installs whatever their switch, less any a server plugin shadows.
pub async fn skill_rows(
    store: &PgStore,
    account: &AccountId,
    bot: &CoworkerId,
    operator: &BTreeMap<String, Plugin>,
) -> opengrok_store::StoreResult<Vec<serde_json::Value>> {
    let installs = installed::skills_for_turn(store, account, bot).await?;
    let policy = store.policy_for(account, bot).await?;
    let server = server_plugins_on(operator, account, bot, &policy).into_iter();
    let server = server.map(|p| {
        let name = p.manifest.name.as_str();
        (name, "server", SERVER_REVISION, &p.skills)
    });
    let installs = installs
        .iter()
        .filter(|(p, ..)| !operator.contains_key(p))
        .map(|(p, revision, s)| (p.as_str(), "account", revision.as_str(), s));
    let mut rows = Vec::new();
    for (plugin, source, revision, skills) in server.chain(installs) {
        for (skill, text) in skills {
            let front = opengrok_plugins::split_frontmatter(text);
            rows.push(serde_json::json!({
                "id": plugin_skill_id(bot, plugin, revision, skill),
                "plugin": plugin,
                "source": source,
                "skill": skill,
                "description": front.description.unwrap_or_default(),
                "on": !opengrok_policy::skill_switched_off(&policy, plugin, skill),
                "body": front.body,
            }));
        }
    }
    Ok(rows)
}

/// The id a plugin skill is offered by, and picked by in the composer (`plugin-skills` rows):
/// `plugin/<bot>/<plugin>/<revision>/<skill>`, read back by [`skill`].
pub fn plugin_skill_id(bot: &CoworkerId, plugin: &str, revision: &str, skill: &str) -> String {
    format!("plugin/{bot}/{plugin}/{revision}/{skill}")
}

/// The Bot of an id shaped as [`plugin_skill_id`] mints one: five segments, none empty and none
/// climbing (`..`), each of the characters a Bot id, a name or a revision has. `None` for any other.
pub fn plugin_skill_bot(id: &str) -> Option<&str> {
    let parts: Vec<&str> = id.split('/').collect();
    let ["plugin", bot, plugin, revision, skill] = parts.as_slice() else {
        return None;
    };
    let byte = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.');
    let shaped = |s: &&str| !s.is_empty() && s.bytes().all(byte);
    let segments = [bot, plugin, revision, skill];
    (!id.contains("..") && segments.into_iter().all(shaped)).then_some(*bot)
}

/// A plugin skill as `use_skill` reads it, by the id `skill_offers` gave it.
pub struct PluginSkill {
    /// The name `use_skill` offers it by, `<plugin>.<skill>`.
    pub name: String,
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

/// Why a plugin skill picked in the composer is not given to the turn.
#[derive(Debug, PartialEq, Eq)]
pub enum NotChosen {
    /// Its id names another Bot: what is offered to one is not another's, whoever owns both.
    OtherBot,
    /// [`skill`] read nothing: switched off, uninstalled, or not this account's; it logs which.
    Unread,
}

/// [`skill`] for a turn of `bot` that picked `id`, a `plugin-skills` row's id, in the composer:
/// read as `use_skill` reads one, and only on the Bot it was offered for.
pub async fn chosen_skill(
    store: &PgStore,
    account: &AccountId,
    bot: &CoworkerId,
    id: &str,
    operator: &BTreeMap<String, Plugin>,
) -> Result<PluginSkill, NotChosen> {
    if plugin_skill_bot(id) != Some(bot.as_str()) {
        return Err(NotChosen::OtherBot);
    }
    skill(store, account, id, operator)
        .await
        .ok_or(NotChosen::Unread)
}

/// `None` when it is not this account's, no longer installed at that revision, or switched off
/// since it was offered.
pub async fn skill(
    store: &PgStore,
    account: &AccountId,
    id: &str,
    operator: &BTreeMap<String, Plugin>,
) -> Option<PluginSkill> {
    let parts: Vec<&str> = id.split('/').collect();
    let ["plugin", bot, name, revision, skill] = parts.as_slice() else {
        return None;
    };
    let bot = CoworkerId::from_stored(*bot);
    let policy = turn_policy(store, account, &bot).await?;
    let on = match operator.get(*name) {
        Some(_) => opengrok_policy::admits_plugin(account, &bot, name, &policy),
        None => switched_on(account, &bot, name, &policy),
    };
    if !on || opengrok_policy::skill_switched_off(&policy, name, skill) {
        tracing::warn!(
            skill = id,
            "a plugin skill was asked for while its plugin is switched off"
        );
        return None;
    }
    if let Some(plugin) = operator.get(*name) {
        // An id minted for a shadowed install of the same name reads nothing.
        if *revision != SERVER_REVISION {
            return None;
        }
        return Some(read_skill(
            name,
            skill,
            revision,
            plugin.skills.get(*skill)?,
            &plugin.files,
        ));
    }
    let installation = installed(store, account, &bot)
        .await
        .into_iter()
        .find(|p| p.name == *name && p.revision == *revision)?;
    let text = installation.bundle.skills.get(*skill)?;
    Some(read_skill(
        name,
        skill,
        revision,
        text,
        &installation.bundle.files,
    ))
}

/// One skill's body and its files, wherever its plugin came from.
fn read_skill(
    name: &str,
    skill: &str,
    revision: &str,
    text: &str,
    files: &BTreeMap<String, String>,
) -> PluginSkill {
    let prefix = format!("skills/{skill}/");
    let files = files
        .iter()
        .filter_map(|(path, text)| {
            let path = path.strip_prefix(&prefix)?;
            Some((path.to_string(), text.as_bytes().to_vec()))
        })
        .collect();
    PluginSkill {
        name: format!("{name}.{skill}"),
        body: opengrok_plugins::split_frontmatter(text).body,
        files,
        dir: format!(".skills/plugin-{name}/{skill}/{revision}"),
    }
}

#[cfg(test)]
#[path = "../tests/unit/turn.rs"]
mod tests;
