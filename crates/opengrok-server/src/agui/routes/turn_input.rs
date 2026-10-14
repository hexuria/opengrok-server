//! Turn input preferences, mentions, recipes, and skills.

use super::*;

/// Tools the person named for THIS turn, e.g. by typing `@websearch` in the composer.
///
/// A preference, not a restriction: every tool the bot may run stays on offer, because narrowing
/// the set would turn a hint into a cage and strand a turn that needed one more tool. What the
/// name buys is that the model is told, in the system message, which tools the person reached
/// for — the thing a person means when they type one.
pub(super) fn preferred_tools_from(input: &RunAgentInput) -> Vec<String> {
    input
        .forwarded_props
        .get("preferTools")
        .and_then(|value| value.as_array())
        .map(|names| {
            names
                .iter()
                .filter_map(|name| name.as_str())
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The most plugins one message may name. A composer puts one tag per plugin, and a body naming
/// hundreds is not a person typing: each name costs an install lookup.
const MAX_MENTIONED_PLUGINS: usize = 16;

/// The plugins the person tagged in THIS message (`@cloudflare`), by install name.
///
/// UNLIKE `preferTools`, A MENTION GIVES ACCESS: a plugin switched off for this Bot is switched on
/// for this one turn, because typing its name is the person asking for it. Who may ask is
/// `turn::mentioned`'s to decide; this only reads the field, bounded, each name of a shape a
/// plugin name can have.
// Provenance: the client sends `forwardedProps.mentionedPlugins: ["cloudflare"]` (#359).
pub(super) fn mentioned_plugins_from(input: &RunAgentInput) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let listed = input
        .forwarded_props
        .get("mentionedPlugins")
        .and_then(|value| value.as_array());
    for name in listed
        .into_iter()
        .flatten()
        .filter_map(|name| name.as_str())
    {
        let name = name.trim();
        let usable = opengrok_plugins::is_valid_name(name) && !name.contains('.');
        if usable && !names.iter().any(|seen| seen == name) {
            names.push(name.to_string());
        }
        if names.len() == MAX_MENTIONED_PLUGINS {
            break;
        }
    }
    names
}

/// The account picked on a "Which account?" card for THIS turn, by plugin then connector:
/// `{"cloudflare": {"cloudflare": "conn_…"}}`. Only ever narrows which of the person's own
/// accounts is used (`installed::values_choosing`); bounded like the tags.
// Provenance: the client sends `forwardedProps.pluginAccounts` when a card's answer re-sends (#360).
pub(super) fn plugin_accounts_from(
    input: &RunAgentInput,
) -> BTreeMap<String, BTreeMap<String, String>> {
    let Some(plugins) = input
        .forwarded_props
        .get("pluginAccounts")
        .and_then(|value| value.as_object())
    else {
        return BTreeMap::new();
    };
    plugins
        .iter()
        .take(MAX_MENTIONED_PLUGINS)
        .filter_map(|(plugin, connectors)| {
            let picks: BTreeMap<String, String> = connectors
                .as_object()?
                .iter()
                .take(MAX_MENTIONED_PLUGINS)
                .filter_map(|(connector, id)| Some((connector.clone(), id.as_str()?.to_string())))
                .collect();
            Some((plugin.clone(), picks))
        })
        .collect()
}

/// The whole answer to a turn whose tagged plugins need something first: the run starts, says
/// what each needs (`turn::PLUGIN_NEEDS`), and finishes. No model is asked and nothing is
/// recorded, since the card's answer sends the same message again as a turn of its own.
pub(super) fn plugin_needs_answer(
    input: &RunAgentInput,
    needs: Vec<serde_json::Value>,
) -> Response {
    let mut projection =
        opengrok_harness::Projection::new(&input.thread_id, &input.run_id, now_ms());
    let mut events = projection.start();
    let value = serde_json::json!({ "needs": needs });
    events.push(projection.custom(opengrok_integrations::turn::PLUGIN_NEEDS, value));
    events.extend(projection.finish());
    sse(futures::stream::iter(
        events.into_iter().map(Ok::<_, std::io::Error>),
    ))
}

/// The named tools that this bot can actually run, in the order they were named.
///
/// A name the bot was never offered is dropped rather than repeated: a system message that
/// names a tool the model has not been given is an instruction it cannot follow, and the model
/// spends the turn looking for it.
pub(super) fn honour_preferences(preferred: &[String], runner: Option<&ToolRunner>) -> Vec<String> {
    let Some(runner) = runner else {
        return Vec::new();
    };
    let offered: Vec<String> = runner
        .tool_schemas()
        .into_iter()
        .filter_map(|schema| Some(schema.get("function")?.get("name")?.as_str()?.to_string()))
        .collect();
    preferred
        .iter()
        .filter_map(|name| {
            offered
                .iter()
                .find(|offered| {
                    let offered = offered.as_str();
                    offered == name || offered == opengrok_tools::openai_safe_tool_name(name)
                })
                .cloned()
        })
        .collect()
}

/// The recipe the person picked in the composer this turn, and the values they typed for it.
///
/// A person who chose a recipe and filled in its fields has said something more definite than a
/// sentence: they named the task and its inputs. Carrying that through means the model does not
/// have to infer a search term from prose — which is exactly how one turn came to search a word
/// lifted from the conversation instead of the one that was asked for.
pub(in crate::agui) fn chosen_recipe_from(
    input: &RunAgentInput,
) -> Option<(String, BTreeMap<String, String>)> {
    let recipe = input
        .forwarded_props
        .get("recipe")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|id| !id.is_empty())?
        .to_string();
    Some((
        recipe,
        recipe_values_from(input.forwarded_props.get("recipeValues")),
    ))
}

/// A recipe's inputs as the text its steps receive. Also how a queued send's saved values are
/// read, so a saved send and the turn that fires it cannot disagree about what was chosen.
pub(in crate::agui) fn recipe_values_from(
    values: Option<&serde_json::Value>,
) -> BTreeMap<String, String> {
    values
        .and_then(|value| value.as_object())
        .map(|object| {
            object
                .iter()
                .filter_map(|(name, value)| {
                    // A value is text by the time it reaches a step, but a client may send a
                    // number or a boolean as itself rather than as a string.
                    let text = match value {
                        serde_json::Value::String(text) => text.clone(),
                        serde_json::Value::Number(number) => number.to_string(),
                        serde_json::Value::Bool(flag) => flag.to_string(),
                        _ => return None,
                    };
                    Some((name.clone(), text))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The most a `forwardedProps.skill` may be before it is refused unread.
///
/// Ours are `skl_` plus a UUID, 40 characters, or a plugin skill's id, at most 217. The bound is
/// small enough that a 2 MB `skill` field is refused before it reaches a log line: this value is
/// caller-controlled, and a WARN that carries it is a record whose size the caller picks.
pub(super) const MAX_SKILL_ID_CHARS: usize = 256;

/// What `forwardedProps.skill` said this turn.
pub(in crate::agui) enum ChosenSkill {
    /// An id worth looking up: bounded, and of a shape an id we mint could have.
    Id(String),
    /// Something was sent and it cannot be an id — a number, an object, two megabytes of text. NOT
    /// the same as nothing chosen, and that distinction is the point: read as "no skill", a client
    /// that changed the field's shape would stop applying skills with no refusal, no log and
    /// nothing anywhere for anybody to notice. Carries the KIND, never the value.
    Unusable(&'static str),
}

/// The skill the person chose in the composer this turn, by id.
///
/// `forwardedProps.skill`, arriving exactly the way `chosen_recipe_from` reads a recipe. THERE IS
/// NO `/name` PARSED OUT OF THE MESSAGE TEXT, here or anywhere: the composer already knows which
/// skill the person picked from the list it drew them, and a second answer read out of their prose
/// would fire on any message that happened to begin with a slash — a path, a date, a command they
/// were quoting.
///
/// THIS REQUEST WINS. When it names no skill, [`skill_line_for_turn`] reuses the skill from the
/// newest prior run on this thread that had one. A different thread does not. A run suspended on
/// a card resumes on the message it opened with (`run.system_for_resume`), which is the same turn
/// and therefore the same skill — WHEN one was captured. A run journalled before `system` was
/// recorded has none, and both resume sites then compose identity and role with no tail at all,
/// losing the skill along with the whose-computer discipline and the network line. That gap
/// predates skills, and no turn that quotes one can reach it: a turn that composes a skill also
/// journals the message it composed.
pub(in crate::agui) fn chosen_skill_from(input: &RunAgentInput) -> Option<ChosenSkill> {
    let value = input.forwarded_props.get("skill")?;
    // Absent and null are "no skill chosen", and so is blank — a composer that always sends the
    // key sends an empty string when nothing is picked. Everything else is a claim about a skill.
    if value.is_null() {
        return None;
    }
    let Some(id) = value.as_str() else {
        return Some(ChosenSkill::Unusable("not a string"));
    };
    let id = id.trim();
    if id.is_empty() {
        return None;
    }
    if id.len() > MAX_SKILL_ID_CHARS {
        return Some(ChosenSkill::Unusable("longer than any id"));
    }
    // Bounded above, and here restricted to characters an id we mint can contain. A newline in
    // this value would otherwise be written into a WARN, where it forges a whole log record at a
    // position the caller chooses.
    let byte = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-';
    if opengrok_integrations::turn::plugin_skill_bot(id).is_none() && !id.bytes().all(byte) {
        return Some(ChosenSkill::Unusable("not shaped like an id"));
    }
    Some(ChosenSkill::Id(id.to_string()))
}

/// The skill segment of this turn's system message — the person's instructions quoted between
/// unforgeable markers, and the id to record on `RunEvent::Started` — or the sentence saying they
/// were not, and no id, so a skill that cannot be given does not stick.
///
/// EVERY PATH OUT OF HERE SAYS SOMETHING. A chosen skill that cannot be given is the case the
/// refusal line exists for, so returning an empty string on any of these would be precisely the
/// silence it was written to prevent.
async fn skill_segment(
    state: &AgUiState,
    (account, coworker): (&AccountId, &CoworkerId),
    chosen: &ChosenSkill,
    tools: Option<&opengrok_harness::ToolRunner>,
) -> (String, Option<String>) {
    // EVERY REFUSAL BELOW GOES THROUGH `NotForThisTurn::line`, including the two this function
    // decides itself. A sentence chosen at the call site is a sentence that drifts from the table
    // that decides the rest.
    let id = match chosen {
        ChosenSkill::Unusable(kind) => {
            // The VALUE is not logged. It is caller-controlled and unbounded, so a WARN carrying
            // it is a log record whose size and contents the caller writes; the kind is what tells
            // a client shape change from somebody probing.
            let why = crate::skills::NotForThisTurn::NotAnId;
            tracing::warn!(kind, why = ?why, "a turn carried a `skill` that cannot be an id");
            return (why.line().to_string(), None);
        }
        ChosenSkill::Id(id) => id,
    };
    if opengrok_integrations::turn::is_plugin_skill(id) {
        return plugin_skill_segment(state, (account, coworker), id, tools).await;
    }
    let skill = match crate::skills::for_turn(state, account, id).await {
        Ok(skill) => skill,
        Err(why) => {
            // `?` on both, not `%`: the id is caller-controlled and `why` carries a store error
            // verbatim, and Display writes a newline as a newline — one forged log record per
            // request, at a position the caller picks. Which case it was is recorded here and
            // nowhere else: the sentence the person reads names no cause at all.
            tracing::warn!(skill = ?id, why = ?why, "a chosen skill was not given to a turn");
            return (why.line().to_string(), None);
        }
    };
    let files = |target| crate::skills::files_line_for_turn(state, &skill, target);
    quote_chosen(id, (&skill.name, &skill.body), skill.author, tools, files).await
}

/// [`skill_segment`] for a plugin skill, by its `plugin-skills` row's id (`turn::chosen_skill`).
async fn plugin_skill_segment(
    state: &AgUiState,
    (account, coworker): (&AccountId, &CoworkerId),
    id: &str,
    tools: Option<&opengrok_harness::ToolRunner>,
) -> (String, Option<String>) {
    use opengrok_integrations::turn::{self, NotChosen};
    let (store, plugins) = (&state.auth.store, &state.plugins);
    let skill = match turn::chosen_skill(store, account, coworker, id, plugins).await {
        Ok(skill) => skill,
        Err(not) => {
            let why = match not {
                NotChosen::OtherBot => crate::skills::NotForThisTurn::NotTheirs,
                NotChosen::Unread => crate::skills::NotForThisTurn::Disabled,
            };
            tracing::warn!(skill = ?id, why = ?why, "a chosen plugin skill was not given to a turn");
            return (why.line().to_string(), None);
        }
    };
    let author = crate::persona::SkillAuthor::Plugin;
    let files = |target| crate::skills::files_line(skill.files, &skill.dir, author, id, target);
    quote_chosen(id, (&skill.name, &skill.body), author, tools, files).await
}

/// A chosen skill fenced between unforgeable markers, `files` before the closing line so our rules
/// stay the last word; or the refusal, and no id, when no marker could be minted.
async fn quote_chosen<F: std::future::Future<Output = Option<String>>>(
    id: &str,
    (name, body): (&str, &str),
    author: crate::persona::SkillAuthor,
    tools: Option<&opengrok_harness::ToolRunner>,
    files: impl FnOnce(Option<(Arc<dyn opengrok_box::Computer>, String)>) -> F,
) -> (String, Option<String>) {
    let quoted = match crate::persona::skill_marker(body) {
        Some(marker) => {
            let target = tools
                .and_then(ToolRunner::fill_target)
                .map(|(computer, box_id, _)| (computer, box_id));
            let files = files(target).await.unwrap_or_default();
            let door = crate::persona::SkillDoor::Chosen;
            crate::persona::fenced_skill(door, name, body, &marker, author, &files)
        }
        None => String::new(),
    };
    if quoted.is_empty() {
        // No marker the body does not already contain, so the quote could not be closed where
        // we say it closes. Refuse rather than quote it unbounded.
        let why = crate::skills::NotForThisTurn::Unquotable;
        tracing::warn!(skill = ?id, why = ?why, "a chosen skill could not be quoted safely");
        return (why.line().to_string(), None);
    }
    (quoted, Some(id.to_string()))
}

/// The skill line for this turn, and the id to record on `RunEvent::Started`.
///
/// An id on this request is used as sent. None means look at prior runs on this
/// thread, newest first, and take the first that quoted a skill.
pub(super) async fn skill_line_for_turn(
    state: &AgUiState,
    (account, coworker): (&AccountId, &CoworkerId),
    thread_id: &str,
    input: &RunAgentInput,
    tools: Option<&opengrok_harness::ToolRunner>,
) -> (String, Option<String>) {
    let chosen = match chosen_skill_from(input) {
        Some(chosen) => chosen,
        None => match inherited_skill_id(state, account, thread_id).await {
            Some(id) => ChosenSkill::Id(id),
            None => return (String::new(), None),
        },
    };
    skill_segment(state, (account, coworker), &chosen, tools).await
}

/// Newest prior run on this thread, owned by this account, that recorded a quoted skill
/// (`RunEvent::Started::skill_id`). Walks past a turn that recorded none: that is the follow-up
/// which dropped the skill (run 01a0c9ef) and the turn before it still has the body.
///
/// NEVER READ BACK OUT OF A SYSTEM MESSAGE. An attached skill's description sits in that message
/// on every turn, and one that wrote the sentence a chosen skill opens with would choose a skill
/// for the next turn (review of #290). A run from before the id was recorded inherits nothing.
async fn inherited_skill_id(
    state: &AgUiState,
    account: &AccountId,
    thread_id: &str,
) -> Option<String> {
    let store = &state.auth.store;
    let runs = store.runs_for_thread_owned_by(thread_id, account, 8).await;
    for run in runs.ok()? {
        let loaded = match store.load_run(&run.id).await {
            Ok((loaded, _)) => loaded,
            Err(error) => {
                tracing::warn!(%error, "could not read a prior run while inheriting a skill");
                continue;
            }
        };
        let recorded = loaded.skill_id.as_deref().map(str::trim);
        if let Some(id) = recorded.filter(|id| !id.is_empty()) {
            return Some(id.to_string());
        }
    }
    None
}

pub(super) fn coworker_id_from(input: &RunAgentInput) -> Option<CoworkerId> {
    input
        .forwarded_props
        .get("coworkerId")
        .and_then(|value| value.as_str())
        .map(|id| CoworkerId::from_stored(id.to_string()))
}
