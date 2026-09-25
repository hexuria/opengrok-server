//! `POST /ag-ui` — the endpoint openbot adds as a Bot.
//!
//! One request opens one SSE stream carrying the events of a single run. openbot supplies the
//! thread and run ids (`RunAgentInput`), so we do not mint them: the client correlates its own UI
//! against those values, and inventing our own would orphan the reply.
//!
//! THE ENVELOPE IS THE CONTRACT, EVEN WHEN THE MIDDLE IS A STUB. A run that starts must finish or
//! error — `RUN_STARTED` … `RUN_FINISHED` — because a consumer holds its spinner open on the
//! promise of that closing event. This slice streams a real, correctly-shaped conversation with a
//! placeholder body; slice 3 replaces the middle with the harness, and the framing does not change.

use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::{HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::Stream;
use opengrok_wire::agui::{Event, RunAgentInput};

use super::provision;
use crate::auth::AuthState;
use opengrok_core::coworker::{CoworkerCommand, CoworkerView};
use opengrok_core::id::{AccountId, CoworkerId, RunId};
use opengrok_core::run::{RunCommand, RunStatus, RunView};
use opengrok_harness::{
    ChatMessage, EventSink, ModelDoor, ModelRequest, ToolRunner, run_conversation_streaming,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// How long the first box-bound tool call of a turn waits for a sleeping box to come up before
/// answering that the computer is down. The wait moved from before the model was asked (every
/// turn paid it) to the tool that needs the box (only those turns pay it).
/// A box.ascii.dev resume restores a snapshot onto a fresh machine: archived → provisioned →
/// running took 10–15s live (bx_ncfmdpem, 2 Sep 2026); 90s leaves room for a slow restore.
pub(crate) const TURN_WAKE_PATIENCE: std::time::Duration = std::time::Duration::from_secs(90);

/// What the endpoint needs: a way to reach a model, and which route to ask for.
///
/// The door is a trait object so `OG_MODEL_DOOR=mock` swaps the whole model layer without the
/// endpoint, the harness or the projection knowing — which is what lets CI exercise this path.
#[derive(Clone)]
pub struct AgUiState {
    pub auth: AuthState,
    pub door: Arc<dyn ModelDoor>,
    pub model: String,
    /// The computer provider. Tools are bound to a coworker's own box **per request**, not here:
    /// a server-wide `ToolRunner` would carry one identity for everybody, which is precisely the
    /// confusion the identity rule exists to prevent.
    pub computer: Option<Arc<dyn opengrok_box::Computer>>,
    /// The route the auto-review judge asks on — the deployment's own, never the coworker's: one
    /// call per reviewed tool call must be cheap, the reviewer must not be the reviewed, and a
    /// coworker-route outage must not become a wall of cards. `OG_AUTO_REVIEW_MODEL`.
    pub auto_review_model: String,
    /// Seals connector credentials. `None` means no connector can be stored, which is a legitimate
    /// deployment — and must read as "connectors unavailable" rather than as a crash.
    pub vault: Option<Arc<opengrok_store::Vault>>,
    /// Provider configuration and the callback URL.
    pub connectors: crate::connections::routes::Connectors,
    /// Plugins installed on this server, by name. Installing one makes it *available*; a coworker
    /// still needs it in their ceiling before its tools run.
    pub plugins: Arc<BTreeMap<String, opengrok_plugins::Plugin>>,
    /// Shared with `HostState.settings` so AG-UI turns see `egressTunnelEnabled`.
    /// `None` until `HostState::new` / `router` attach the Arc; env flags still apply.
    pub host_settings: Option<Arc<Mutex<serde_json::Value>>>,
}

impl AgUiState {
    /// Env `OG_EGRESS_TUNNEL_ENABLED=1` / `SAND_EGRESS_TUNNEL_ENABLED=1` (Grok host parity),
    /// or host setting `egressTunnelEnabled`. This is host *intent*. The gateway verb
    /// and Review-an-action gate also need `/v1/info` `egress_tunnel.ready`.
    #[must_use]
    pub fn egress_tunnel_enabled(&self) -> bool {
        let settings = self
            .host_settings
            .as_ref()
            .and_then(|lock| lock.lock().ok().map(|value| value.clone()))
            .unwrap_or_else(crate::host_state::default_settings);
        crate::host_state::egress_tunnel_available(&settings)
    }

    /// Host intent AND this box's `egress_tunnel.ready`. Failed info → false.
    /// Review-an-action for leave-box tools uses this, not host intent alone.
    pub async fn egress_tunnel_for(
        &self,
        computer: &dyn opengrok_box::Computer,
        box_id: &str,
    ) -> bool {
        let host_wants = self.egress_tunnel_enabled();
        if !host_wants {
            return false;
        }
        opengrok_box::EgressTunnel::advertised(true, computer.egress_tunnel(box_id).await)
    }
}

/// Which coworker a run belongs to, and therefore whose computer its tools use.
///
/// AG-UI has no field for this, so the client passes it in `forwardedProps` — and it is a
/// *request*, not an authorisation: the id names a coworker, and the box comes from that
/// coworker's own row. A client naming a coworker it does not own is the next thing policy must
/// check (slice 5); today the row simply has to exist.
/// Tools the person named for THIS turn, e.g. by typing `@websearch` in the composer.
///
/// A preference, not a restriction: every tool the bot may run stays on offer, because narrowing
/// the set would turn a hint into a cage and strand a turn that needed one more tool. What the
/// name buys is that the model is told, in the system message, which tools the person reached
/// for — the thing a person means when they type one.
fn preferred_tools_from(input: &RunAgentInput) -> Vec<String> {
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

/// The named tools that this bot can actually run, in the order they were named.
///
/// A name the bot was never offered is dropped rather than repeated: a system message that
/// names a tool the model has not been given is an instruction it cannot follow, and the model
/// spends the turn looking for it.
fn honour_preferences(preferred: &[String], runner: Option<&ToolRunner>) -> Vec<String> {
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
pub(super) fn chosen_recipe_from(
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
pub(super) fn recipe_values_from(values: Option<&serde_json::Value>) -> BTreeMap<String, String> {
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
/// Ours are `skl_` plus a UUID — 40 characters. The bound is generous enough for a longer id shape
/// later and small enough that a 2 MB `skill` field is refused before it reaches a log line: this
/// value is caller-controlled, and a WARN that carries it is a record whose size the caller picks.
const MAX_SKILL_ID_CHARS: usize = 128;

/// What `forwardedProps.skill` said this turn.
pub(super) enum ChosenSkill {
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
pub(super) fn chosen_skill_from(input: &RunAgentInput) -> Option<ChosenSkill> {
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
    if !id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Some(ChosenSkill::Unusable("not shaped like an id"));
    }
    Some(ChosenSkill::Id(id.to_string()))
}

/// The skill segment of this turn's system message: the person's instructions quoted between
/// unforgeable markers, or the sentence saying they were not.
///
/// EVERY PATH OUT OF HERE SAYS SOMETHING. A chosen skill that cannot be given is the case the
/// refusal line exists for, so returning an empty string on any of these would be precisely the
/// silence it was written to prevent.
async fn skill_segment(state: &AgUiState, account: &AccountId, chosen: &ChosenSkill) -> String {
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
            return why.line().to_string();
        }
        ChosenSkill::Id(id) => id,
    };
    let skill = match crate::skills::for_turn(state, account, id).await {
        Ok(skill) => skill,
        Err(why) => {
            // `?` on both, not `%`: the id is caller-controlled and `why` carries a store error
            // verbatim, and Display writes a newline as a newline — one forged log record per
            // request, at a position the caller picks. Which case it was is recorded here and
            // nowhere else: the sentence the person reads names no cause at all.
            tracing::warn!(skill = ?id, why = ?why, "a chosen skill was not given to a turn");
            return why.line().to_string();
        }
    };
    let quoted = crate::persona::skill_marker(&skill.body)
        .map(|marker| {
            crate::persona::chosen_skill_line(&skill.name, &skill.body, &marker, skill.author)
        })
        .filter(|segment| !segment.is_empty());
    match quoted {
        Some(segment) => segment,
        None => {
            // No marker the body does not already contain, so the quote could not be closed where
            // we say it closes. Refuse rather than quote it unbounded.
            let why = crate::skills::NotForThisTurn::Unquotable;
            tracing::warn!(skill = ?id, why = ?why, "a chosen skill could not be quoted safely");
            why.line().to_string()
        }
    }
}

/// The skill line for this turn, and the id to record on `RunEvent::Started`.
///
/// An id on this request is used as sent. None means look at prior runs on this
/// thread, newest first, and take the first that quoted a skill. A refusal line
/// records no id, so a skill that cannot be given does not stick.
async fn skill_line_for_turn(
    state: &AgUiState,
    account: &AccountId,
    thread_id: &str,
    input: &RunAgentInput,
) -> (String, Option<String>) {
    let chosen = match chosen_skill_from(input) {
        Some(chosen) => chosen,
        None => match inherited_skill_id(state, account, thread_id).await {
            Some(id) => ChosenSkill::Id(id),
            None => return (String::new(), None),
        },
    };
    let id = match &chosen {
        ChosenSkill::Id(id) => Some(id.clone()),
        ChosenSkill::Unusable(_) => None,
    };
    let line = skill_segment(state, account, &chosen).await;
    let recorded = line
        .contains("For THIS message the person chose the skill `")
        .then_some(id)
        .flatten();
    (line, recorded)
}

/// Newest prior run on this thread, owned by this account, that quoted a skill.
/// Walks past a turn that recorded none: that is the follow-up which dropped the
/// skill (run 01a0c9ef) and the turn before it still has the body.
async fn inherited_skill_id(
    state: &AgUiState,
    account: &AccountId,
    thread_id: &str,
) -> Option<String> {
    let runs = state
        .auth
        .store
        .runs_for_thread_owned_by(thread_id, account, 8)
        .await
        .ok()?;
    for run in runs {
        let loaded = match state.auth.store.load_run(&run.id).await {
            Ok((loaded, _)) => loaded,
            Err(error) => {
                tracing::warn!(%error, "could not read a prior run while inheriting a skill");
                continue;
            }
        };
        if let Some(skill_id) = loaded
            .skill_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            return Some(skill_id.to_string());
        }
        if let Some(name) = loaded
            .system
            .as_deref()
            .and_then(crate::persona::skill_name_from_system)
        {
            match state.auth.store.skill_named(account.as_str(), name).await {
                Ok(Some(row)) => return Some(row.id),
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(%error, "could not resolve an inherited skill by name");
                    return None;
                }
            }
        }
    }
    None
}

fn coworker_id_from(input: &RunAgentInput) -> Option<CoworkerId> {
    input
        .forwarded_props
        .get("coworkerId")
        .and_then(|value| value.as_str())
        .map(|id| CoworkerId::from_stored(id.to_string()))
}

/// Build the tools for this run, bound to this coworker's own computer and this principal's grant.
///
/// THE GRANT IS READ HERE, ON THIS TURN. Not cached from sign-in and not carried in the request:
/// a grant revoked a second ago must stop this turn (CLAUDE.md #6).
/// The same binding, addressed by coworker rather than by request — because the scheduler and the
/// monitor fire runs with no `RunAgentInput` anywhere in sight.
/// `wake_patience` bounds how long the first box-bound tool call waits for a sleeping box before
/// answering that the computer is down or still starting: a turn can afford `TURN_WAKE_PATIENCE`;
/// the MCP door, whose caller (Claude Code) has its own request timeout, passes a shorter one and
/// lets the tool result say "still starting".
pub(crate) async fn tools_for_coworker(
    state: &AgUiState,
    account_id: &opengrok_core::id::AccountId,
    coworker_id: &CoworkerId,
    approved: &[String],
    review_approved: &[String],
    wake_patience: std::time::Duration,
) -> Option<ToolRunner> {
    let coworker_id = coworker_id.clone();
    let (coworker, _) = state.auth.store.load_coworker(&coworker_id).await.ok()?;
    // A coworker with no computer gets no tools rather than tools that cannot run: a tool the
    // model is told about but that always refuses is a dead end it keeps trying.
    coworker.computer()?;
    // Resolve the provider for this coworker's computer by its account's effective sharing mode and
    // scope (per-org / per-account / per-bot), then that scope's recorded kind — so tools run on the
    // same provider that created the box.
    let (mode, org_id) = super::provision::resolve_mode(state, account_id).await;
    let (scope, scope_id, _) = super::provision::scope_for(
        &mode,
        account_id.as_str(),
        org_id.as_deref(),
        coworker_id.as_str(),
        coworker.is_group(),
    );
    let (mut box_id, kind, stopped) = state
        .auth
        .store
        .scoped_computer_full(scope, &scope_id)
        .await
        .ok()
        .flatten()?;
    let mut computer = super::provision::provider_for(state, org_id.as_deref(), &kind).await?;
    // The box is NOT woken here. A turn used to wait up to `wake_patience` for a sleeping box
    // before the model was even asked, and a plain "hi" paid for it (90 s with a dead box, 21 Sep
    // 2026). The executor wakes the box the first time a tool needs it, and the stream says so.
    // What stays is one cheap look at the box: a provider that refuses to say (401/403 — an ascii
    // key revoked, a computer this deployment may no longer reach) is taken over by local Docker
    // now, as it was when the wake found the same refusal.
    let _ = stopped;
    let mut running = false;
    match computer.state(&box_id).await {
        Ok(state) => running = state == "running",
        Err(error) => {
            let forbidden = matches!(
                &error,
                opengrok_box::BoxError::Refused {
                    status: 401 | 403,
                    ..
                }
            ) || error.to_string().contains("forbidden");
            if forbidden {
                tracing::warn!(%error, box_id, "the provider refuses this box; taking it over with local Docker");
                match super::provision::take_over_with_local_docker(
                    state,
                    scope,
                    &scope_id,
                    org_id.as_deref(),
                )
                .await
                {
                    Some((local, new_id)) => {
                        computer = local;
                        box_id = new_id;
                        running = true;
                    }
                    None => return None,
                }
            } else {
                tracing::warn!(%error, box_id, "the box's state could not be read; a tool that needs it will say so");
            }
        }
    }
    // The in-use stamp keeps the idle sweep off a box while it is used. A box that is asleep is
    // not in use by a turn that never touches it, so it is stamped only when running now — and by
    // the executor, through `on_woken`, the moment a tool (or a form fill) brings it up; a box
    // the executor merely finds running is not stamped twice.
    if running {
        let _ = state
            .auth
            .store
            .mark_scoped_used(scope, &scope_id, chrono::Utc::now().timestamp_millis())
            .await;
    }
    let stamp_store = state.auth.store.clone();
    let stamp_scope_id = scope_id.clone();
    let on_woken: opengrok_tools::OnWoken = std::sync::Arc::new(move |_| {
        let store = stamp_store.clone();
        let scope_id = stamp_scope_id.clone();
        tokio::spawn(async move {
            let _ = store
                .mark_scoped_used(scope, &scope_id, chrono::Utc::now().timestamp_millis())
                .await;
        });
    });

    // The policy a turn runs under, so a member of the org talking to a shared coworker gets the
    // tools its owner's grant allows — the same answer the run door's gate just gave. Callers
    // that must be the owner (routines, the tool listing) gate on that before they get here.
    let policy = state
        .auth
        .store
        .policy_to_use(account_id, &coworker_id)
        .await
        .ok()?;

    // The plugins this coworker may use, connected with its own credentials. On a shared
    // coworker that includes its `bot`-scoped connections, which its OWNER authorised: a member's
    // turn acts through them, while the owner's `user`-scoped ones stay the owner's (ROADMAP
    // 19.4). Narrowing that is a decision about what sharing lends, not a filter to add here.
    let (sessions, tools) = connect_plugins(state, account_id, &coworker_id, &policy).await;

    // Bind the SCOPE's live box, not the coworker's frozen hire-time id. They match at hire, but a
    // reset or re-provision changes the account's box while the aggregate id stays put — and this is
    // the same box we just resumed above, so exec must run on it, or a reset would leave the bot
    // executing against a destroyed box.
    let mut context = opengrok_tools::ToolContext::from_coworker(
        account_id.clone(),
        coworker_id.clone(),
        &coworker,
    );
    // A box with a display gets the screen tools (`open_url`, `computer`); a headless one is
    // never told about them, so it cannot be sent down a dead end.
    // Whether the coworker has a screen is how its box is made, not whether the box happens to be
    // awake: the prompt and the tool list then say the same thing on every turn.
    let screen = computer.offers_a_screen(&box_id).await;
    // The tunnel probe asks the box's guest, which only answers when the box is up. For a box
    // that is asleep now, the executor asks the guest right after the first leave-box tool wakes
    // it, and raises the consent card only if the tunnel is really there.
    let egress_tunnel = if running {
        if state.egress_tunnel_for(computer.as_ref(), &box_id).await {
            opengrok_tools::EgressTunnelMode::On
        } else {
            opengrok_tools::EgressTunnelMode::Off
        }
    } else if state.egress_tunnel_enabled() {
        opengrok_tools::EgressTunnelMode::AskTheBoxAfterWake
    } else {
        opengrok_tools::EgressTunnelMode::Off
    };
    // The person's standing answer for THIS computer to the tunnel's card, keyed by the scope
    // the box lives under so a reset or takeover that changes the box id keeps the choice.
    // No row is `ask`: one card per run, as before there was a choice.
    let (egress_policy, egress_unconfirmed) =
        provision::egress_policy_for_turn(state, scope, &scope_id).await;
    context.box_id = Some(opengrok_core::id::BoxId::from_stored(box_id));
    let transcript_hold = match state
        .auth
        .store
        .gateway_transcript(&coworker_id, account_id)
        .await
    {
        Ok(entries) => entries
            .iter()
            .any(opengrok_tools::user_form::holds_the_screen),
        // A transcript we cannot read must not freeze every screen tool; the form's own
        // submit path still refuses to log secrets.
        Err(_) => false,
    };
    context.screen_hold =
        transcript_hold || pending_form_hold(state, account_id, &coworker_id).await;

    // The recipes this bot was granted: offered as `run_recipe` only with a screen to run on.
    let recipes = if screen {
        crate::recipes::offers_for(state, &coworker_id).await
    } else {
        Vec::new()
    };
    let mut executor = opengrok_tools::Executor::with_policy(computer, policy)
        .with_wake_patience(wake_patience)
        .with_on_woken(on_woken)
        .with_screen(screen)
        .with_recipes(recipes, crate::recipes::source_for(state))
        .with_plugin_tools(sessions, tools)
        .with_approved(approved.iter().cloned())
        .with_review_approved(review_approved.iter().cloned())
        .with_egress_tunnel_mode(egress_tunnel)
        .with_egress_policy(egress_policy)
        .with_egress_policy_unconfirmed(egress_unconfirmed);
    // The reverse-exec tool: offered ONLY when this account has an enrolled, enabled machine to
    // reach — otherwise the model is never told about a channel it cannot use. Bound to that
    // machine, and to this coworker for the audit origin.
    if let Some((machine_id, _label)) =
        crate::local_exec::enabled_machine(&state.auth.store, account_id.as_str()).await
    {
        executor =
            executor.with_user_machine(std::sync::Arc::new(crate::local_exec::ReverseExecSink {
                auth: state.auth.clone(),
                coworker_id: coworker_id.as_str().to_string(),
                machine_id,
            }));
    }

    // THE TIER WALK HAPPENS HERE, ONCE PER RUN (docs/AUTO-REVIEW.md §3). Per tool call the check
    // is one in-memory test on the runner; a run that started before a PUT keeps the policy it
    // started with, and a resumed run rebuilds its runner through this function and re-resolves.
    // Nothing is attached when the policy is off or empty, so an unreviewed run costs nothing.
    let effective = crate::auto_review::load_effective(
        &state.auth.store,
        account_id.as_str(),
        Some(coworker_id.as_str()),
    )
    .await;
    if let Some(policy) = effective.review_policy() {
        // The judge is billed to the coworker whose turn raised it, and checked against that
        // coworker's limits. Resolved HERE because this function already holds the coworker and
        // runs once per run, which is exactly where the run path resolves its own key.
        //
        // A coworker over its cap now has its judge refused, and a refused judge is
        // `Unavailable`, which the executor turns into an Ask — so the tool call raises a card
        // for a person rather than proceeding unreviewed or spending past a limit.
        let judge =
            opengrok_harness::ModelJudge::new(state.door.clone(), state.auto_review_model.clone())
                .for_coworker(
                    coworker_id.as_str(),
                    account_id.as_str(),
                    crate::spend::key_for(state, &coworker_id, account_id).await,
                );
        executor = executor.with_auto_review(policy, Arc::new(judge));
    }

    Some(ToolRunner::new(executor, context))
}

/// A run suspended on a user-form holds the screen whether or not the transcript has a card
/// for it: the pending row is the truth, the card is chrome.
async fn pending_form_hold(
    state: &AgUiState,
    account_id: &opengrok_core::id::AccountId,
    coworker_id: &CoworkerId,
) -> bool {
    let Ok(run_ids) = state.auth.store.awaiting_approval(account_id).await else {
        return false;
    };
    for run_id in run_ids {
        let Ok((run, _)) = state.auth.store.load_run(&run_id).await else {
            continue;
        };
        if !crate::agui::resume::run_belongs_to(&run, coworker_id) {
            continue;
        }
        if matches!(
            run.pending.as_ref().map(|pending| pending.reason),
            Some(opengrok_core::run::SuspendReason::UserForm)
        ) {
            return true;
        }
    }
    false
}

/// The access token for a connection, refreshed first if it is about to expire.
///
/// REFRESHED BEFORE USE, NOT AFTER FAILURE. Waiting for a 401 means every expiry costs a person one
/// visibly failed tool call, and a model that reads that failure may go and do something else. The
/// leeway lives in `ConnectionView::is_expiring` so a token cannot expire mid-flight.
///
/// A refusal that means the person revoked access disconnects rather than retrying: `invalid_grant`
/// is a decision somebody made, and retrying it forever turns a revoked connection into a permanent
/// error loop.
async fn live_token(
    state: &AgUiState,
    vault: &opengrok_store::Vault,
    chosen: &opengrok_core::connection::ConnectionView,
) -> Option<String> {
    let stored = state
        .auth
        .store
        .open_credential(vault, &chosen.id)
        .await
        .ok()
        .flatten();

    if !chosen.is_expiring(now_ms()) {
        return stored;
    }

    // Expiring. Without a provider or a refresh token there is nothing to do but use what we have
    // and let the provider say no — which is still better than refusing a call we might complete.
    let Some(config) = state.connectors.providers.get(&chosen.connector) else {
        return stored;
    };
    let Ok(Some(refresh_token)) = state
        .auth
        .store
        .open_credential(vault, &format!("{}_refresh", chosen.id))
        .await
    else {
        return stored;
    };

    match crate::connections::flow::refresh(&reqwest::Client::new(), config, &refresh_token).await {
        Ok(token) => {
            let at_ms = now_ms();
            let expires_at = token.expires_at_ms(at_ms);

            // Sealed before it is returned, so a crash between here and the next request does not
            // leave the old token in the database and the new one only in memory.
            if let Ok(sealed) = vault.seal(&chosen.id, &token.access_token) {
                let _ = state
                    .auth
                    .store
                    .put_secret(&chosen.id, &sealed, at_ms)
                    .await;
            }
            // Google omits the refresh token on a refresh, so the stored one is kept.
            if let Some(rotated) = token.refresh_token_to_store(Some(&refresh_token))
                && rotated != refresh_token
                && let Ok(sealed) = vault.seal(&format!("{}_refresh", chosen.id), &rotated)
            {
                let _ = state
                    .auth
                    .store
                    .put_secret(&format!("{}_refresh", chosen.id), &sealed, at_ms)
                    .await;
            }
            let _ = state
                .auth
                .store
                .touch_expiry(&chosen.id, expires_at, at_ms)
                .await;

            tracing::info!(
                connector = chosen.connector,
                "refreshed an expiring connection"
            );
            Some(token.access_token)
        }
        Err(error) => {
            if error.is_revoked() {
                tracing::warn!(
                    connector = chosen.connector,
                    "a connection was revoked at the provider; disconnecting it"
                );
                let _ = disconnect_revoked(state, &chosen.id).await;
                return None;
            }
            tracing::warn!(%error, connector = chosen.connector, "could not refresh; using what we have");
            stored
        }
    }
}

/// Record that a provider has revoked a connection.
///
/// Written down rather than merely logged: a person looking at their connections should see it is
/// gone, and the next run should not try again.
async fn disconnect_revoked(state: &AgUiState, id: &str) -> Result<(), opengrok_store::StoreError> {
    let (mut connection, seq) = state.auth.store.load_connection(id).await?;
    let at_ms = now_ms();
    let events = connection
        .decide(opengrok_core::connection::ConnectionCommand::Disconnect { at_ms })
        .unwrap_or_default();
    for event in &events {
        connection.apply(event);
    }
    state
        .auth
        .store
        .append_connection(
            id,
            seq,
            &events,
            &connection,
            &opengrok_store::CredentialUpdate::none(at_ms),
        )
        .await?;
    Ok(())
}

/// Open a session with every plugin server this coworker can both reach and be permitted to use.
///
/// TWO GATES, AND BOTH MATTER. A plugin must be in the coworker's ceiling — installing one on the
/// server is not the same as letting a coworker use it — and its credential must resolve, because
/// a connected tool without a token is a tool that fails at the moment of use rather than at the
/// moment of offer.
///
/// A server that will not connect is skipped with a warning rather than failing the run: the other
/// tools still work, and a turn that dies because one connector is down is worse than a turn that
/// proceeds without it.
async fn connect_plugins(
    state: &AgUiState,
    account_id: &opengrok_core::id::AccountId,
    coworker_id: &CoworkerId,
    policy: &opengrok_policy::Context,
) -> (
    BTreeMap<String, Arc<opengrok_tools::mcp::Session>>,
    Vec<opengrok_tools::mcp::McpTool>,
) {
    let mut sessions = BTreeMap::new();
    let mut tools = Vec::new();

    if state.plugins.is_empty() {
        return (sessions, tools);
    }

    // Every credential this coworker can use, keyed the way a plugin's placeholders name them:
    // `GMAIL_TOKEN` for the `gmail` connector.
    let candidates = state
        .auth
        .store
        .connections_for(account_id, coworker_id)
        .await
        .unwrap_or_default();

    let mut values: BTreeMap<String, String> = BTreeMap::new();
    if let Some(vault) = state.vault.as_ref() {
        for connector in candidates
            .iter()
            .map(|candidate| candidate.connector.clone())
            .collect::<std::collections::BTreeSet<_>>()
        {
            // The domain decides which of several connections wins — bot's own, then lent, then
            // global. That rule is pure and tested; this only asks it.
            let Some(chosen) =
                opengrok_core::connection::resolve(&candidates, &connector, coworker_id)
            else {
                continue;
            };
            if let Some(token) = live_token(state, vault, chosen).await {
                values.insert(format!("{}_TOKEN", connector.to_uppercase()), token);
            }
        }
    }

    for plugin in state.plugins.values() {
        let (endpoints, problems) = opengrok_tools::mcp::endpoints_for(plugin, &values);
        for problem in problems {
            tracing::debug!(%problem, plugin = plugin.manifest.name, "a plugin server is unavailable");
        }

        for endpoint in endpoints {
            let key = format!("{}.{}", endpoint.plugin, endpoint.server);

            let session = match opengrok_tools::mcp::Session::connect(endpoint).await {
                Ok(session) => Arc::new(session),
                Err(error) => {
                    tracing::warn!(%error, server = key, "could not reach a plugin server");
                    continue;
                }
            };

            match session.tools().await {
                Ok(offered) => {
                    // The ceiling gate. A tool the coworker may not run is not offered at all —
                    // being told about a tool that always refuses is a dead end a model retries.
                    for tool in offered {
                        let decision = opengrok_policy::decide(
                            account_id,
                            coworker_id,
                            opengrok_policy::Action::RunTool(&tool.qualified_name),
                            policy,
                        );
                        if decision.is_allowed() || decision.needs_approval() {
                            tools.push(tool);
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, server = key, "a plugin server would not list its tools");
                    continue;
                }
            }

            sessions.insert(key, session);
        }
    }

    (sessions, tools)
}

/// `POST /ag-ui` lives on `HostState` so a UserForm CUSTOM can mint the gateway card and
/// stamp `entryId` on the SSE frame NativeChat receives. So does the answer to a card: the
/// run it continues can raise a form of its own, and a form with no card entry has no
/// `entryId` for NativeChat to submit to — its Log in button stayed grey (21 Sep 2026).
/// Other AG-UI routes stay on `AgUiState`. The SSE still forwards CUSTOM
/// `run-awaiting-approval`; the card is additive.
pub fn run_router(state: crate::host_state::HostState) -> Router {
    Router::new()
        .route("/ag-ui", post(run))
        .route("/ag-ui/runs/{run_id}/answer", post(answer_run))
        .with_state(state)
}

pub fn router(state: AgUiState) -> Router {
    Router::new()
        .route("/ag-ui/runs/{run_id}", get(replay_run))
        .route("/ag-ui/runs/{run_id}/stop", post(stop_run))
        .route("/ag-ui/runs/{run_id}/hide", post(hide_run))
        .route("/ag-ui/threads/{thread_id}", get(replay_thread))
        .route("/ag-ui/approvals", get(list_awaiting))
        .route(
            "/ag-ui/host-settings",
            get(host_settings).put(patch_host_settings),
        )
        .route("/coworkers", post(hire).get(list_coworkers))
        .route("/models", get(list_models))
        // The org's coworker templates, for the hire picker. Written by the admin
        // (`account_api.rs`, `/admin/templates`).
        .route("/templates", get(list_templates))
        .route("/models/probe", post(probe_model))
        .route(
            "/coworkers/{coworker_id}",
            axum::routing::patch(repin_coworker).delete(delete_coworker),
        )
        .route("/coworkers/{coworker_id}/approvals", post(set_approvals))
        // Spend limits: the coworker's three meters, read-only here; limits are written by the
        // org admin (`account_api.rs`, `/admin/spend`).
        .route("/coworkers/{coworker_id}/spend", get(get_spend))
        // Points (`points.rs`): usage per model, a report; and the coworker's own limit, the
        // owner's to write (its cap for the month, its brake for the day).
        .route("/coworkers/{coworker_id}/usage", get(get_usage))
        .route(
            "/coworkers/{coworker_id}/limit",
            get(get_limit).put(set_limit),
        )
        .route(
            "/coworkers/{coworker_id}/keys",
            post(mint_bot_key).get(list_bot_keys),
        )
        .route(
            "/coworkers/{coworker_id}/keys/{jti}",
            axum::routing::delete(revoke_bot_key),
        )
        .route("/coworkers/{coworker_id}/mcp-calls", get(list_mcp_calls))
        .route(
            "/coworkers/{coworker_id}/computer",
            get(computer_status).post(ensure_computer),
        )
        .route("/coworkers/{coworker_id}/screen", get(computer_screen))
        .route("/coworkers/{coworker_id}/tools", get(list_tools))
        .route(
            "/coworkers/{coworker_id}/computer/update",
            post(computer_update),
        )
        .route(
            "/coworkers/{coworker_id}/computer/egress-policy",
            get(get_egress_policy).put(set_egress_policy),
        )
        .route(
            "/coworkers/{coworker_id}/computer/reset",
            post(computer_reset),
        )
        .with_state(state.clone())
        // Pending user messages: NativeChat's follow-up queue. Nested under the thread they
        // belong to, because `GET /ag-ui/threads/{id}` is already how other clients hydrate.
        .merge(super::pending::router(state))
}

/// `GET /models` — the routes this deployment's gateway advertises.
///
/// Signed in is enough: this is the list of things a person may pin their own coworker to, and it
/// carries no secret. The gateway's key stays here; the browser only ever learns ids.
pub async fn list_models(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
) -> Response {
    if account_from_bearer(&state, &headers).is_none() {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    }
    let Some(catalogue) = state.auth.model_catalogue.clone() else {
        // A mock door has no gateway to ask. Say that, rather than answering [] as though the
        // gateway had told us it serves nothing.
        return Json(serde_json::json!({
            "models": [],
            "note": "this deployment has no gateway configured (OG_MODEL_DOOR is a mock), so a \
                     pin must be typed by hand",
        }))
        .into_response();
    };
    let listing = catalogue.list().await;
    let points = crate::points::models_points(&state).await;
    Json(serde_json::json!({
        "models": listing
            .models
            .iter()
            .map(|model| serde_json::json!({
                "id": model.id,
                // Points multipliers (`points.rs`): null on a gateway with no reference price
                // or older than open-ai-gateway #52; the picker shows ×N after the id.
                "points": crate::points::points_json(
                    points.as_ref().and_then(|p| p.get(crate::points::base_model(&model.id))),
                ),
            }))
            .collect::<Vec<_>>(),
        "note": listing.note,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub struct ProbeRequest {
    pub model: String,
}

/// `POST /models/probe` — ask the gateway to answer one tiny prompt on a candidate pin.
///
/// This is how a pin is proven BEFORE it is saved. It is also how we learned that `oag/auto` is
/// refused on a route with no matching credential — a fact no amount of reading the catalogue
/// would have revealed, because the id IS advertised.
pub async fn probe_model(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Json(request): Json<ProbeRequest>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let model = request.model.trim();
    if model.is_empty() {
        return (StatusCode::BAD_REQUEST, "a model is required").into_response();
    }
    let Some(catalogue) = state.auth.model_catalogue.clone() else {
        return Json(serde_json::json!({
            "ok": false,
            "detail": "this deployment has no gateway configured, so a pin cannot be proven here",
        }))
        .into_response();
    };
    // A probe is a REAL, billed completion on the deployment's own key. One person clicking Test
    // needs a handful; a loop wants thousands of somebody else's money.
    if !catalogue.may_probe(account_id.as_str()) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "wait a moment before testing another route",
        )
            .into_response();
    }
    match catalogue.probe(model).await {
        Ok(served) => Json(serde_json::json!({ "ok": true, "served": served })).into_response(),
        // The gateway's own words. A paraphrase would lose the part that says what to do.
        Err(detail) => Json(serde_json::json!({ "ok": false, "detail": detail })).into_response(),
    }
}

/// `PATCH /coworkers/{id}` — a partial update. A field absent is left alone; `role: null` or a
/// blank string clears it. Taking the body as a `Value` rather than a struct of `Option`s is what
/// makes "absent" and "null" different, which a nullable field needs.
#[derive(Debug, Deserialize)]
pub struct RepinRequest {
    /// A route through the gateway, never a key.
    pub model: String,
}

/// `PATCH /coworkers/{id}` — change this coworker's name, its route, its standing role, its
/// decoration, or several at once.
///
/// EVERY FIELD THE CLIENT SENDS IS READ HERE. The app's Save button puts the whole card in one
/// body — name, title and role together — so a field this route quietly skipped was an edit the
/// person watched succeed and lost: a rename to "Greendale" was dropped on the floor while the
/// role beside it was stored, and the coworker went on introducing itself as "New Bot".
///
/// The fields land in two different homes, as `persona.rs` explains: the name, model, role and
/// visibility are the aggregate's, and the title, avatar shape and colour are the client's
/// decoration in the seam-B profile blob. `notifyOnUpdates` has no home at all — see below.
///
/// A coworker not on your roster answers 404, like every other per-coworker route here: an id
/// you cannot use must not be distinguishable from one that does not exist. One an org-mate
/// shared with you is on your roster, so you already know it exists; what you may change on it is
/// your own sidebar, and anything else is a 403 that says so — management stays with the owner.
/// A body naming no field is a 400 rather than a silent no-op, because a caller who sent one
/// meant something.
pub async fn repin_coworker(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(coworker_id): axum::extract::Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let refuse = |sentence: String| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": sentence })),
        )
            .into_response()
    };
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let coworker_id = CoworkerId::from_stored(coworker_id);
    // The caller's own roster row, so the reply below can be exactly what the roster will list.
    let (listed, owner) = match state.auth.store.roster_for(&account_id).await {
        Ok(roster) => match roster.into_iter().find(|(view, _)| view.id == coworker_id) {
            Some(seat) => seat,
            None => return (StatusCode::NOT_FOUND, "no such coworker").into_response(),
        },
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };
    let mine = owner.id == account_id;

    // A name is trimmed and must survive it. Null is a wrong type here rather than "clear it":
    // the role is nullable and a name is not, because `persona::system_message` has no identity
    // line to write without one and the coworker would stop knowing what it is called.
    let name = match body.get("name") {
        None => None,
        Some(serde_json::Value::String(name)) => match crate::persona::validate_name(name) {
            Ok(name) => Some(name),
            Err(sentence) => return refuse(sentence),
        },
        Some(_) => return refuse("name: expected a string".to_string()),
    };
    let model = match body.get("model") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(model)) => Some(model.clone()),
        Some(_) => return refuse("model: expected a route id".to_string()),
    };
    // Absent leaves the role alone; null or blank clears it. Both are meaningful, so they cannot
    // collapse into one `Option`.
    let role = match body.get("role") {
        None => None,
        Some(serde_json::Value::Null) => Some(None),
        Some(serde_json::Value::String(role)) => match crate::persona::validate_role(Some(role)) {
            Ok(role) => Some(role),
            Err(sentence) => return refuse(sentence),
        },
        Some(_) => return refuse("role: expected a string, or null to clear it".to_string()),
    };
    // "private" | "org". An unrecognised word is refused rather than defaulted: a caller who
    // wrote "public" meant something we do not offer, and quietly storing "private" would tell
    // them they had shared a coworker they had not. `org` is honoured by the roster
    // (`roster_for`) and the run door (`policy_to_use`). Until #175 nothing on this door read
    // it, and a 200 here reported a sharing that did nothing; an owner in no org, for whom that
    // is still true, is refused below.
    let visibility = match body.get("visibility") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(text)) => {
            match opengrok_core::coworker::Visibility::parse(text) {
                Some(visibility) => Some(visibility),
                None => {
                    return refuse(format!("visibility: '{text}' is not one of private, org"));
                }
            }
        }
        Some(_) => return refuse("visibility: expected \"private\" or \"org\"".to_string()),
    };
    let hidden = match body.get("hiddenFromSidebar") {
        None => None,
        Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Bool(hidden)) => Some(*hidden),
        Some(_) => return refuse("hiddenFromSidebar: expected a boolean".to_string()),
    };
    // The decoration, type-checked at the door and merged into the profile blob below. A key
    // absent leaves the stored value alone; an empty string is stored as one, which is how the
    // client clears a field and how the desktop's own update already behaves.
    //
    // `description` is in the blob too but no client sends it here, so this door does not read
    // it — the key list itself lives in `persona.rs`, where the blob's shape is decided.
    let mut decoration = serde_json::Map::new();
    for key in ["title", "avatarShape", "avatarColor"] {
        match body.get(key) {
            None => {}
            Some(serde_json::Value::String(text)) => {
                decoration.insert(key.to_string(), serde_json::Value::String(text.clone()));
            }
            Some(_) => return refuse(format!("{key}: expected a string")),
        }
    }
    // `notifyOnUpdates` arrives from the app and is READ NOWHERE, deliberately. Nothing on this
    // server stores it, `coworker_row` never answers it, and the desktop client keeps the real
    // answer in its own settings file (`docs/research/client-grok-bot.md` §8.1). Accepting it here would need a table, and
    // inventing one to make a toggle look persistent is worse than the toggle not persisting.
    //
    // So a body naming nothing this route can change — including one carrying only that toggle —
    // is a 400 rather than a silent no-op, and the sentence lists what it could have named.
    let manages = name.is_some()
        || model.is_some()
        || role.is_some()
        || visibility.is_some()
        || !decoration.is_empty();
    if !manages && hidden.is_none() {
        return refuse(
            "nothing to change: send a name, a model, a role, a title, an avatar shape or \
             colour, a visibility, hiddenFromSidebar, or several"
                .to_string(),
        );
    }
    if !mine {
        return match (manages, hidden) {
            (false, Some(hidden)) => {
                hide_shared(&state, &account_id, &listed, &owner, hidden).await
            }
            _ => (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "error": "only the person who hired this coworker can change it; you can \
                              hide it from your own sidebar"
                })),
            )
                .into_response(),
        };
    }

    let Ok((loaded, seq)) = state.auth.store.load_coworker(&coworker_id).await else {
        return (StatusCode::NOT_FOUND, "no such coworker").into_response();
    };
    // Read before anything is written, and a failure refuses the whole PATCH: the blob is merged
    // into and written back whole, so an unreadable one taken as `{}` would overwrite the stored
    // title and avatar with nothing — and even a PATCH that touches no decoration answers it.
    let mut profile = match state.auth.store.seamb_profile(&coworker_id).await {
        Ok(profile) => profile.unwrap_or_else(|| serde_json::json!({})),
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };
    // The stored hide flag, read here for the same reason and with the same refusal: the reply
    // is the roster row the app overwrites its own from, so a failed read answered as `false`
    // would un-hide the coworker on the sidebar of the person who hid it. `GET /coworkers`
    // answers the same failure 503.
    let hidden_from_sidebar = match hidden {
        Some(hidden) => hidden,
        None => match state.auth.store.hidden_coworker_ids(&account_id).await {
            Ok(ids) => ids.contains(coworker_id.as_str()),
            Err(error) => {
                return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
            }
        },
    };
    let at_ms = now_ms();
    let mut events = Vec::new();
    // One command per decision, as the aggregate defines them: renaming, repinning and describing
    // are different things and a caller that meant one must not do the other.
    if let Some(name) = name {
        match loaded.decide(CoworkerCommand::Rename { name, at_ms }) {
            Ok(more) => events.extend(more),
            Err(error) => return refuse(error.to_string()),
        }
    }
    if let Some(model) = model {
        match loaded.decide(CoworkerCommand::Repin { model, at_ms }) {
            Ok(more) => events.extend(more),
            Err(error) => return refuse(error.to_string()),
        }
    }
    if let Some(role) = role {
        match loaded.decide(CoworkerCommand::SetRole { role, at_ms }) {
            Ok(more) => events.extend(more),
            Err(error) => return refuse(error.to_string()),
        }
    }
    // Sharing with the org when there is no org would store a word that reaches nobody — the
    // 200 would tell the person their coworker was shared when it was not. Refused only on the
    // way IN, so a coworker already marked `org` (its owner has since left the org) can still be
    // saved from a card that sends its visibility back unchanged.
    if visibility == Some(opengrok_core::coworker::Visibility::Org)
        && loaded.visibility != opengrok_core::coworker::Visibility::Org
        && owner.org_id.is_none()
    {
        return refuse(
            "visibility: this account is in no org, so there is nobody to share this coworker \
             with; it stays private"
                .to_string(),
        );
    }
    if let Some(visibility) = visibility {
        match loaded.decide(CoworkerCommand::SetVisibility { visibility, at_ms }) {
            Ok(more) => events.extend(more),
            Err(error) => return refuse(error.to_string()),
        }
    }
    let mut after = loaded.clone();
    for event in &events {
        after.apply(event);
    }
    let view = opengrok_core::coworker::CoworkerView {
        id: coworker_id.clone(),
        name: after.name.clone(),
        model: after.model.clone(),
        box_id: after.box_id.clone(),
        retired: after.retired,
        members: after.members.clone(),
        // The stored stamp when nothing is appended (a decoration- or hide-only PATCH): the
        // projection is only rewritten with events, and a reply stamped `now` over an unchanged
        // row would move the coworker in the sidebar until the next roster read moved it back.
        updated_at_ms: if events.is_empty() {
            listed.updated_at_ms
        } else {
            at_ms
        },
        role: after.role.clone(),
        visibility: after.visibility,
    };
    if !events.is_empty()
        && state
            .auth
            .store
            .append_coworker(&coworker_id, &account_id, seq, &events, &view)
            .await
            .is_err()
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, "could not save").into_response();
    }
    if let Some(hidden) = hidden
        && state
            .auth
            .store
            .set_coworker_hidden(&account_id, &coworker_id, hidden, at_ms)
            .await
            .is_err()
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, "could not save").into_response();
    }
    if !decoration.is_empty() {
        crate::persona::merge_profile_text(&mut profile, &serde_json::Value::Object(decoration));
        // A 500 rather than the seam-B path's silent `let _`: this door exists because an edit
        // that is accepted and not stored is the bug being fixed, and a reply saying the title
        // changed when the write failed would be that bug again.
        if state
            .auth
            .store
            .put_seamb_profile(&coworker_id, &profile, at_ms)
            .await
            .is_err()
        {
            return (StatusCode::INTERNAL_SERVER_ERROR, "could not save").into_response();
        }
    }
    // The same row the roster lists, by construction: the app overwrites its row from this reply
    // and relaunches onto `GET /coworkers`, so two spellings of one coworker is a coworker that
    // changes shape on restart.
    Json(coworker_row(
        &view,
        Some(&profile),
        hidden_from_sidebar,
        &owner,
        &account_id,
    ))
    .into_response()
}

/// A member hiding a coworker an org-mate shared: the one change that is theirs to make, because
/// the sidebar it changes is their own (`coworker_hidden` is keyed by the viewer). Without it a
/// shared coworker is a row somebody can never put away.
async fn hide_shared(
    state: &AgUiState,
    account_id: &opengrok_core::id::AccountId,
    listed: &opengrok_core::coworker::CoworkerView,
    owner: &opengrok_store::RosterOwner,
    hidden: bool,
) -> Response {
    let profile = match state.auth.store.seamb_profile(&listed.id).await {
        Ok(profile) => profile,
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };
    if state
        .auth
        .store
        .set_coworker_hidden(account_id, &listed.id, hidden, now_ms())
        .await
        .is_err()
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, "could not save").into_response();
    }
    Json(coworker_row(
        listed,
        profile.as_ref(),
        hidden,
        owner,
        account_id,
    ))
    .into_response()
}

/// One coworker, as every route that answers with one spells it — the roster, the hire reply
/// (which adds `computerError` and `templateNote`) and the PATCH reply.
///
/// camelCase throughout, because that is what the app's `Coworker` deserialises: a snake_case
/// key is a field it silently reads as absent, which is how #27 lost a whole reply, and how the
/// roster (which serialized the core `CoworkerView` as-is) lost the sort key, the title and the
/// avatar on every relaunch.
///
/// Provenance, key by key, because no NativeChat source is in this checkout and the struct that
/// decodes this row is its `Coworker`: `title`, `avatarShape`, `avatarColor`, `isGroup` and
/// `memberIds` are the Electron host's roster names (`docs/research/client-grok-bot.md` §8.1,
/// transcribed from `source/host/extensions/session/session-summaries.ts:13-16`). §8.1 spells
/// the sort key `updatedAt` and the hide flag `isHiddenFromSidebar`; this row keeps
/// `updatedAtMs`, `hiddenFromSidebar` and `boxId`, the spellings the hire and PATCH replies
/// already answered NativeChat with before this row existed, so renaming one to match §8.1
/// would break the client that reads it today. The permission keys are server precedent (below).
/// Checking these against NativeChat's `Coworker` is the client's half.
///
/// Every key is always present, null when unset: a key that is sometimes missing is a shape the
/// app has to guess about. `retired` is not a key because a retired coworker is never a row.
/// `notifyOnUpdates` is absent on purpose: nothing stores it, and echoing a constant would
/// overwrite the toggle the person just moved.
///
/// The permission fields are decided here, per viewer, on every row (ROADMAP 19.2; the shape the
/// pre-deletion roster answered, `git show 0cc3487^:crates/opengrok-server/src/gateway/live.rs`).
/// `mine` is ownership; `canManage` follows it exactly, because management stays with the owner
/// when a coworker is shared — without it a member's client offers edit controls that answer
/// 403; `owner` names the hirer so a shared row can say whose it is.
pub(crate) fn coworker_row(
    view: &opengrok_core::coworker::CoworkerView,
    profile: Option<&serde_json::Value>,
    hidden_from_sidebar: bool,
    owner: &opengrok_store::RosterOwner,
    viewer: &opengrok_core::id::AccountId,
) -> serde_json::Value {
    let mine = owner.id == *viewer;
    // Blank reads as absent, the way `Persona::compose` reads the same blob: a cleared title is
    // a coworker with no title, not one called "".
    let decorated = |key: &str| {
        profile
            .and_then(|profile| profile.get(key))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    serde_json::json!({
        "id": view.id.as_str(),
        "name": view.name,
        "model": view.model,
        "role": view.role,
        "title": decorated("title"),
        "avatarShape": decorated("avatarShape"),
        "avatarColor": decorated("avatarColor"),
        "visibility": view.visibility.as_str(),
        "hiddenFromSidebar": hidden_from_sidebar,
        "updatedAtMs": view.updated_at_ms,
        // The hirer's computer, so null on a shared row: a member's turns resolve a box from
        // the member's own scope, and every computer route answers them 404, so the owner's id
        // here would name a machine the row's reader can neither open nor work on. Null is
        // already the row's word for "no computer" (a hire with none answers it).
        "boxId": view.box_id.as_ref().filter(|_| mine).map(|id| id.as_str()),
        "isGroup": !view.members.is_empty(),
        "memberIds": view.members.iter().map(CoworkerId::as_str).collect::<Vec<_>>(),
        "mine": mine,
        "canManage": mine,
        "owner": {
            "id": owner.id.as_str(),
            "name": format!("{} {}", owner.first_name, owner.last_name).trim(),
        },
    })
}

/// `DELETE /coworkers/{id}` — retire this coworker. Same ownership 404 as every other
/// per-coworker route: an id that is not yours is indistinguishable from one that does not exist.
pub async fn delete_coworker(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(coworker_id): axum::extract::Path<String>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let coworker_id = CoworkerId::from_stored(coworker_id);
    let owns = state
        .auth
        .store
        .coworkers_for(&account_id)
        .await
        .map(|roster| roster.iter().any(|view| view.id == coworker_id))
        .unwrap_or(false);
    if !owns {
        return (StatusCode::NOT_FOUND, "no such coworker").into_response();
    }
    let Ok((loaded, seq)) = state.auth.store.load_coworker(&coworker_id).await else {
        return (StatusCode::NOT_FOUND, "no such coworker").into_response();
    };
    let at_ms = now_ms();
    let mut after = loaded;
    let Ok(events) = after.decide(CoworkerCommand::Retire { at_ms }) else {
        return (StatusCode::CONFLICT, "that coworker has retired").into_response();
    };
    for event in &events {
        after.apply(event);
    }
    let view = CoworkerView {
        id: coworker_id.clone(),
        name: after.name.clone(),
        model: after.model.clone(),
        box_id: after.box_id.clone(),
        retired: after.retired,
        members: after.members.clone(),
        updated_at_ms: at_ms,
        role: after.role.clone(),
        visibility: after.visibility,
    };
    if state
        .auth
        .store
        .append_coworker(&coworker_id, &account_id, seq, &events, &view)
        .await
        .is_err()
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, "could not save").into_response();
    }
    let _ = state
        .auth
        .store
        .set_coworker_hidden(&account_id, &coworker_id, false, at_ms)
        .await;
    crate::agui::provision::teardown_computer_for(&state, &account_id, &coworker_id).await;
    crate::spend::revoke_for(&state, &coworker_id).await;
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Debug, Deserialize)]
pub struct HireRequest {
    pub name: String,
    /// A route through the gateway, never a key.
    #[serde(default)]
    pub model: Option<String>,
    /// One of the org's coworker templates (`templates.rs`): its model when none is given here,
    /// its tool ceiling and approval set, its spend limits — copied at hire. camelCase on the
    /// wire like the rest of this API; the snake_case name once made the picker a silent no-op.
    #[serde(default, rename = "templateId")]
    pub template_id: Option<String>,
}

/// Hire a coworker, and optionally give it a computer.
///
/// The account comes from the bearer token, never from the body: a client that could name an
/// account could hire into somebody else's roster.
pub async fn hire(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Json(request): Json<HireRequest>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };

    // The hirer as the roster will name them, read BEFORE anything is written: the reply is a
    // roster row, and a read failing after the hire committed could only answer 503 over a
    // coworker that exists — which a client retries into a second hire.
    let owner = match state.auth.store.roster_owner(&account_id).await {
        Ok(owner) => owner,
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };

    let coworker_id = CoworkerId::new();
    let at_ms = now_ms();
    // A template, when named, must be the hirer's org's: somebody else's reads as "no such
    // template", never as a hire on the deployment's defaults.
    let template = match request.template_id.as_deref().map(str::trim) {
        Some(id) if !id.is_empty() => {
            match crate::templates::for_account(&state, &account_id, id).await {
                Ok(Some(template)) => Some(template),
                Ok(None) => return (StatusCode::NOT_FOUND, "no such template").into_response(),
                Err(error) => {
                    return (StatusCode::SERVICE_UNAVAILABLE, error).into_response();
                }
            }
        }
        _ => None,
    };
    // Absent OR blank falls back: `unwrap_or_else` alone let `"model": ""` through, and the
    // aggregate would (now) refuse it rather than the caller getting the default they meant.
    // The template's pin sits between the request's and the deployment's.
    let model = request
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_string)
        .or_else(|| template.as_ref().and_then(|t| t.model.clone()))
        .unwrap_or_else(|| state.model.clone());

    let mut coworker = opengrok_core::coworker::Coworker::default();
    let mut events = match coworker.decide(CoworkerCommand::Hire {
        name: request.name.clone(),
        model: model.clone(),
        at_ms,
    }) {
        Ok(events) => events,
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };
    // The template's standing role, in the SAME append as the hire: a second write could fail
    // after the coworker exists, leaving a coworker the template promised a role and did not get.
    if let Some(role) = template.as_ref().and_then(|t| t.role.clone())
        && let Ok(more) = coworker.decide(CoworkerCommand::SetRole {
            role: Some(role),
            at_ms,
        })
    {
        events.extend(more);
    }
    for event in &events {
        coworker.apply(event);
    }

    // A computer, if asked for — via the shared helper so REST, gateway and seam-B create paths
    // behave identically. A failure leaves a boxless-but-hired coworker; the reason is in the reply.
    // 1 account = 1 computer: the account's first agent creates it, later agents share it.
    let provisioned =
        provision::ensure_computer_for(&state, &account_id, &coworker_id, &mut coworker, at_ms)
            .await;
    events.extend(provisioned.events);
    // Stamped with NOW, because this one is fresh by construction — it is the error from the
    // provisioning attempt this very request just made. Every `computerError` on the wire carries
    // `updatedAtMs` or the client cannot tell which of them it may trust; a field that is
    // sometimes present is worse than one that never is.
    let computer_error = provisioned
        .error
        .map(|(code, message)| (code, message, at_ms));
    // A key of its own, so a cap can be written on it. Never fails the hire; the console says
    // why when it could not be minted.
    let _key =
        crate::spend::ensure_key_for(&state, &account_id, &coworker_id, &coworker.name).await;

    let view = CoworkerView {
        id: coworker_id.clone(),
        name: coworker.name.clone(),
        model: coworker.model.clone(),
        box_id: coworker.computer().cloned(),
        retired: coworker.retired,
        members: coworker.members.clone(),
        updated_at_ms: at_ms,
        role: coworker.role.clone(),
        visibility: coworker.visibility,
    };

    if let Err(error) = state
        .auth
        .store
        .append_coworker(&coworker_id, &account_id, 0, &events, &view)
        .await
    {
        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
    }

    // Hiring grants the hirer access to what they hired. Written explicitly rather than implied by
    // ownership: "the owner may do anything" is the rule that has no seam to narrow later, and
    // coworker-to-coworker delegation will need one.
    //
    // The ceiling starts at the tools this server actually implements, not `All`: a coworker's
    // limits should be a list somebody can read, and `All` would silently include whatever is
    // added next.
    // The ceiling starts at the tools this server implements without any plugin. A plugin granted
    // to this coworker later must widen it — policy correctly refuses a tool nobody permitted, so
    // "install a plugin" and "let this coworker use it" stay two decisions rather than one.
    let tools =
        opengrok_policy::ToolSet::only(opengrok_tools::Executor::builtin_tool_names().to_vec());
    let mut template_note: Option<String> = None;
    let granted = match template.as_ref() {
        // Hired from a template: the template's ceiling, approval set and limits, copied. A
        // limit that could not be copied comes back as a note for the hirer.
        Some(template) => match crate::templates::apply_at_hire(
            &state,
            &account_id,
            &coworker_id,
            template,
            at_ms,
        )
        .await
        {
            Ok(note) => {
                template_note = note;
                Ok(())
            }
            Err(error) => Err(opengrok_store::StoreError::Corrupt(error)),
        },
        None => {
            state
                .auth
                .store
                // Nothing needs approval by default. A person who wants a second pair of eyes on
                // `shell` sets it deliberately; defaulting to "approve everything" would make the
                // prompt noise and teach people to click yes.
                .grant_access(
                    &account_id,
                    &coworker_id,
                    &tools,
                    &tools,
                    &opengrok_policy::ToolSet::None,
                    at_ms,
                )
                .await
        }
    };
    if let Err(error) = granted {
        // A coworker nobody may use is worse than no coworker: fail the hire rather than leave one
        // that silently refuses everything.
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("the coworker was created but could not be granted: {error}"),
        )
            .into_response();
    }

    // The roster's row, so a hired coworker does not change shape the first time the app
    // relaunches onto `GET /coworkers`; the two keys only a hire can answer ride on top of it.
    // Not hidden: nobody can have hidden an id minted a moment ago.
    let profile = template.as_ref().and_then(crate::templates::hire_profile);
    let mut row = coworker_row(&view, profile.as_ref(), false, &owner, &account_id);
    if let Some(row) = row.as_object_mut() {
        row.insert(
            "computerError".to_string(),
            provision::error_json_at(&computer_error),
        );
        // A sentence when something the template promised did not land; null otherwise.
        row.insert("templateNote".to_string(), serde_json::json!(template_note));
    }
    (StatusCode::CREATED, Json(row)).into_response()
}

#[derive(Debug, Deserialize)]
pub struct ApprovalsRequest {
    /// Tools this coworker may only run with a human yes. An empty list means none.
    #[serde(default)]
    pub tools: Vec<String>,
}

/// Say which of a coworker's tools need a person to approve them — PLAN §4.5 layer 5.
///
/// Set by the person who holds the grant, on their own grant: approval is about what *they* may
/// have done without asking, so it is a property of the grant and not of the coworker.
pub async fn set_approvals(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(coworker_id): Path<String>,
    Json(request): Json<ApprovalsRequest>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let coworker_id = CoworkerId::from_stored(coworker_id);

    // Only somebody who already holds a grant may change its approval list — otherwise this would
    // be a way to create a grant, which is a different permission entirely.
    let policy = match state.auth.store.policy_for(&account_id, &coworker_id).await {
        Ok(policy) => policy,
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };
    let decision = opengrok_policy::decide(
        &account_id,
        &coworker_id,
        opengrok_policy::Action::UseCoworker,
        &policy,
    );
    if let Some(reason) = decision.reason() {
        return refuse_use(&state, &account_id, &coworker_id, reason).await;
    }

    let (Some(grant), Some(ceiling)) = (policy.grant, policy.ceiling) else {
        return (StatusCode::FORBIDDEN, "no grant to change").into_response();
    };

    let needs_approval = if request.tools.is_empty() {
        opengrok_policy::ToolSet::None
    } else {
        opengrok_policy::ToolSet::only(request.tools.clone())
    };

    if let Err(error) = state
        .auth
        .store
        .grant_access(
            &account_id,
            &coworker_id,
            &grant.profile,
            &ceiling.tools,
            &needs_approval,
            now_ms(),
        )
        .await
    {
        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
    }

    Json(serde_json::json!({
        "coworkerId": coworker_id.as_str(),
        "needsApproval": request.tools,
    }))
    .into_response()
}

/// A refused use of a coworker, answered the way every per-coworker route answers: 404 when it
/// is not on the caller's roster, so an outsider's probe reads the same for a coworker that
/// exists and one that does not (#175) — and 403 with the rule's reason when it is on their
/// roster, because somebody who can see it already knows it exists, and a refusal they can read
/// is one they can act on (CLAUDE.md #8).
///
/// Asked only after the policy has refused, so it can never turn a refusal into an allow. A
/// store error keeps the 403: its reason is the no-grant sentence an unknown id gets too, so it
/// confirms nothing either.
pub(crate) async fn refuse_use(
    state: &AgUiState,
    account_id: &opengrok_core::id::AccountId,
    coworker_id: &CoworkerId,
    reason: &str,
) -> Response {
    match state
        .auth
        .store
        .may_use_coworker(account_id, coworker_id)
        .await
    {
        Ok(false) => (StatusCode::NOT_FOUND, "no such coworker").into_response(),
        Ok(true) | Err(_) => (StatusCode::FORBIDDEN, reason.to_string()).into_response(),
    }
}

/// The roster, newest first — the order the client sorts by.
pub async fn list_coworkers(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    // `roster_for`, not `coworkers_for`: the roster is what this person may TALK to, including
    // what an org-mate shared; `coworkers_for` is what they may manage, and stays owner-only.
    let coworkers = match state.auth.store.roster_for(&account_id).await {
        Ok(coworkers) => coworkers,
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };
    let hidden = match state.auth.store.hidden_coworker_ids(&account_id).await {
        Ok(ids) => ids,
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };
    // A 503 rather than rows without their decoration: a roster that answers 200 with every
    // title and avatar quietly gone is the app repainting a correct sidebar as a wrong one.
    let ids: Vec<CoworkerId> = coworkers.iter().map(|(view, _)| view.id.clone()).collect();
    let profiles = match state.auth.store.seamb_profiles(&ids).await {
        Ok(profiles) => profiles,
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };
    // An ARRAY, always. An empty roster is a valid answer and must not become null or an
    // object — the desktop client throws on a malformed array reply (RUNBOOK §4).
    let rows: Vec<serde_json::Value> = coworkers
        .iter()
        .map(|(view, owner)| {
            coworker_row(
                view,
                profiles.get(view.id.as_str()),
                hidden.contains(view.id.as_str()),
                owner,
                &account_id,
            )
        })
        .collect();
    Json(rows).into_response()
}

/// Whose account this is, from the bearer token. Never from the body.
pub(crate) fn account_from_bearer(
    state: &AgUiState,
    headers: &axum::http::HeaderMap,
) -> Option<opengrok_core::id::AccountId> {
    // Header OR the console's httpOnly cookie. The browser cannot send an Authorization header on
    // its own and must never hold a token in JS, so a cookie is the only way it can reach these —
    // it is the same access token, verified the same way, which is what account_api already does.
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_string)
        .or_else(|| {
            crate::auth::cookies::read_cookie(headers, crate::auth::cookies::ACCESS_COOKIE)
        })?;
    // Name the failure, as the seam-A path does. `.ok()?` here meant a mint refused with
    // "a signed access token is required" and no record of WHY — ExpiredSignature and
    // InvalidSignature are different bugs belonging to different people, and the caller cannot
    // tell you which because it only sees the 401.
    match state.auth.minter.verify_access(&token) {
        Ok(claims) => Some(opengrok_core::id::AccountId::from_stored(claims.sub)),
        Err(error) => {
            tracing::warn!(%error, token_len = token.len(), "a bearer access token did not verify");
            None
        }
    }
}

/// Whose account this is, from an `Authorization: Bearer` header and nothing else: the
/// console's cookie does not count here. For the doors only the person's own app may open.
pub(crate) fn account_from_header_bearer(
    state: &AgUiState,
    headers: &axum::http::HeaderMap,
) -> Option<opengrok_core::id::AccountId> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")?;
    match state.auth.minter.verify_access(token) {
        Ok(claims) => Some(opengrok_core::id::AccountId::from_stored(claims.sub)),
        Err(error) => {
            tracing::warn!(%error, token_len = token.len(), "a bearer access token did not verify");
            None
        }
    }
}

pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Mint a durable key that lets a client Bot run AS this coworker.
///
/// Access tokens live an hour; a Bot registered in a client's vault with a static header dies
/// hourly. This key is signed like everything else but LONG-lived, because its real lifecycle
/// control is the revocable row — showing the token once at mint is the only time it exists in
/// a reply, the same bargain every credential here makes.
async fn mint_bot_key(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(coworker_id): axum::extract::Path<String>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let coworker_id = CoworkerId::from_stored(coworker_id);
    // Owner-and-permitted, or a 404 that does not confirm the coworker exists.
    let owns = state
        .auth
        .store
        .coworkers_for(&account_id)
        .await
        .map(|roster| {
            roster
                .iter()
                .any(|view| view.id == coworker_id && !view.retired)
        })
        .unwrap_or(false);
    if !owns {
        return (StatusCode::NOT_FOUND, "no such coworker").into_response();
    }

    let minted = match crate::auth::bot_keys::mint(
        &state.auth.store,
        &state.auth.minter,
        &account_id,
        &coworker_id,
        "bot key",
        None,
        crate::auth::bot_keys::HAND_MINTED_TTL_SECS,
    )
    .await
    {
        Ok(minted) => minted,
        Err(error) => {
            tracing::error!(%error, "could not mint a bot key");
            return (StatusCode::INTERNAL_SERVER_ERROR, error).into_response();
        }
    };
    let jti = minted.jti;
    let token = minted.token;
    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "jti": jti,
            "coworkerId": coworker_id.as_str(),
            // Shown exactly once. The row keeps the jti; the token is the caller's to keep.
            "key": token,
        })),
    )
        .into_response()
}

/// `GET /templates` — the caller's org's coworker templates; `[]` outside any org.
async fn list_templates(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let org_id = match state.auth.store.load_account(&account_id).await {
        Ok((account, _)) => account.org_id.filter(|org| !org.is_empty()),
        Err(error) => {
            tracing::error!(%error, "could not load the account");
            return (StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response();
        }
    };
    let Some(org_id) = org_id else {
        return Json(serde_json::json!({ "templates": [] })).into_response();
    };
    match state.auth.store.templates_for_org(&org_id).await {
        Ok(templates) => Json(serde_json::json!({
            "templates": templates.iter().map(crate::templates::template_json).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(error) => {
            tracing::error!(%error, "could not list templates");
            (StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response()
        }
    }
}

/// Is this coworker the caller's? `None` ⇒ 404: another account's coworker id must read as
/// "no such coworker", never as an empty or refused one.
pub(crate) async fn owned_coworker(
    state: &AgUiState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
) -> Result<bool, Response> {
    match state.auth.store.coworkers_for(account_id).await {
        Ok(coworkers) => Ok(coworkers.iter().any(|c| c.id == *coworker_id)),
        Err(error) => {
            tracing::error!(%error, "could not list coworkers");
            Err((StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response())
        }
    }
}

/// `GET /coworkers/{id}/computer` — live box state and noVNC URL for NativeChat's Open button.
async fn computer_status(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(coworker_id): Path<String>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let coworker_id = CoworkerId::from_stored(coworker_id);
    match owned_coworker(&state, &account_id, &coworker_id).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::NOT_FOUND, "no such coworker").into_response(),
        Err(refusal) => return refusal,
    }
    Json(provision::coworker_screen(&state, &account_id, &coworker_id).await).into_response()
}

/// The person's standing answer, for this coworker's computer, to the tunnel's card.
#[derive(serde::Deserialize)]
struct EgressPolicyBody {
    mode: String,
}

/// The coworker's scoped box row for a policy read or write, or the refusal: 401 without a
/// bearer, 404 for another account's coworker or one with no computer. The provider is not
/// needed — the preference is a row keyed by the scope, and the pane paints the control even
/// while the provider cannot be built.
async fn egress_policy_box(
    state: &AgUiState,
    headers: &axum::http::HeaderMap,
    coworker_id: String,
) -> Result<(AccountId, CoworkerId, provision::ScopedBoxRow), Response> {
    let Some(account_id) = account_from_bearer(state, headers) else {
        return Err((StatusCode::UNAUTHORIZED, "sign in first").into_response());
    };
    let coworker_id = CoworkerId::from_stored(coworker_id);
    match owned_coworker(state, &account_id, &coworker_id).await {
        Ok(true) => {}
        Ok(false) => return Err((StatusCode::NOT_FOUND, "no such coworker").into_response()),
        Err(refusal) => return Err(refusal),
    }
    match provision::scoped_box_row_for(state, &account_id, &coworker_id).await {
        Some(row) => Ok((account_id, coworker_id, row)),
        None => Err((StatusCode::NOT_FOUND, "this coworker has no computer").into_response()),
    }
}

/// `GET /coworkers/{id}/computer/egress-policy` — `{ scope, scopeId, boxId, mode }`; an unset
/// policy reads as `ask`, which is what it means.
async fn get_egress_policy(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(coworker_id): Path<String>,
) -> Response {
    let (_, _, scoped) = match egress_policy_box(&state, &headers, coworker_id).await {
        Ok(found) => found,
        Err(refusal) => return refusal,
    };
    let Ok(mode) = provision::egress_policy_read(&state, scoped.scope, &scoped.scope_id).await
    else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "the network policy could not be read right now",
        )
            .into_response();
    };
    Json(serde_json::json!({
        "scope": provision::share_scope_of(scoped.scope),
        "scopeId": scoped.scope_id,
        "boxId": scoped.box_id,
        "mode": mode.as_stored(),
    }))
    .into_response()
}

/// `PUT /coworkers/{id}/computer/egress-policy` `{ "mode": "bypass" | "ask" | "never" }` — 204.
/// The choice is kept whether or not the tunnel is on right now: it says what happens when it is.
async fn set_egress_policy(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(coworker_id): Path<String>,
    Json(body): Json<EgressPolicyBody>,
) -> Response {
    if !opengrok_tools::EgressPolicy::is_valid(&body.mode) {
        return (StatusCode::UNPROCESSABLE_ENTITY, "unknown mode").into_response();
    }
    let (_, _, scoped) = match egress_policy_box(&state, &headers, coworker_id).await {
        Ok(found) => found,
        Err(refusal) => return refusal,
    };
    // An org-shared computer is every member's: its standing consent is the org admin's to
    // set, like the org's sharing mode. A member's own box, or the box of a group they run,
    // is theirs.
    if scoped.scope == "org"
        && let Err(refusal) = crate::account_api::admin_org(&state.auth, &headers).await
    {
        return refusal;
    }
    match state
        .auth
        .store
        .set_egress_policy_mode(
            scoped.scope,
            &scoped.scope_id,
            &body.mode,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => {
            tracing::error!(%error, "could not set the egress policy");
            (StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response()
        }
    }
}

/// `GET /coworkers/{id}/screen` — the box's display as a PNG, for the Computer pane's tile
/// and for an explicit observe / Open the screen. Same shape as `TOOL_CALL_RESULT.image`,
/// with `visibility: transcript` so a client that fetches this on purpose may persist it.
/// Step shots on the run are `agent` and must not flood the transcript.
/// `GET /coworkers/{id}/tools` — what this bot would be offered on a turn RIGHT NOW.
///
/// The set is assembled per turn from the coworker's grant, its computer and its plugins, and
/// until now it was only ever built inside the run handler and thrown away. A client that wants
/// to name a tool has to be able to see the names, and a name it cannot see is a name it would
/// guess wrong — so this answers with exactly what the model is told, nothing invented.
async fn list_tools(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(coworker_id): Path<String>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let coworker_id = CoworkerId::from_stored(coworker_id);
    match owned_coworker(&state, &account_id, &coworker_id).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::NOT_FOUND, "no such coworker").into_response(),
        Err(refusal) => return refusal,
    }
    // No approvals are pending on a listing, so the two gates are empty; the patience is short
    // because nobody is waiting on a turn — a sleeping box should not hold a menu open.
    let Some(runner) = tools_for_coworker(
        &state,
        &account_id,
        &coworker_id,
        &[],
        &[],
        std::time::Duration::from_secs(5),
    )
    .await
    else {
        return Json(serde_json::json!({ "tools": [] })).into_response();
    };
    let tools: Vec<serde_json::Value> = runner
        .tool_schemas()
        .into_iter()
        .filter_map(|schema| {
            let function = schema.get("function")?;
            let name = function.get("name")?.as_str()?.to_string();
            // OpenAI-safe plugin names have no dots (`gmail_api_send`). Kind is
            // "not a builtin", not "contains a dot".
            let kind = if opengrok_tools::Executor::builtin_tool_names().contains(&name.as_str())
                || name == opengrok_tools::USER_MACHINE_SHELL
            {
                "builtin"
            } else {
                "plugin"
            };
            Some(serde_json::json!({
                "name": name,
                "description": function.get("description").and_then(serde_json::Value::as_str).unwrap_or(""),
                "kind": kind,
            }))
        })
        .collect();
    Json(serde_json::json!({ "tools": tools })).into_response()
}

async fn computer_screen(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(coworker_id): Path<String>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let coworker_id = CoworkerId::from_stored(coworker_id);
    match owned_coworker(&state, &account_id, &coworker_id).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::NOT_FOUND, "no such coworker").into_response(),
        Err(refusal) => return refusal,
    }
    match provision::coworker_screenshot(&state, &account_id, &coworker_id).await {
        Ok(shot) => Json(serde_json::json!({
            "mime": shot.mime,
            "base64": shot.png_base64,
            "width": shot.width,
            "height": shot.height,
            // Explicit observe / Open the screen: this PNG is a transcript event
            // the client may persist. Step shots on TOOL_CALL_RESULT are `agent`.
            "visibility": "transcript",
        }))
        .into_response(),
        Err((status, message)) => (status, message).into_response(),
    }
}

/// `POST /coworkers/{id}/computer/update` — rebuild the coworker's computer on the newest image,
/// keeping its files. Answers 202 with the status; the phases arrive on `GET …/computer`.
async fn computer_update(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(coworker_id): Path<String>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let coworker_id = CoworkerId::from_stored(coworker_id);
    match owned_coworker(&state, &account_id, &coworker_id).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::NOT_FOUND, "no such coworker").into_response(),
        Err(refusal) => return refusal,
    }
    if let Err((status, message)) =
        provision::begin_update_for_coworker(&state, &account_id, &coworker_id).await
    {
        return (status, message).into_response();
    }
    (
        StatusCode::ACCEPTED,
        Json(provision::coworker_screen(&state, &account_id, &coworker_id).await),
    )
        .into_response()
}

/// `POST /coworkers/{id}/computer/reset` — destroy the computer, data and all, and start fresh.
async fn computer_reset(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(coworker_id): Path<String>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let coworker_id = CoworkerId::from_stored(coworker_id);
    match owned_coworker(&state, &account_id, &coworker_id).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::NOT_FOUND, "no such coworker").into_response(),
        Err(refusal) => return refusal,
    }
    if let Err((code, message)) =
        provision::reset_for_coworker(&state, &account_id, &coworker_id).await
    {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("{code}: {message}"),
        )
            .into_response();
    }
    Json(provision::coworker_screen(&state, &account_id, &coworker_id).await).into_response()
}

/// `POST /coworkers/{id}/computer` — ensure the box is running, then return the same status.
async fn ensure_computer(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(coworker_id): Path<String>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let coworker_id = CoworkerId::from_stored(coworker_id);
    match owned_coworker(&state, &account_id, &coworker_id).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::NOT_FOUND, "no such coworker").into_response(),
        Err(refusal) => return refusal,
    }
    let Ok((mut coworker, seq)) = state.auth.store.load_coworker(&coworker_id).await else {
        return (StatusCode::NOT_FOUND, "no such coworker").into_response();
    };
    let at_ms = now_ms();
    let provisioned =
        provision::ensure_computer_for(&state, &account_id, &coworker_id, &mut coworker, at_ms)
            .await;
    if !provisioned.events.is_empty() {
        for event in &provisioned.events {
            coworker.apply(event);
        }
        let view = CoworkerView {
            id: coworker_id.clone(),
            name: coworker.name.clone(),
            model: coworker.model.clone(),
            box_id: coworker.computer().cloned(),
            retired: coworker.retired,
            members: coworker.members.clone(),
            updated_at_ms: at_ms,
            role: coworker.role.clone(),
            visibility: coworker.visibility,
        };
        if let Err(error) = state
            .auth
            .store
            .append_coworker(&coworker_id, &account_id, seq, &provisioned.events, &view)
            .await
        {
            return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
        }
    }
    provision::wake_coworker_computer(&state, &account_id, &coworker_id).await;
    Json(provision::coworker_screen(&state, &account_id, &coworker_id).await).into_response()
}

/// `GET /coworkers/{id}/spend` — the coworker's three meters and the limits it is under.
async fn get_spend(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(coworker_id): Path<String>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let coworker_id = CoworkerId::from_stored(coworker_id);
    match owned_coworker(&state, &account_id, &coworker_id).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::NOT_FOUND, "no such coworker").into_response(),
        Err(refusal) => return refusal,
    }
    match crate::spend::spend_for(&state, &account_id, &coworker_id).await {
        Ok(spend) => Json(spend).into_response(),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error).into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct UsageQuery {
    pub window: Option<String>,
}

/// `GET /coworkers/{id}/usage?window=5h|24h|7d|month` — what the coworker used, per model.
async fn get_usage(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(coworker_id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<UsageQuery>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let coworker_id = CoworkerId::from_stored(coworker_id);
    match owned_coworker(&state, &account_id, &coworker_id).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::NOT_FOUND, "no such coworker").into_response(),
        Err(refusal) => return refusal,
    }
    let window = query.window.as_deref().unwrap_or("month");
    match crate::points::usage_for(&state, &account_id, &coworker_id, window).await {
        Ok(usage) => Json(usage).into_response(),
        Err((code, error)) => (
            StatusCode::from_u16(code).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
            Json(serde_json::json!({ "error": error })),
        )
            .into_response(),
    }
}

/// `GET /coworkers/{id}/limit` — the coworker's cap and brake, what it has used, and the pool
/// it draws on.
async fn get_limit(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(coworker_id): Path<String>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let coworker_id = CoworkerId::from_stored(coworker_id);
    match owned_coworker(&state, &account_id, &coworker_id).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::NOT_FOUND, "no such coworker").into_response(),
        Err(refusal) => return refusal,
    }
    match crate::points::limit_for(&state, &account_id, &coworker_id).await {
        Ok(limit) => Json(limit).into_response(),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": error })),
        )
            .into_response(),
    }
}

/// `PUT /coworkers/{id}/limit` ← `{ cap, dayCap }` — the owner's; null clears, absent leaves.
async fn set_limit(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(coworker_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let coworker_id = CoworkerId::from_stored(coworker_id);
    match owned_coworker(&state, &account_id, &coworker_id).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::NOT_FOUND, "no such coworker").into_response(),
        Err(refusal) => return refusal,
    }
    match crate::points::set_limit(&state, &account_id, &coworker_id, &body).await {
        Ok(limit) => Json(limit).into_response(),
        Err((code, error)) => (
            StatusCode::from_u16(code).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
            Json(serde_json::json!({ "error": error })),
        )
            .into_response(),
    }
}

/// `GET /coworkers/{id}/keys` — the owner's bot keys for this coworker. Anyone else's id, a
/// shared coworker's included, is a 404 like every other coworker route: the query alone
/// answered `[]` to a stranger and an unknown id alike, which is the empty success that reads
/// as "no keys" rather than "not yours".
async fn list_bot_keys(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(coworker_id): axum::extract::Path<String>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let coworker_id = CoworkerId::from_stored(coworker_id);
    match owned_coworker(&state, &account_id, &coworker_id).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::NOT_FOUND, "no such coworker").into_response(),
        Err(refusal) => return refusal,
    }
    match state
        .auth
        .store
        .bot_keys_for(&account_id, &coworker_id)
        .await
    {
        Ok(keys) => Json(keys).into_response(),
        Err(error) => {
            tracing::error!(%error, "could not list bot keys");
            (StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response()
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct McpCallsQuery {
    limit: Option<i64>,
}

/// `GET /coworkers/{id}/mcp-calls?limit=` — what this coworker's bot keys have been used for,
/// newest first. The owner only: another account's coworker id is a 404, not an empty list
/// (an empty success is the dangerous reply).
async fn list_mcp_calls(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(coworker_id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<McpCallsQuery>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let coworker_id = CoworkerId::from_stored(coworker_id);
    let owned = match state.auth.store.coworkers_for(&account_id).await {
        Ok(coworkers) => coworkers.iter().any(|c| c.id == coworker_id),
        Err(error) => {
            tracing::error!(%error, "could not list coworkers");
            return (StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response();
        }
    };
    if !owned {
        return (StatusCode::NOT_FOUND, "no such coworker").into_response();
    }
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    match state
        .auth
        .store
        .mcp_calls_for(&account_id, &coworker_id, limit)
        .await
    {
        Ok(calls) => Json(calls).into_response(),
        Err(error) => {
            tracing::error!(%error, "could not list mcp door calls");
            (StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response()
        }
    }
}

/// `DELETE /coworkers/{id}/keys/{jti}` — keyed by the caller's own key, not by the path's
/// coworker, and deliberately not gated on `owned_coworker`: retiring a coworker does not revoke
/// its bot keys, so a gate on the retired (off-roster) row would leave a key the owner still holds
/// live with no door to revoke it through. Somebody else's key, or an id that is no key, is the
/// same 404, so nothing about the path's coworker is confirmed.
async fn revoke_bot_key(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    axum::extract::Path((_coworker_id, jti)): axum::extract::Path<(String, String)>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    // An OAuth-minted key has refresh tokens that would mint it a successor; they die with it
    // in ONE transaction, or "revoke" would mean "revoke until the next refresh". A failure is
    // a 500 the person sees, never a 204 over a key that can still come back.
    match state
        .auth
        .store
        .revoke_bot_key_with_refresh(&account_id, &jti)
        .await
    {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "no such key").into_response(),
        Err(error) => {
            tracing::error!(%error, "could not revoke a bot key");
            (StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response()
        }
    }
}

/// What a bot key says about itself. The `use` claim is the discriminator the minter's own
/// documentation demands: without it, a stolen access token would verify here too.
pub(crate) use crate::auth::bot_keys::BotKeyClaims;

/// Who is calling, and — when the credential is a bot key — AS which coworker.
///
/// Three outcomes, and the middle one matters most: `Err(response)` is a bot key that VERIFIES
/// but is revoked or unknown. That must refuse rather than fall through to anonymous, or a
/// revoked Bot silently keeps talking on the deployment's model and nobody notices the
/// revocation did nothing.
pub(crate) async fn principal_from_bearer(
    state: &AgUiState,
    headers: &axum::http::HeaderMap,
) -> Result<Option<(opengrok_core::id::AccountId, Option<CoworkerId>)>, Response> {
    let Some(token) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
        return Ok(None);
    };
    if let Ok(claims) = state.auth.minter.verify_access(token) {
        return Ok(Some((
            opengrok_core::id::AccountId::from_stored(claims.sub),
            None,
        )));
    }
    if let Ok(claims) = state.auth.minter.verify_claims::<BotKeyClaims>(token) {
        if claims.purpose != "bot-key" {
            return Ok(None);
        }
        let live = state
            .auth
            .store
            .bot_key_live(&claims.jti)
            .await
            .unwrap_or(false);
        if !live {
            return Err((StatusCode::UNAUTHORIZED, "this bot key has been revoked").into_response());
        }
        return Ok(Some((
            opengrok_core::id::AccountId::from_stored(claims.sub),
            Some(CoworkerId::from_stored(claims.coworker)),
        )));
    }
    Ok(None)
}

/// Start a run and stream its events.
pub async fn run(
    State(gateway): State<crate::host_state::HostState>,
    headers: axum::http::HeaderMap,
    Json(input): Json<RunAgentInput>,
) -> Response {
    // `AgUiState` has no path to the live bus (`HostState` owns it). This handler lives on
    // `HostState` so a UserForm CUSTOM can mint the card and stamp `entryId` before the
    // SSE frame is sent; the rest of the turn still reads `agui` the same way every other
    // AG-UI path does.
    let state = gateway.agui.clone();
    // Who is asking. Established first, because the permission check, the run's ownership and the
    // model it thinks with all depend on it.
    //
    // Layer 1, every turn: may this principal talk to this coworker at all? An anonymous run gets
    // no tools rather than being refused outright — the AG-UI endpoint is also how a client with
    // no coworker just talks to a model.
    let (account_id, key_coworker) = match principal_from_bearer(&state, &headers).await {
        Ok(Some((account, coworker))) => (Some(account), coworker),
        Ok(None) => (None, None),
        // A revoked bot key refuses; downgrading to anonymous would make revocation invisible.
        Err(refusal) => return refusal,
    };
    // A BOT KEY NAMES THE COWORKER. barok-works registers a Bot with an endpoint and a header —
    // it has no forwardedProps to send — so the key itself carries which coworker the Bot IS.
    // An explicit forwardedProps still wins: a client that says what it means is believed.
    let run_coworker = coworker_id_from(&input).or(key_coworker);

    // A COWORKER NAMED BY A CALLER WE CANNOT NAME BACK IS REFUSED HERE, NOT LATER. `coworker_id_from`
    // reads `forwardedProps`, which anyone can send, so this pair is reachable from outside: a turn
    // that names a coworker and carries no credential.
    //
    // It could only ever fail, and it failed late and in the wrong words. Every gate below is keyed
    // on having BOTH — the policy check, the coworker's own model and role, its tools and its
    // persona are all skipped — and the turn then reached the spend guard carrying a scope with no
    // payer, which held it with "This turn does not say whose spend it is … This is a server bug".
    // The guard was right, and the sentence was ours to prevent: it named our defect in a place the
    // person could only read as a limit they had hit.
    //
    // Anonymous turns stay allowed. What is refused is naming somebody else's coworker while
    // declining to say who you are.
    if run_coworker.is_some() && account_id.is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            "that turn names a coworker, so it needs a signed-in caller; sign in and send it again",
        )
            .into_response();
    }

    // The deployment's model is the default, not the answer: a named coworker overrides it below.
    let mut model = state.model.clone();
    let mut coworker_name = String::new();
    let mut coworker_role: Option<String> = None;

    if let (Some(account_id), Some(coworker_id)) = (&account_id, run_coworker.clone()) {
        // `policy_to_use`, not `policy_for`: a coworker an org-mate shared is one this person may
        // talk to under the owner's grant, read now. A store error refuses too, but as a 503
        // that says so: an empty context would deny with "no grant lets …", which sends the
        // person off to repair a grant that is fine.
        let policy = match state
            .auth
            .store
            .policy_to_use(account_id, &coworker_id)
            .await
        {
            Ok(policy) => policy,
            Err(error) => {
                tracing::error!(%error, coworker = %coworker_id.as_str(), "the run door could not read the policy; the turn is refused");
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "the permission check could not be read right now, so nothing ran; send it \
                     again in a moment",
                )
                    .into_response();
            }
        };
        let decision = opengrok_policy::decide(
            account_id,
            &coworker_id,
            opengrok_policy::Action::UseCoworker,
            &policy,
        );
        if let Some(reason) = decision.reason() {
            return refuse_use(&state, account_id, &coworker_id, reason).await;
        }

        // WHICH MODEL A COWORKER THINKS WITH IS THE COWORKER'S, NOT THE DEPLOYMENT'S. Hiring takes
        // a model and stores it, and the roster reports it; a run that read past it left every one
        // of those answers describing a choice that never happened — a coworker hired on one model
        // silently answered on another, and the only visible symptom was the bill.
        //
        // AFTER the policy check and only for a named principal. An anonymous caller may still talk
        // to the deployment's model, but must not learn a coworker's configuration by noticing
        // which model replies.
        //
        // A coworker that cannot be loaded keeps the default rather than failing the run: the model
        // is how the turn is answered, not whether it is allowed, and that question was just asked.
        if let Ok((coworker, _)) = state.auth.store.load_coworker(&coworker_id).await {
            model = coworker.model.clone();
            coworker_name = coworker.name;
            coworker_role = coworker.role;
        }
    }

    // Refuse stale sends before interrupting a parked turn or preparing any model work.
    if let Some(account) = &account_id {
        if let Err(refusal) =
            crate::agui::pending::consume_for_turn(&state.auth.store, account, &input).await
        {
            return refusal;
        }
    } else if crate::agui::pending::pending_id_from(&input).is_some() {
        return (StatusCode::UNAUTHORIZED, "sign in to send a queued message").into_response();
    }

    // PAST THE CLAIM, A HANG-UP MUST NOT CANCEL THE TURN. The queued send is drained now, and the
    // setup below waits on a box wake, the store and the door. Run inline, a client that timed
    // out there dropped this future and left the row drained under a run that never started. The
    // turn itself was always spawned; now the setup that leads to it is too.
    let turn = tokio::spawn(start_claimed_turn(
        gateway,
        input,
        account_id,
        run_coworker,
        model,
        coworker_name,
        coworker_role,
    ));
    match turn.await {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(%error, "a claimed turn's setup did not finish");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "this turn could not be started",
            )
                .into_response()
        }
    }
}

/// Everything a turn does once any queued send it fires has been claimed.
async fn start_claimed_turn(
    gateway: crate::host_state::HostState,
    input: RunAgentInput,
    account_id: Option<opengrok_core::id::AccountId>,
    run_coworker: Option<CoworkerId>,
    model: String,
    coworker_name: String,
    coworker_role: Option<String>,
) -> Response {
    let state = gateway.agui.clone();
    // A RUN ID THAT ALREADY HAS A RUN IS NOT A NEW TURN. A client retrying its POST — its stream
    // dropped, or it sent the same body twice — used to start a second loop on the same run: a
    // second model call, every tool twice, one transcript interleaving both. Answered here,
    // before anything below has a side effect (a parked card stopped, a box woken). AFTER the
    // queued-send check, deliberately: a same-run retry whose words changed gets that check's
    // own refusal (`stale-pending-message`), which says more than being handed the old run.
    // The claim further down is what makes it exact: two POSTs at once can both get past this.
    if let Some(answer) = answer_for_existing_run(&state, account_id.as_ref(), &input.run_id).await
    {
        return answer;
    }
    if let (Some(account_id), Some(coworker_id)) = (&account_id, &run_coworker) {
        crate::agui::resume::interrupt_parked_hitl(
            &gateway,
            account_id,
            coworker_id,
            account_id.as_str(),
        )
        .await;
    }

    // Read once. The tool runner needs it to bind the run, and the system prompt needs it to say
    // so; reading it twice would let the two disagree about what the person chose.
    let chosen = chosen_recipe_from(&input);
    let tools = match &account_id {
        Some(account_id) => match &run_coworker {
            Some(coworker_id) => {
                let runner = tools_for_coworker(
                    &state,
                    account_id,
                    coworker_id,
                    &[],
                    &[],
                    TURN_WAKE_PATIENCE,
                )
                .await;
                match (runner, chosen.clone()) {
                    (Some(runner), Some((recipe, values))) => {
                        Some(runner.with_chosen_recipe(recipe, values))
                    }
                    (runner, _) => runner,
                }
            }
            None => None,
        },
        // No bearer, no identity, and therefore no computer tools.
        None => None,
    };
    // bar_chart / form are painted by the client from TOOL_CALL frames. Offer them on
    // every AG-UI turn, including coworkers with no computer, so a chart request is a
    // tool call rather than streamed markdown.
    let tools = Some(super::chat_ui::attach(tools));

    // Who this coworker is, plus whose computer its tools touch. Desktop `sendPrompt` already
    // composes this; AG-UI used to send `system: None`, so a Description saved as the standing
    // role never reached the model. Anonymous runs still compose nothing — there is nobody to
    // introduce, and loading a named coworker's role without a principal would leak configuration
    // by the shape of the reply.
    let mut recorded_skill: Option<String> = None;
    let system = match (account_id.as_ref(), run_coworker.as_ref()) {
        (Some(account_id), Some(coworker_id)) => {
            let persona = crate::persona::of(&state, coworker_id, coworker_role).await;
            let has_computer = tools.is_some();
            let reaches_user_machine = tools.as_ref().is_some_and(|runner| {
                runner
                    .tool_schemas()
                    .iter()
                    .any(|schema| schema["function"]["name"] == opengrok_tools::USER_MACHINE_SHELL)
            });
            let user_machine_label = if reaches_user_machine {
                crate::local_exec::enabled_machine(&state.auth.store, account_id.as_str())
                    .await
                    .map(|(_id, label)| label)
                    .filter(|label| !label.trim().is_empty())
            } else {
                None
            };
            let has_screen = tools.as_ref().is_some_and(|runner| runner.has_screen());
            let network_off = tools.as_ref().is_some_and(|runner| runner.network_off());
            let network_unconfirmed = tools
                .as_ref()
                .is_some_and(|runner| runner.network_unconfirmed());
            let has_recipes = tools.as_ref().is_some_and(|runner| runner.has_recipes());
            // What the person named this turn, kept to what this bot can actually run.
            let preferred = honour_preferences(&preferred_tools_from(&input), tools.as_ref());
            // The recipe is named to the model by its NAME, not its id: the sentence is read by
            // something that has to repeat it back to a person, and `rcp_01a0ac0a-…` is not a
            // thing anybody chose. A recipe the store cannot produce says nothing at all rather
            // than naming an id, because a prompt that cites what the person cannot see is worse
            // than a prompt that stays quiet.
            let chosen_line = match (&chosen, has_recipes) {
                (Some((recipe_id, values)), true) => {
                    match state.auth.store.recipe(recipe_id).await {
                        Ok(Some(row)) => crate::persona::chosen_recipe_line(&row.name, values),
                        _ => String::new(),
                    }
                }
                _ => String::new(),
            };
            // The skill the person chose, read against the account the token named and never
            // against anything in the body. A skill that cannot be given does not cost the turn —
            // it costs the skill, and the coworker is told to say so rather than answer as though
            // it had followed instructions it never saw (CLAUDE.md #8). No id on this request
            // reuses the skill from a prior run on this thread.
            let (skill_line, skill_id) =
                skill_line_for_turn(&state, account_id, &input.thread_id, &input).await;
            recorded_skill = skill_id;
            let text = crate::persona::system_message(
                &coworker_name,
                &persona,
                Some(&format!(
                    "{}{}{}{}{}",
                    crate::persona::computer_system_prompt(
                        has_computer,
                        has_screen,
                        has_recipes,
                        reaches_user_machine,
                        user_machine_label.as_deref(),
                    ),
                    crate::persona::network_off_line(network_off, network_unconfirmed),
                    crate::persona::preferred_tools_line(&preferred),
                    chosen_line,
                    // LAST, AFTER EVERY SEGMENT THAT SAYS WHAT THIS COWORKER MAY DO. A skill body
                    // is prose a person wrote; it must not be able to read as granting itself
                    // something the segments above just withheld. `persona::chosen_skill_line`
                    // carries the rest of the reason, and an empty one adds nothing at all — a
                    // turn with no skill is byte-for-byte the turn we had before.
                    skill_line,
                )),
            );
            if text.is_empty() { None } else { Some(text) }
        }
        // A turn with no coworker composes no persona — there is nobody to introduce, and loading
        // a named coworker's role without a principal would leak configuration by the shape of the
        // reply. A SKILL CHOSEN ON SUCH A TURN IS STILL SOMETHING THE PERSON ASKED FOR AND IS NOT
        // GETTING, and dropping it here without a word was exactly the silence the refusal line
        // exists to prevent. It is the generic sentence because there is no account to resolve the
        // skill against, and trimmed because there is no paragraph above it to join.
        _ => match chosen_skill_from(&input) {
            Some(_) => {
                tracing::warn!("a turn carried a chosen skill with no coworker to give it to");
                Some(crate::persona::SKILL_UNAVAILABLE_LINE.trim().to_string())
            }
            None => None,
        },
    };

    let mut messages = to_chat_messages(&input);
    // ONE system message. A client-supplied `system` in the AG-UI body would be a second claim
    // about the same coworker; drop it when we composed one.
    if system.is_some() {
        messages.retain(|message| message.role != "system");
    }
    // NativeChat steer is stop, then a new run whose body is chat bubbles. A stopped
    // turn often has tool results and no assistant text, so the next run repeats the
    // work. Splice those results in front of the new message. A finished answer is
    // already in the bubbles. A parked card is a fresh turn, not a continuation.
    if let Some(account) = &account_id {
        continue_stopped_turn(
            &state,
            account,
            &input.thread_id,
            &input.run_id,
            &mut messages,
        )
        .await;
    }

    let request = ModelRequest {
        gateway_key: crate::spend::key_for_opt(&state, run_coworker.as_ref(), account_id.as_ref())
            .await,
        spend_scope: run_coworker.as_ref().map(|c| c.as_str().to_string()),
        // An anonymous AG-UI run names nobody, so it is billed to nobody and the guard lets it
        // through on the deployment's key — the same door an anonymous caller already had.
        spend_actor: account_id.as_ref().map(|a| a.as_str().to_string()),
        model,
        system: system.clone(),
        messages,
        tools: Vec::new(),
    };

    // The journal writes each round to Postgres before the next model call, and stamps the run's
    // owner so only they can read it back. A run that cannot be recorded fails inside the loop
    // rather than being streamed (CLAUDE.md #5).
    let journal = StoreJournal {
        state: state.clone(),
        thread_id: input.thread_id.clone(),
        account_id: account_id.clone(),
        coworker_id: run_coworker,
        model: Some(request.model.clone()),
        system,
        skill_id: recorded_skill,
    };

    // THE CLAIM. Exactly one POST per run id appends the run's `Started` at the first seq, under
    // the unique `(stream_id, stream_seq)`, and only that one runs a loop
    // (`formal/tla/RunLifecycle.tla` OneDriver). One that lost to a POST racing it is answered
    // the way the check at the top answers a retry.
    match journal.claim(&input.run_id).await {
        Ok(true) => {}
        Ok(false) => {
            return answer_for_existing_run(&state, account_id.as_ref(), &input.run_id)
                .await
                .unwrap_or_else(run_taken);
        }
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    }

    // Hold the run while we serve it, so a recovery sweep does not mistake a slow model call for
    // an abandoned run. Released when the spawned turn drops — including when the process dies,
    // which is exactly the case the lease exists for.
    let thread_id = input.thread_id.clone();
    let run_id = input.run_id.clone();
    let at_ms = now_ms();
    let door = state.door.clone();
    let lease = crate::recovery::Lease::new(crate::recovery::hold(
        state.clone(),
        RunId::from_stored(run_id.clone()),
    ));
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    // The HTTP body is the live sink. Awaiting the conversation first, then wrapping the
    // Vec in `stream::iter`, is what made NativeChat paint the whole reply at once.
    tokio::spawn(async move {
        let _lease = lease;
        // Card mint + `entryId` stamp happen inside the sink, **before** the CUSTOM frame
        // is forwarded, so NativeChat sees the id on the AG-UI stream.
        let sink = AgUiSink {
            tx,
            gateway,
            coworker_id: journal.coworker_id.clone(),
            account_id: journal.account_id.clone(),
            form_hold: Mutex::new(crate::agui::user_form::UserFormSseHold::default()),
        };
        let _ = run_conversation_streaming(
            door.as_ref(),
            tools.as_ref(),
            &journal,
            request,
            &thread_id,
            &run_id,
            at_ms,
            &sink,
        )
        .await;
    });
    sse(futures::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|event| (Ok::<_, std::io::Error>(event), rx))
    }))
}

/// The event store, as the harness's journal.
///
/// The harness owns *when* to write (before the next model call); this owns *where*. Keeping them
/// apart is what lets the ordering rule be tested without a database and still be enforced against
/// one.
pub struct StoreJournal {
    pub state: AgUiState,
    pub thread_id: String,
    /// Whose run this is, so it can be read back by them and by nobody else.
    pub account_id: Option<opengrok_core::id::AccountId>,
    /// Which coworker is doing the work. Recorded on the run because a run that is answered days
    /// later has to know whose tools to continue with, and the request that started it is long gone.
    pub coworker_id: Option<CoworkerId>,
    /// The pin this turn captured. Written on `RunCommand::Start` so a resume does not reload
    /// a coworker that was repinned while we were waiting.
    pub model: Option<String>,
    /// The composed system message this turn opened with, captured for the same reason as the
    /// pin: a role edited while a person answered an approval card must not change the coworker
    /// halfway through the turn.
    pub system: Option<String>,
    /// The skill quoted into `system`, recorded so the next message on this thread
    /// can reuse it when the client sends no skill id.
    pub skill_id: Option<String>,
}

#[async_trait::async_trait]
impl opengrok_harness::RunJournal for StoreJournal {
    async fn record(
        &self,
        run_id: &str,
        events: &[Event],
    ) -> Result<(), opengrok_harness::JournalError> {
        append_events(&self.state, run_id, &self.run_start(), events)
            .await
            .map_err(|error| match error {
                AppendError::Ended => opengrok_harness::JournalError::Ended(format!(
                    "run {run_id} ended before its card could be recorded"
                )),
                AppendError::Store(error) => {
                    opengrok_harness::JournalError::Unwritable(error.to_string())
                }
            })
    }

    /// The log's own answer to "has somebody stopped this run". One primary-key read of the
    /// projection, not a replay: the loop asks this at every step boundary.
    ///
    /// A READ THAT FAILS SAYS NO. The turn is already running and already spending; stopping it
    /// because the database blinked would turn a hiccup into a cancelled turn, and the person who
    /// really did press stop still has a durable `Stopped` in the log for the next boundary to
    /// find.
    ///
    /// ANY ENDED RUN ANSWERS YES. A loop only runs on a run it claimed or resumed, so a run that
    /// ended under it was ended from outside — failed by the sweep after a renewal was lost —
    /// and carrying on only spends on events the log refuses (`formal/tla/RunLifecycle.tla`
    /// AtMostOneStaleTool). Before the claim this broke a same-runId retry, which ran a loop on
    /// an ended run and was told to stop before it said a word (the slice2 smoke, 23 Sep 2026).
    /// The cost: a loop whose run the sweep failed ends its live stream as `run-stopped`, though
    /// nobody pressed Stop; the log keeps the real ending.
    async fn stopped(&self, run_id: &str) -> bool {
        let run_id = RunId::from_stored(run_id.to_string());
        match self.state.auth.store.run_status(&run_id).await {
            Ok(status) => status.is_some_and(|status| status.is_terminal()),
            Err(error) => {
                tracing::warn!(%error, run = %run_id, "could not read whether a run was stopped");
                false
            }
        }
    }
}

impl StoreJournal {
    fn run_start(&self) -> RunStart<'_> {
        RunStart {
            thread_id: &self.thread_id,
            account_id: self.account_id.as_ref(),
            coworker_id: self.coworker_id.as_ref(),
            model: self.model.as_deref(),
            system: self.system.as_deref(),
            skill_id: self.skill_id.as_deref(),
        }
    }

    /// Start `run_id` for this turn. `Ok(false)` is a run that already exists, from an earlier
    /// POST with the same id or one racing this one: not this turn's to run.
    pub async fn claim(&self, run_id: &str) -> Result<bool, opengrok_store::StoreError> {
        let id = RunId::from_stored(run_id.to_string());
        let (mut run, seq) = self.state.auth.store.load_run(&id).await?;
        if run.started {
            return Ok(false);
        }
        let at_ms = now_ms();
        let started = run
            .decide(start_command(&self.run_start(), at_ms))
            .map_err(|error| opengrok_store::StoreError::Corrupt(error.to_string()))?;
        for event in &started {
            run.apply(event);
        }
        let view = RunView {
            id: id.clone(),
            thread_id: self.thread_id.clone(),
            status: run.status,
            event_count: run.emitted.len() as i64,
            updated_at_ms: at_ms,
        };
        match self
            .state
            .auth
            .store
            .append_run(&id, seq, &started, &view, self.account_id.as_ref())
            .await
        {
            Ok(_) => Ok(true),
            Err(opengrok_store::StoreError::Conflict) => Ok(false),
            Err(error) => Err(error),
        }
    }
}

/// The `Start` a run records about itself: whose turn, and what it opened with.
fn start_command(start: &RunStart<'_>, at_ms: i64) -> RunCommand {
    RunCommand::Start {
        thread_id: start.thread_id.to_string(),
        coworker_id: start.coworker_id.cloned(),
        model: start
            .model
            .map(str::trim)
            .filter(|pin| !pin.is_empty())
            .map(str::to_string),
        system: start
            .system
            .map(str::to_string)
            .filter(|text| !text.is_empty()),
        skill_id: start
            .skill_id
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string),
        at_ms,
    }
}

/// Attempts at a journal write that loses the race to another writer: one more than a run has
/// writers at once (its turn, a Stop, the sweep), like `STOP_ATTEMPTS`.
const APPEND_ATTEMPTS: usize = 5;

/// Append a batch of a run's events to the log, starting the run if this is its first batch.
///
/// RETRIED ON A CONFLICT, AND ONLY ON ONE. A Conflict is a write that lost the race and wrote
/// nothing, so reading and deciding again is what a write a moment later would have done. Without
/// it the turn lost every race to a Stop (which retries its own), and the round on screen when the
/// button was pressed left the log. Any other error may be a commit whose reply was lost: writing
/// it again would log the round twice (`formal/tla/JournalAppend.tla`).
async fn append_events(
    state: &AgUiState,
    run_id: &str,
    start: &RunStart<'_>,
    events: &[Event],
) -> Result<(), AppendError> {
    for _ in 1..APPEND_ATTEMPTS {
        match append_events_once(state, run_id, start, events).await {
            Err(AppendError::Store(opengrok_store::StoreError::Conflict)) => continue,
            other => return other,
        }
    }
    append_events_once(state, run_id, start, events).await
}

/// Why a batch was not written.
#[derive(Debug)]
enum AppendError {
    Store(opengrok_store::StoreError),
    /// The batch parks, and the run it parks has ended: the Stop won the race the loop's last
    /// `stopped` question could not see. Nothing was written (`RunJournal::record`).
    Ended,
}

impl From<opengrok_store::StoreError> for AppendError {
    fn from(error: opengrok_store::StoreError) -> Self {
        Self::Store(error)
    }
}

/// What a run records about itself at its first batch. A struct rather than five more
/// parameters: these are one fact — whose turn this is and what it opened with — and they are
/// only ever passed together.
struct RunStart<'a> {
    thread_id: &'a str,
    account_id: Option<&'a opengrok_core::id::AccountId>,
    coworker_id: Option<&'a CoworkerId>,
    model: Option<&'a str>,
    system: Option<&'a str>,
    skill_id: Option<&'a str>,
}

/// One attempt: read the run, decide what this batch appends, write it at the seq it read.
async fn append_events_once(
    state: &AgUiState,
    run_id: &str,
    start: &RunStart<'_>,
    events: &[Event],
) -> Result<(), AppendError> {
    let RunStart {
        thread_id,
        account_id,
        ..
    } = *start;
    if events.is_empty() {
        return Ok(());
    }
    let run_id = RunId::from_stored(run_id.to_string());
    let at_ms = now_ms();

    let (mut run, seq) = state.auth.store.load_run(&run_id).await?;
    let mut to_append = Vec::new();

    if !run.started {
        let started = run.decide(start_command(start, at_ms)).map_err(|error| {
            AppendError::Store(opengrok_store::StoreError::Corrupt(error.to_string()))
        })?;
        for event in &started {
            run.apply(event);
        }
        to_append.extend(started);
    }
    // A batch that parks must record its suspension, or record nothing.
    let parks = events.iter().any(is_suspend_frame);

    for event in events {
        let payload = serde_json::to_value(event).map_err(|error| {
            AppendError::Store(opengrok_store::StoreError::Corrupt(error.to_string()))
        })?;
        // The aggregate refuses a frame after an ending; that is a rule, not a hiccup.
        let Ok(decided) = run.decide(RunCommand::Emit { payload, at_ms }) else {
            if parks {
                return Err(AppendError::Ended);
            }
            break;
        };
        for decided_event in &decided {
            run.apply(decided_event);
        }
        to_append.extend(decided);

        // A suspension carries which call is waiting, so a person can answer *that* call later.
        // Read from the event the projection emitted, because the harness is the only thing that
        // knows the run stopped.
        if is_suspend_frame(event) {
            let call_id = event
                .extra
                .get("callId")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string();
            if !call_id.is_empty() {
                // REFUSED ONLY WHEN THE RUN HAS ENDED. Dropping the refusal and writing the rest
                // put a card on screen for a suspension the log never got; answering it was a 409.
                let Ok(suspended) = run.decide(RunCommand::Suspend {
                    call_id,
                    tool: event
                        .extra
                        .get("tool")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    arguments: event
                        .extra
                        .get("arguments")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                    // Absent on rows written before reasons existed ⇒ exec-consent, which is
                    // what every such suspension meant.
                    reason: opengrok_core::run::SuspendReason::from_stored(
                        event
                            .extra
                            .get("reason")
                            .and_then(|value| value.as_str())
                            .unwrap_or_default(),
                    ),
                    at_ms,
                }) else {
                    return Err(AppendError::Ended);
                };
                for suspended_event in &suspended {
                    run.apply(suspended_event);
                }
                to_append.extend(suspended);
            }
        }

        // The run's own ending, recorded once, from the event that carries it.
        //
        // HITL park emits `RUN_FINISHED` after CUSTOM `run-awaiting-approval` so NativeChat
        // Waiting chrome can treat the HTTP turn as not running. That closer is a stream
        // fact, not an aggregate ending: a suspended run must stay `awaiting-approval` so
        // Continue can resume. Skip Finish while we are waiting; a later resume answers
        // first (status Running) and then a real `RUN_FINISHED` Finishes.
        let closing = match event.event_type {
            opengrok_wire::agui::EventType::RunFinished
                if run.status != RunStatus::AwaitingApproval =>
            {
                Some(run.decide(RunCommand::Finish { at_ms }))
            }
            opengrok_wire::agui::EventType::RunFinished => None,
            opengrok_wire::agui::EventType::RunError => Some(
                run.decide(RunCommand::Fail {
                    reason: event
                        .extra
                        .get("message")
                        .and_then(|message| message.as_str())
                        .unwrap_or("the run failed")
                        .to_string(),
                    at_ms,
                }),
            ),
            _ => None,
        };
        if let Some(Ok(closing)) = closing {
            for closing_event in &closing {
                run.apply(closing_event);
            }
            to_append.extend(closing);
        }
    }

    let view = RunView {
        id: run_id.clone(),
        thread_id: thread_id.to_string(),
        status: run.status,
        event_count: run.emitted.len() as i64,
        updated_at_ms: at_ms,
    };
    state
        .auth
        .store
        .append_run(&run_id, seq, &to_append, &view, account_id)
        .await?;
    Ok(())
}

/// A `run-awaiting-approval` CUSTOM: the frame whose suspension makes a card answerable.
fn is_suspend_frame(event: &Event) -> bool {
    event.event_type == opengrok_wire::agui::EventType::Custom
        && crate::agui::resume::is_suspend_custom(
            event.extra.get("name").and_then(|name| name.as_str()),
        )
}

/// What a POST whose run id already has a run gets, or `None` while the id is free.
///
/// ITS OWNER GETS THE RUN BACK: what it has recorded, then what it records until it ends or
/// parks — the answer the retry was waiting for, without a second loop making a second one.
/// ANYBODY ELSE IS REFUSED. A run id is a password (see `replay_run`), and a POST used to be the
/// way around it: a stranger's turn appended to the run and, through `append_run`, became its
/// owner. An anonymous caller owns nothing, so it is refused too.
async fn answer_for_existing_run(
    state: &AgUiState,
    account: Option<&AccountId>,
    run_id: &str,
) -> Option<Response> {
    let id = RunId::from_stored(run_id.to_string());
    let run = match state.auth.store.load_run(&id).await {
        Ok((run, _)) => run,
        Err(error) => {
            return Some((StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response());
        }
    };
    if !run.started {
        return None;
    }
    let Some(account) = account else {
        return Some(run_taken());
    };
    let owned = state.auth.store.run_owned_by(&id, account).await;
    Some(match owned {
        Ok(true) => attach(state.clone(), account.clone(), id),
        other => not_attached(other),
    })
}

/// A POST that may not attach: someone else's run, or an owner the store could not confirm.
///
/// A READ THAT FAILS IS A 503, NEVER `run-exists`. The 409 tells its caller to use a new run id,
/// and the owner retrying a dropped stream obeys it: a second run with the same words, every
/// model call and tool twice — what the existing-run check exists to prevent.
fn not_attached(owned: Result<bool, opengrok_store::StoreError>) -> Response {
    match owned {
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
        Ok(_) => run_taken(),
    }
}

fn run_taken() -> Response {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({
            "error": "run-exists",
            "message": "this run id already has a run; a new turn needs a new run id",
        })),
    )
        .into_response()
}

/// How often an attached stream looks for what its run has recorded since. The turn journals a
/// round at a time, so the stream moves a round at a time, and a second is plenty. Each look is
/// one primary-key read; the log itself is read only when that says something changed.
const ATTACH_POLL: std::time::Duration = std::time::Duration::from_secs(1);

/// Stream a run that another request is running or ran: what it has recorded, then what it
/// records, until it ends or parks.
///
/// ONLY THE LOG'S OWN FRAMES, ONE FOR ONE. Replay hydration also appends transcript cards it
/// could not match, and for a run with no frames yet it matches none, so every card the
/// coworker ever showed this person would arrive here — and the count used to resume the
/// stream would then skip that many real frames on the next look.
///
/// ONE CLOSER, THE ONE THE RUN'S STATUS SAYS (`attached_closer`). Closers inside the log are
/// held back: a parked and resumed run has the park's `RUN_FINISHED` in the middle, and a
/// parked run that was stopped still ends its log with it.
fn attach(state: AgUiState, account_id: AccountId, run_id: RunId) -> Response {
    use opengrok_wire::agui::EventType;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    tokio::spawn(async move {
        let unreadable = |thread_id: &str| {
            Event::new(EventType::RunError, now_ms())
                .with("threadId", thread_id.to_string())
                .with("runId", run_id.as_str())
                .with(
                    "message",
                    "this run could not be read back; ask for it again",
                )
        };
        let mut sent = 0usize;
        let mut seen = None;
        let mut stop_notice = false;
        let mut last: Option<Event> = None;
        loop {
            // A client that hung up stops the follow; a long tool would otherwise keep it
            // reading the log for nobody until the run ended.
            if tx.is_closed() {
                return;
            }
            let progress = match state.auth.store.run_progress(&run_id).await {
                Ok(progress) => progress,
                Err(_) => {
                    let _ = tx.send(unreadable(""));
                    return;
                }
            };
            if progress == seen && matches!(progress, Some((RunStatus::Running, _))) {
                tokio::time::sleep(ATTACH_POLL).await;
                continue;
            }
            seen = progress;
            let Ok((run, _)) = state.auth.store.load_run(&run_id).await else {
                let _ = tx.send(unreadable(""));
                return;
            };
            let (started_at_ms, updated_at_ms) = run_time_window(&run.emitted);
            let frames = log_frames(
                events_for_client(&state, &account_id, &run, started_at_ms, updated_at_ms).await,
                run.emitted.len(),
            );
            for frame in frames.into_iter().skip(sent) {
                sent += 1;
                let Ok(event) = serde_json::from_value::<Event>(frame) else {
                    continue;
                };
                let is_end = matches!(
                    event.event_type,
                    EventType::RunFinished | EventType::RunError
                );
                if !is_end {
                    stop_notice |= event.event_type == EventType::Custom
                        && event.extra.get("name").and_then(|name| name.as_str())
                            == Some("run-stopped");
                    if tx.send(event.clone()).is_err() {
                        return;
                    }
                }
                last = Some(event);
            }
            if run.status == RunStatus::Running {
                tokio::time::sleep(ATTACH_POLL).await;
                continue;
            }
            for event in attached_closer(&run, &run_id, last.as_ref(), stop_notice, now_ms()) {
                if tx.send(event).is_err() {
                    return;
                }
            }
            return;
        }
    });
    sse(futures::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|event| (Ok::<_, std::io::Error>(event), rx))
    }))
}

/// A replay's frames that are the log's own: hydration overlays cards onto them in place and
/// appends the cards it could not place, which an attached stream must not send (see `attach`).
fn log_frames(mut hydrated: Vec<serde_json::Value>, logged: usize) -> Vec<serde_json::Value> {
    hydrated.truncate(logged);
    hydrated
}

/// The closer an attached stream ends with, from what the run IS rather than from the last closer
/// its log happens to hold. `last` is the log's last frame and `stop_notice` whether its
/// `run-stopped` was already sent.
fn attached_closer(
    run: &opengrok_core::run::Run,
    run_id: &RunId,
    last: Option<&Event>,
    stop_notice: bool,
    at_ms: i64,
) -> Vec<Event> {
    use opengrok_wire::agui::EventType;
    let closing = |event_type| {
        Event::new(event_type, at_ms)
            .with("threadId", run.thread_id.clone())
            .with("runId", run_id.as_str())
    };
    let last_is = |event_type| last.filter(|event| event.event_type == event_type).cloned();
    match run.status {
        RunStatus::Failed => vec![last_is(EventType::RunError).unwrap_or_else(|| {
            closing(EventType::RunError).with(
                "message",
                run.failure
                    .clone()
                    .unwrap_or_else(|| "the run failed".to_string()),
            )
        })],
        RunStatus::Stopped => {
            let mut ending = Vec::new();
            if !stop_notice {
                ending.push(closing(EventType::Custom).with("name", "run-stopped"));
            }
            ending.push(match (stop_notice, last_is(EventType::RunFinished)) {
                (true, Some(finished)) => finished,
                _ => closing(EventType::RunFinished),
            });
            ending
        }
        // Finished, or parked on a card: the log's own closer when it has one.
        _ => {
            vec![last_is(EventType::RunFinished).unwrap_or_else(|| closing(EventType::RunFinished))]
        }
    }
}

/// Replay a run from the log.
///
/// THIS IS THE PROMISE, MADE CHECKABLE. Close the tab mid-run, come back, ask here: every event
/// the run produced is returned, in order, without asking a model anything a second time.
pub async fn replay_run(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(run_id): Path<String>,
) -> Response {
    let run_id = RunId::from_stored(run_id);

    // LAYER 4 (`docs/PLAN.md` §4.5): a run holds a whole conversation, so without this check a run
    // id is a password — and run ids travel in client URLs and logs. `NOT_FOUND` rather than
    // `FORBIDDEN` for both "no such run" and "not yours", so probing ids reveals nothing about
    // which runs exist.
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    };
    match state.auth.store.run_owned_by(&run_id, &account_id).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::NOT_FOUND, "no such run").into_response(),
        Err(error) => {
            return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
        }
    }

    let (run, _) = match state.auth.store.load_run(&run_id).await {
        Ok(loaded) => loaded,
        Err(error) => {
            return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
        }
    };

    if !run.started {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }

    let (started_at_ms, updated_at_ms) = run_time_window(&run.emitted);
    let events = events_for_client(&state, &account_id, &run, started_at_ms, updated_at_ms).await;

    Json(serde_json::json!({
        "runId": run_id.as_str(),
        "threadId": run.thread_id,
        "status": run.status.as_str(),
        // When the turn began. A client that picks a run up after a restart has no bubble for
        // it and has to make one; without this it would stamp that bubble with the moment it
        // noticed, and the turn would sort to the wrong place in the thread for good.
        "startedAtMs": started_at_ms,
        "failure": run.failure,
        "pending": run.pending,
        "events": events,
    }))
    .into_response()
}

fn run_time_window(emitted: &[serde_json::Value]) -> (i64, i64) {
    let times: Vec<i64> = emitted
        .iter()
        .filter_map(|event| event.get("timestamp").and_then(serde_json::Value::as_i64))
        .collect();
    match (times.first(), times.last()) {
        (Some(&first), Some(&last)) => (first, last),
        _ => (0, i64::MAX),
    }
}

async fn events_for_client(
    state: &AgUiState,
    account_id: &AccountId,
    run: &opengrok_core::run::Run,
    started_at_ms: i64,
    updated_at_ms: i64,
) -> Vec<serde_json::Value> {
    let forms = match run.coworker_id.as_ref() {
        Some(coworker_id) => state
            .auth
            .store
            .gateway_transcript(coworker_id, account_id)
            .await
            .unwrap_or_default(),
        None => Vec::new(),
    };
    crate::agui::user_form::hydrate_agui_events(
        run.emitted.clone(),
        &forms,
        started_at_ms,
        updated_at_ms,
    )
}

/// How many runs a thread answers with when the caller does not ask for a number.
///
/// Twenty, which is what `gateway/lifecycle.rs` already asks `runs_for_thread` for when it builds
/// a routine's run list. Two readers of the same history disagreeing about how much of it is
/// "recent" gets reported as "the app shows fewer turns than the pane does", and the cheapest way
/// not to have that conversation is to pick the number once. Twenty turns is also more than a
/// screenful, which is what a client reopening a conversation actually has to draw.
const THREAD_RUNS_DEFAULT: i64 = 20;

/// The most runs one request may ask for.
///
/// A run carries EVERY frame it emitted, and a streamed answer is hundreds of text deltas, so the
/// response grows with the size of the conversation and not with the number of runs in it — an
/// uncapped `?limit=` is a way to ask this server to send megabytes. A hundred turns bounds that
/// at something a client can still render, and a client that wants a deeper history than this is
/// not drawing a conversation: `?events=false` is the cheap way to ask which runs exist, and
/// `GET /ag-ui/runs/{id}` fetches the few whose frames are actually missing.
const THREAD_RUNS_MAX: i64 = 100;

#[derive(Debug, Deserialize)]
pub struct ThreadHistoryQuery {
    /// How many runs, counted from the newest end and clamped to `THREAD_RUNS_MAX`. Counting from
    /// the newest end is what makes a limit useful on a long thread: the turns a person is coming
    /// back to are the last ones, and they are still handed back oldest-first.
    pub limit: Option<i64>,
    /// `false` keeps every run and drops its frames, for a client that only wants to know which
    /// runs exist — the desktop working out which transcripts it is missing before it fetches
    /// them one at a time. Defaults to true, because the whole point of this route is the frames.
    pub events: Option<bool>,
}

/// One run as a thread's history lists it. `events` is the only optional part: everything else
/// costs a handful of bytes and a client that has to branch on which fields arrived is a client
/// that will get the branch wrong.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ThreadRunReplay {
    run_id: String,
    status: &'static str,
    started_at_ms: i64,
    updated_at_ms: i64,
    failure: Option<String>,
    /// ABSENT, not empty, when `?events=false` asked for the list without the bodies. An empty
    /// array would say this run emitted nothing, which is a different claim and a false one.
    #[serde(skip_serializing_if = "Option::is_none")]
    events: Option<Vec<serde_json::Value>>,
}

/// Replay a whole thread from the log — `GET /ag-ui/threads/{thread_id}?limit=&events=`.
///
/// THE SAME PROMISE AS `replay_run`, ASKED THE WAY A PERSON REMEMBERS THINGS. A reconnecting
/// stream knows its run id and asks for that one run; somebody who closed the app, or switched to
/// another coworker and came back, knows only who they were talking to. Without a way to ask for
/// the conversation, a client has to keep its own copy of the transcript and trust it over ours —
/// and a local copy that is authoritative is exactly the arrangement that loses the messages a
/// turn produced after the app stopped watching.
///
/// HALF A CONVERSATION, AND SAYING SO IS THE POINT. A run's log holds the events it EMITTED, which
/// is the coworker's side of the turn: its text, its tool calls, their results. The person's own
/// message arrives in `RunAgentInput.messages`, is spent on the model call and is never journaled
/// — `RunEvent::Started` captures the thread, the coworker, the pin and the system message, and
/// nothing about what was asked. So a client rendering a transcript from this has to interleave
/// the person's side from somewhere else: the seam-B entries (`seamb_send.rs`) for a turn that
/// came through the gateway's send, and its own records for a turn that came through `POST /ag-ui`
/// directly, where the server keeps no copy of the question at all. Closing that means journaling
/// the turn's own prompt on the run, which changes the aggregate and belongs to its own change.
pub async fn replay_thread(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(thread_id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<ThreadHistoryQuery>,
) -> Response {
    // LAYER 4 (`docs/PLAN.md` §4.5), and the reasoning is `replay_run`'s: a thread holds a whole
    // conversation — more of one than a run does — so without this check a thread id is a
    // password, and thread ids travel in client URLs and logs. `NOT_FOUND` rather than `FORBIDDEN`
    // for "no such thread", "not yours" and "not signed in" alike, so probing ids reveals nothing
    // about which threads exist or who they belong to.
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::NOT_FOUND, "no such thread").into_response();
    };
    let limit = query
        .limit
        .unwrap_or(THREAD_RUNS_DEFAULT)
        .clamp(1, THREAD_RUNS_MAX);
    let with_events = query.events.unwrap_or(true);

    // Owner-filtered in the query, so "not yours" and "no such thread" arrive here as the same
    // empty answer and cannot be told apart even by accident.
    let newest_first = match state
        .auth
        .store
        .runs_for_thread_owned_by(&thread_id, &account_id, limit)
        .await
    {
        Ok(runs) => runs,
        Err(error) => {
            return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
        }
    };
    // The turns this account hid, named rather than silently missing: a client keeps its own
    // copy of a thread, and a name it is not told stays on its screen. Asked before the empty
    // check, because a thread whose every turn was hidden is exactly the one whose client most
    // needs to hear which names to put away — and it is also the answer that would otherwise
    // read as "no such thread" and leave that client painting from its cache for good.
    let hidden = match state
        .auth
        .store
        .hidden_runs_in_thread(&thread_id, &account_id)
        .await
    {
        Ok(hidden) => hidden,
        Err(error) => {
            return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
        }
    };
    let pending =
        match crate::agui::pending::thread_pending_json(&state.auth.store, &thread_id, &account_id)
            .await
        {
            Ok(pending) => pending,
            Err(error) => {
                return (StatusCode::SERVICE_UNAVAILABLE, error).into_response();
            }
        };
    if newest_first.is_empty() && hidden.is_empty() {
        return (StatusCode::NOT_FOUND, "no such thread").into_response();
    }

    let mut runs = Vec::with_capacity(newest_first.len());
    let mut forms_by_coworker: std::collections::HashMap<CoworkerId, Vec<serde_json::Value>> =
        std::collections::HashMap::new();
    // OLDEST FIRST, which is the other way round from the store. `runs_for_thread_owned_by` hands
    // back the newest runs because that is how a limit has to be counted on a long thread; a
    // transcript is read in the order it happened. Reversing once here, rather than leaving it to
    // each caller, is what stops two clients from disagreeing about which end a conversation
    // starts at — and a disagreement like that shows up as messages in the wrong order, which
    // reads as lost work.
    for summary in newest_first.into_iter().rev() {
        let (run, _) = match state.auth.store.load_run(&summary.id).await {
            Ok(loaded) => loaded,
            Err(error) => {
                return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
            }
        };
        // A run the log has no start for never took its turn, so it has nothing to contribute to a
        // transcript. `replay_run` answers `404` when one is asked about by id; a history simply
        // leaves it out, because the question here is what happened and nothing did.
        if !run.started {
            continue;
        }
        let events = if with_events {
            let forms = match run.coworker_id.as_ref() {
                Some(coworker_id) => {
                    if let Some(cached) = forms_by_coworker.get(coworker_id) {
                        cached.clone()
                    } else {
                        let loaded = state
                            .auth
                            .store
                            .gateway_transcript(coworker_id, &account_id)
                            .await
                            .unwrap_or_default();
                        forms_by_coworker.insert(coworker_id.clone(), loaded.clone());
                        loaded
                    }
                }
                None => Vec::new(),
            };
            Some(crate::agui::user_form::hydrate_agui_events(
                run.emitted,
                &forms,
                summary.started_at_ms,
                summary.updated_at_ms,
            ))
        } else {
            None
        };
        runs.push(ThreadRunReplay {
            run_id: summary.id.as_str().to_string(),
            // The aggregate's status, not the projection's, for the same reason `replay_run` uses
            // it: the log is the truth and the view is derived from it.
            status: run.status.as_str(),
            started_at_ms: summary.started_at_ms,
            updated_at_ms: summary.updated_at_ms,
            failure: run.failure,
            // The frames are loaded either way: whether a run started and why it failed are only
            // knowable from its log, and answering those two from the projection would mean
            // guessing. `events=false` saves the client the megabytes, not the server the read.
            events,
        });
    }

    let mut body = serde_json::json!({
        "threadId": thread_id,
        "runs": runs,
        "hiddenRunIds": hidden,
    });
    // Sibling of `runs`, never mixed into a run's frames: a queued send is not a turn yet, and
    // dropping it into `events` would make NativeChat paint a bubble as if the coworker had
    // already seen it. `pendingEvents` are CUSTOM `pending-user-message` snapshots for clients
    // that already walk CUSTOM; `pendingUserMessages` is the array to replace a local queue with.
    if let Some(object) = body.as_object_mut()
        && let Some(pending) = pending.as_object()
    {
        for (key, value) in pending {
            object.insert(key.clone(), value.clone());
        }
    }
    Json(body).into_response()
}

/// Hide a turn from every client of the account that owns it.
///
/// The person deleting a turn in their app is not asking for it to be destroyed. They are asking
/// not to be shown it again — and on the next machine they sign in from, not to be shown it
/// there either. So nothing here is removed: the run keeps its frames, and the coworker keeps
/// its memory of the turn. What changes is that a thread stops offering the run, so no client
/// paints it and the one that hid it does not fetch it back.
///
/// LAYER 4 (`docs/PLAN.md` §4.5): the account comes from the bearer and another account's run is
/// "no such run", the same answer as one that never existed.
pub async fn hide_run(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(run_id): Path<String>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    };
    match state
        .auth
        .store
        .hide_run(&RunId::from_stored(run_id), &account_id, now_ms())
        .await
    {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "no such run").into_response(),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct AnswerRequest {
    /// Which call is being answered. Required: "approve the run" is ambiguous the moment a turn
    /// asks for two things.
    pub call_id: String,
    pub approved: bool,
}

/// Answer a suspended run — PLAN §4.5 layer 5, the other half.
///
/// EXACTLY ONCE, AND THE AGGREGATE IS WHAT GUARANTEES IT. A retried request, a double-clicked
/// button and two devices answering together all reach here; the aggregate refuses every answer
/// after the first, and the store's sequence check makes the concurrent case safe — the loser gets
/// a conflict and re-reads to find the call already answered.
pub async fn answer_run(
    State(host): State<crate::host_state::HostState>,
    headers: axum::http::HeaderMap,
    Path(run_id): Path<String>,
    Json(request): Json<AnswerRequest>,
) -> Response {
    let state = host.agui.clone();
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let run_id = RunId::from_stored(run_id);

    // Only the run's owner may answer it — the same rule as replay, for the same reason.
    match state.auth.store.run_owned_by(&run_id, &account_id).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::NOT_FOUND, "no such run").into_response(),
        Err(error) => {
            return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
        }
    }

    let (mut run, seq) = match state.auth.store.load_run(&run_id).await {
        Ok(loaded) => loaded,
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };

    // Captured BEFORE the answer, because answering clears it — and the continuation needs to know
    // exactly which command was approved rather than asking the model to propose one again.
    let pending = run.pending.clone();
    let resumed_seq = run.emitted.len() as u32;

    let at_ms = now_ms();
    let answer = RunCommand::Answer {
        call_id: request.call_id.clone(),
        approved: request.approved,
        by: account_id.to_string(),
        at_ms,
    };

    // AN MCP AUDIT RUN IS NOT A CONVERSATION, so it does not carry on: answering its card finishes
    // it, and a yes is remembered for the MCP client's own retry instead. Resuming it would run
    // the tool here while the client is being told to retry, and the retry would run it again.
    //
    // THE WHOLE SETTLE GOES TO THE BACKGROUND, Answer and all. It has to hold the door's
    // per-coworker lock, and `dispatch` holds that lock across a real tool call — minutes, on a
    // slow box — so doing it here would park somebody's approve on a request that cannot finish.
    // Journalling the Answer inside that lock is also stricter than doing it here would be: it
    // closes the window where a `tools/call` arriving between the append and the lock finds no
    // pending ask, runs the call and raises a second card.
    //
    // The aggregate still decides the answer synchronously first, so a retried press still gets
    // `alreadyAnswered` and a wrong call id still gets a 409 — `decide` is pure, so this costs
    // nothing and settles nothing.
    if crate::mcp_door::is_mcp_audit_run(&run)
        && let Some(pending) = pending.clone()
    {
        if let Err(error) = run.decide(answer) {
            return match error {
                opengrok_core::run::RunError::AlreadyAnswered => (
                    StatusCode::OK,
                    Json(serde_json::json!({
                        "runId": run_id.as_str(),
                        "callId": request.call_id,
                        "alreadyAnswered": true,
                    })),
                )
                    .into_response(),
                error => (StatusCode::CONFLICT, error.to_string()).into_response(),
            };
        }
        let store = state.auth.store.clone();
        let settling = crate::mcp_door::McpCardAnswer {
            run_id: run_id.clone(),
            run,
            seq,
            pending,
            approved: request.approved,
            at_ms,
        };
        let account = account_id.clone();
        tokio::spawn(async move {
            crate::mcp_door::settle_mcp_answer(store, account, settling).await;
        });
        return Json(serde_json::json!({
            "runId": run_id.as_str(),
            "callId": request.call_id,
            "approved": request.approved,
            "alreadyAnswered": false,
            // Nothing follows on this run — the MCP client's retry is what happens next.
            "continuing": false,
            // And the ending is on its way rather than already done: a client that needs to know
            // reads the run back until it is `finished`.
            "settling": true,
        }))
        .into_response();
    }

    let events = match run.decide(answer) {
        Ok(events) => events,
        // A second answer is not an error the caller needs to fix; it is the same answer arriving
        // twice. Reporting the settled state is what makes a retry safe to send.
        Err(opengrok_core::run::RunError::AlreadyAnswered) => {
            return (
                StatusCode::OK,
                Json(serde_json::json!({
                    "runId": run_id.as_str(),
                    "callId": request.call_id,
                    "alreadyAnswered": true,
                })),
            )
                .into_response();
        }
        Err(error) => return (StatusCode::CONFLICT, error.to_string()).into_response(),
    };

    for event in &events {
        run.apply(event);
    }

    let view = RunView {
        id: run_id.clone(),
        thread_id: run.thread_id.clone(),
        status: run.status,
        event_count: run.emitted.len() as i64,
        updated_at_ms: at_ms,
    };
    if let Err(error) = state
        .auth
        .store
        .append_run(&run_id, seq, &events, &view, Some(&account_id))
        .await
    {
        // A conflict here means somebody answered between our read and our write. The answer that
        // won is as good as ours, so this is not a failure to report as one.
        if matches!(error, opengrok_store::StoreError::Conflict) {
            return (
                StatusCode::OK,
                Json(serde_json::json!({
                    "runId": run_id.as_str(),
                    "callId": request.call_id,
                    "alreadyAnswered": true,
                })),
            )
                .into_response();
        }
        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
    }

    // The answer is durable; now carry the run on. In the background, because a model call can
    // take minutes and the person clicking "approve" should not hold a socket open for it.
    //
    // A NO CARRIES ON TOO, AND THAT IS THE WHOLE POINT OF THIS BRANCH. `Answered` puts the run
    // back to `Running` whether the answer was yes or no, because the refusal still has to reach
    // the model so it can say something else — the aggregate says so where it applies the event.
    // This route used to resume only on yes, so a denied run was left `Running` with nobody
    // advancing it: the person saw a stop button over a turn nothing was doing, the model was
    // never told it had been refused, and the recovery sweep eventually claimed the run as one a
    // restart had abandoned and ended it with "we do not know whether it ran" — which is a lie
    // about a refusal somebody made on purpose. Worse, the tool call was left with no result at
    // all, so the next turn in that thread replayed a call nothing answered.
    //
    // The suspended-run resume path has done this from the start (`agui::resume`); the
    // two doors on to the same run disagreed, and this is the one that was wrong.
    let continuing = pending.is_some();
    if let Some(pending) = pending {
        let outcome = resume_outcome(request.approved, &pending);
        let host = host.clone();
        let account_id = account_id.clone();
        let run_id = run_id.clone();
        tokio::spawn(async move {
            continue_run(host, account_id, run_id, pending, resumed_seq, outcome).await;
        });
    }

    Json(serde_json::json!({
        "runId": run_id.as_str(),
        "callId": request.call_id,
        "approved": request.approved,
        "alreadyAnswered": false,
        // Whether anything is still going to happen on this run — which is what a client uses to
        // decide whether to keep watching. A no is still something happening: the model is being
        // told, and it answers. The one case where nothing follows is an answer that arrived with
        // no pending call to answer.
        "continuing": continuing,
    }))
    .into_response()
}

/// What the model is told about the card the person just answered.
///
/// A refusal names what was refused rather than saying "declined" alone, so the model can choose
/// something else instead of proposing the same call again, and so the transcript says why. The
/// words follow the gateway's, which has been saying them to the other door's runs all along.
fn resume_outcome(
    approved: bool,
    pending: &opengrok_core::run::PendingApproval,
) -> opengrok_harness::ResumeOutcome {
    match pending.reason {
        // Re-running `request_user_form` would raise the card again. Submit/dismiss synthesise
        // the tool result; `/answer` is the fallback and must do the same.
        opengrok_core::run::SuspendReason::UserForm => {
            opengrok_harness::ResumeOutcome::Settled(if approved {
                "The person submitted the form. It was filled into the page. Secret field values were typed into the page and never shown to you.".to_string()
            } else {
                "The person dismissed the form without filling anything. Continue without those credentials; do not type secrets with `computer`.".to_string()
            })
        }
        _ if approved => opengrok_harness::ResumeOutcome::Approved,
        opengrok_core::run::SuspendReason::AutoReview => opengrok_harness::ResumeOutcome::Refused(
            "the user declined this on the auto-review card".to_string(),
        ),
        opengrok_core::run::SuspendReason::PolicyApproval => {
            opengrok_harness::ResumeOutcome::Refused(format!(
                "the user declined this on the approval card: the coworker's policy needs a \
                 person's yes before it may run {}",
                pending.tool
            ))
        }
        // Named rather than caught by a wildcard, so a fifth kind of card has to decide here
        // what its refusal says instead of quietly borrowing this one's words.
        opengrok_core::run::SuspendReason::ExecConsent => {
            opengrok_harness::ResumeOutcome::Refused(format!(
                "the user declined this on the approval card, so {} did not run. do not ask \
                 for it again in this turn; say what you can do without it, or ask what to do \
                 instead",
                pending.tool
            ))
        }
    }
}

/// How many times a stop will re-read and try again when somebody else wrote first.
///
/// The thing most likely to collide with this write is the turn itself, journaling the round it is
/// in the middle of — which is exactly the moment a person presses stop. Losing that race once is
/// ordinary; reporting a failure because of it would leave a run going after its owner was told it
/// was not.
const STOP_ATTEMPTS: usize = 5;

/// Stop a run — `POST /ag-ui/runs/{run_id}/stop`.
///
/// THE ONLY WAY OUT OF A LOOP, AND UNTIL THIS EXISTED THERE WAS NONE. A taught recipe ran, and the
/// bot ran it again, and again, opening the browser and typing the search term each time; the
/// person typed "stop it" into the chat, which reached nothing, because that is one more message to
/// a model that is mid-turn. A stop has to be a command against the run, not a sentence to the
/// coworker.
///
/// WHAT IT DOES: records `Stopped` in the run's log, which is the one place every reader of this
/// run already looks. The turn asks the log at each step boundary and ends when it sees it; the
/// recovery sweep skips a run that is not `running`; `replay_run` reports the status and keeps
/// every frame from before the stop. Nothing here reaches into a task, so a stop works the same
/// whether the turn is in this process, in another replica, or in a process that has since died.
///
/// WHAT IT DOES NOT DO, AND THE ANSWER SAYS SO: it does not take back a step already under way. See
/// `stopped_answer`.
///
/// WHAT IT COSTS. A turn that is stopped keeps whatever it has already spent, and that is the
/// correct outcome rather than an oversight: the model calls really happened and the gateway
/// metered each one against the coworker's own key as it completed. Nothing here aborts a task or
/// drops a response mid-stream, and that is deliberate — a half-read model stream would leave
/// tokens spent at the provider and absent from the meter, which is the only way a stop could
/// actually lose money. The frames that spend bought are journaled too, including the round the
/// turn was in the middle of, so `replay_run` still shows what was paid for.
pub async fn stop_run(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(run_id): Path<String>,
) -> Response {
    let run_id = RunId::from_stored(run_id);

    // `replay_run`'s check, byte for byte, and for the same reason: a run holds a whole
    // conversation, so without it a run id is a password — and run ids travel in client URLs and
    // logs. `NOT_FOUND` rather than `FORBIDDEN` for "no such run", "not yours" and "not signed in"
    // alike, so probing ids reveals nothing about which runs exist. The two answers have to be
    // indistinguishable down to the bytes, which is why this says the same words.
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    };
    match state.auth.store.run_owned_by(&run_id, &account_id).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::NOT_FOUND, "no such run").into_response(),
        Err(error) => {
            return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
        }
    }

    for _ in 0..STOP_ATTEMPTS {
        let (mut run, seq) = match state.auth.store.load_run(&run_id).await {
            Ok(loaded) => loaded,
            Err(error) => {
                return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
            }
        };
        // A run the log has no start for never took a turn, so there is nothing to stop and
        // nothing to say about it — the same answer `replay_run` gives, for the same reason.
        if !run.started {
            return (StatusCode::NOT_FOUND, "no such run").into_response();
        }

        // Read BEFORE the stop is applied: what the run was doing is what decides how honest the
        // answer can be about when the stop takes hold.
        let was = run.status;

        let at_ms = now_ms();
        let events = match run.decide(RunCommand::Stop {
            by: account_id.to_string(),
            at_ms,
        }) {
            Ok(events) => events,
            // The only refusal `Stop` produces is `AlreadyEnded`, and it is a SUCCESS. The person
            // pressed a button asking for this run not to be running; whether they won the race
            // with the model is not their problem, and an error here would make a retry — a second
            // press, a client resending — look like a fault.
            Err(_) => return stopped_answer(&run_id, was),
        };

        for event in &events {
            run.apply(event);
        }
        let view = RunView {
            id: run_id.clone(),
            thread_id: run.thread_id.clone(),
            status: run.status,
            event_count: run.emitted.len() as i64,
            updated_at_ms: at_ms,
        };
        match state
            .auth
            .store
            .append_run(&run_id, seq, &events, &view, Some(&account_id))
            .await
        {
            Ok(_) => {
                tracing::info!(run = %run_id, by = %account_id, "a run was stopped");
                return stopped_answer(&run_id, was);
            }
            // Somebody wrote to this run between the read and the write. Re-read and decide again
            // against what is actually there: either the run is now ended, and the next pass
            // answers with that, or the turn simply journaled a round and this stop still stands.
            Err(opengrok_store::StoreError::Conflict) => continue,
            Err(error) => {
                return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
            }
        }
    }

    (
        StatusCode::SERVICE_UNAVAILABLE,
        "that run is being written to faster than it can be stopped; try again",
    )
        .into_response()
}

/// The answer to a stop, and how honest it can be about when the stop takes hold.
///
/// `202`, NOT `200`, AND THE DIFFERENCE IS THE POINT. The stop is durable by the time this is
/// written — nothing will start again — but a step already under way is not taken back, and
/// pretending otherwise would promise an instant stop that does not exist. `takesEffect` says which
/// of the three cases this was, and `note` is the sentence a client can put in front of a person
/// instead of inventing its own.
///
/// `status` is always `stopped`, including for a run that had already finished, because it is the
/// outcome of the REQUEST and not the run's own status — the run's status is unchanged and
/// `GET /ag-ui/runs/{id}` still reports it. `takesEffect: already-ended` is what distinguishes the
/// two for a client that cares.
fn stopped_answer(run_id: &RunId, was: RunStatus) -> Response {
    let (takes_effect, note) = match was {
        // Nothing was running, and saying "stopped" is still the right answer: the person asked
        // for this run not to be running, and it is not.
        RunStatus::Finished | RunStatus::Failed | RunStatus::Stopped => (
            "already-ended",
            "That run had already ended, so there was nothing left to stop.",
        ),
        // Waiting on a person is not working. No model call is in flight and no tool is running,
        // so the run is over the moment the log says so.
        RunStatus::AwaitingApproval => (
            "immediately",
            "That run was waiting on an approval, and the card it was waiting on is closed.",
        ),
        // THE HONEST ONE. The turn asks the log whether it has been stopped between steps: before
        // each model call, and again after the model has answered and before its tools are run. A
        // call already in flight is not reached from there — a taught recipe playing on the box is
        // a single request that takes many seconds, and the box's API on the revision this server
        // is pinned to (`grok-box` rev 2dea0d4: `POST /v1/exec`, `GET /v1/exec/{id}`, no delete)
        // offers no way to take one back. So the step in progress finishes and nothing after it
        // begins.
        RunStatus::Running => (
            "next-step",
            "That run is stopped. A step already under way — a model call, or a recipe playing on \
             the computer — finishes first; nothing after it will be started.",
        ),
    };
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "runId": run_id.as_str(),
            "status": "stopped",
            "takesEffect": takes_effect,
            "note": note,
        })),
    )
        .into_response()
}

/// Rebuild the conversation from what a run already emitted.
///
/// The log is the only record of a run that outlives the request that started it, so a resumed run
/// has to read its own history rather than being handed one. Text the assistant said and results
/// its tools returned are what the model needs to carry on; the framing events are not.
pub(crate) fn conversation_from(run: &opengrok_core::run::Run) -> Vec<ChatMessage> {
    let mut messages = Vec::new();
    let mut assistant = String::new();

    for payload in &run.emitted {
        let Some(kind) = payload.get("type").and_then(|value| value.as_str()) else {
            continue;
        };
        match kind {
            "TEXT_MESSAGE_CONTENT" => {
                if let Some(delta) = payload.get("delta").and_then(|value| value.as_str()) {
                    assistant.push_str(delta);
                }
            }
            // An empty message is skipped rather than pushed: a provider that rejects empty
            // content would fail the whole resumed turn over nothing.
            "TEXT_MESSAGE_END" if !assistant.is_empty() => {
                messages.push(ChatMessage {
                    images: Vec::new(),
                    role: "assistant".to_string(),
                    content: std::mem::take(&mut assistant),
                });
            }
            "TOOL_CALL_RESULT" => {
                if let Some(content) = payload.get("content").and_then(|value| value.as_str()) {
                    messages.push(ChatMessage {
                        images: Vec::new(),
                        role: "user".to_string(),
                        content: format!("[tool result] {content}"),
                    });
                }
            }
            _ => {}
        }
    }

    messages
}

/// How many tool results from a stopped turn are worth showing the next one.
/// One per model call, so a turn that hit the 8-call cap still shows every result.
const STEER_TOOL_CAP: usize = 8;
/// Bound one result so a shell dump cannot become the next prompt.
const STEER_TOOL_CHARS: usize = 800;

const STEER_CONTINUATION: &str = "[harness] The previous turn on this thread stopped or failed \
before it answered. Those tool results are that turn. Continue from them and from the person's \
latest message. Do not repeat a command that already returned ok.";

/// A stopped or failed turn's tools are the work a steer should continue.
/// A parked approval is a card the new message is declining, not a plan to resume.
/// A finished turn already put its answer in the chat bubbles.
pub(crate) fn prior_turn_can_continue(status: RunStatus, payloads: &[serde_json::Value]) -> bool {
    let parked = payloads.iter().any(|payload| {
        payload.get("name").and_then(serde_json::Value::as_str) == Some("run-awaiting-approval")
    });
    if parked {
        return false;
    }
    matches!(
        status,
        RunStatus::Stopped | RunStatus::Failed | RunStatus::Running
    )
}

fn clip_chars(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    format!("{head}…")
}

/// Tool calls a turn already made, in order, as messages the next turn can read.
/// The command rides with the result. A result alone does not say what was run.
pub(crate) fn unfinished_tool_messages(payloads: &[serde_json::Value]) -> Vec<ChatMessage> {
    let mut names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut arguments: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut out = Vec::new();
    for payload in payloads {
        let kind = payload
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let id = payload
            .get("toolCallId")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        match kind {
            "TOOL_CALL_START" => {
                if let Some(name) = payload
                    .get("toolCallName")
                    .and_then(serde_json::Value::as_str)
                {
                    names.insert(id, name.to_string());
                }
            }
            "TOOL_CALL_ARGS" => {
                if let Some(delta) = payload.get("delta").and_then(serde_json::Value::as_str) {
                    arguments.entry(id).or_default().push_str(delta);
                }
            }
            "TOOL_CALL_RESULT" => {
                let content = payload
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                if content.is_empty() {
                    continue;
                }
                let name = names.get(&id).map(String::as_str).unwrap_or("tool");
                let args = arguments.get(&id).map(String::as_str).unwrap_or("");
                out.push(ChatMessage {
                    images: Vec::new(),
                    role: "user".to_string(),
                    content: format!(
                        "[earlier {name} {}] {}",
                        clip_chars(args, 400),
                        clip_chars(content, STEER_TOOL_CHARS)
                    ),
                });
            }
            _ => {}
        }
    }
    if out.len() > STEER_TOOL_CAP {
        out = out.split_off(out.len() - STEER_TOOL_CAP);
    }
    out
}

/// Put the stopped turn's tools immediately before the person's new message.
pub(crate) fn splice_unfinished_tools(messages: &mut Vec<ChatMessage>, prior: Vec<ChatMessage>) {
    if prior.is_empty()
        || messages
            .iter()
            .any(|message| message.content.contains(STEER_CONTINUATION))
    {
        return;
    }
    let mut block = prior;
    block.push(ChatMessage {
        images: Vec::new(),
        role: "user".to_string(),
        content: STEER_CONTINUATION.to_string(),
    });
    let at = messages
        .iter()
        .rposition(|message| message.role == "user")
        .unwrap_or(messages.len());
    messages.splice(at..at, block);
}

/// Newest prior run on this thread, when it stopped or failed with tools and
/// was not parked on a card.
async fn continue_stopped_turn(
    state: &AgUiState,
    account: &AccountId,
    thread_id: &str,
    this_run_id: &str,
    messages: &mut Vec<ChatMessage>,
) {
    let Ok(runs) = state
        .auth
        .store
        .runs_for_thread_owned_by(thread_id, account, 4)
        .await
    else {
        return;
    };
    let Some(prior) = runs.into_iter().find(|run| run.id.as_str() != this_run_id) else {
        return;
    };
    let Ok((loaded, _)) = state.auth.store.load_run(&prior.id).await else {
        return;
    };
    if !prior_turn_can_continue(loaded.status, &loaded.emitted) {
        return;
    }
    let tools = unfinished_tool_messages(&loaded.emitted);
    splice_unfinished_tools(messages, tools);
}

/// Carry an answered run on, without waiting for anybody to ask again.
///
/// THE SERVER PICKS IT BACK UP. A run that only continues when the next request happens to arrive
/// is a run that depends on a client being there — which is the thing this project exists to stop
/// (CLAUDE.md #5). The answer is already durable when this starts, so a crash here leaves a run
/// that is answered and unfinished, which `interrupted_runs` can find and this can be told to do
/// again.
async fn continue_run(
    host: crate::host_state::HostState,
    account_id: opengrok_core::id::AccountId,
    run_id: RunId,
    answered: opengrok_core::run::PendingApproval,
    resumed_seq: u32,
    outcome: opengrok_harness::ResumeOutcome,
) {
    let state = host.agui.clone();
    // HOLD THE RUN WHILE IT IS CARRIED ON, exactly as the turn that parked it did. The answer
    // flips the run back to `running`; the parked turn's lease died with it, so without one
    // here an approved call that ran past LEASE_MS with nothing journaled — a recipe on the
    // box — was claimed by the sweep and failed as "interrupted by a restart" while it was
    // still running (`formal/tla/RunLifecycle.tla` NoFalseFailure: TLC's trace is park,
    // answer, sweep). Held before anything is loaded, so no early return runs unleased.
    let _lease = crate::recovery::Lease::new(crate::recovery::hold(state.clone(), run_id.clone()));
    let Ok((run, _)) = state.auth.store.load_run(&run_id).await else {
        tracing::warn!(run = %run_id, "could not load an answered run to continue it");
        return;
    };

    // The coworker whose tools these are. Without it there is nothing to continue *as*.
    let Some(coworker_id) = run.coworker_id.clone() else {
        tracing::warn!(run = %run_id, "an answered run has no coworker, so it cannot continue");
        return;
    };
    let Ok((coworker, _)) = state.auth.store.load_coworker(&coworker_id).await else {
        return;
    };

    // The answered call, and only it — carried on the SAME runner every other path builds
    // (plugins, the user's machine, auto-review). This path once built a bare executor of its
    // own and so resumed with no plugins and no review: a resumed call slipped every gate but the
    // grant's. Which yes it was decides which gate it releases.
    //
    // A refusal releases a gate it will not walk through, which is deliberate and is what the
    // gateway's resume does too: `resume_conversation` never dispatches a refused call, it writes
    // the refusal where the tool's result would have gone. Gating on the outcome here as well
    // would be a second place to keep the same rule, and the place that already keeps it is the
    // one that runs the call.
    let (gate_yes, review_yes): (&[String], &[String]) = match answered.reason {
        opengrok_core::run::SuspendReason::AutoReview => {
            (&[], std::slice::from_ref(&answered.call_id))
        }
        _ => (std::slice::from_ref(&answered.call_id), &[]),
    };
    let Some(runner) = tools_for_coworker(
        &state,
        &account_id,
        &coworker_id,
        gate_yes,
        review_yes,
        TURN_WAKE_PATIENCE,
    )
    .await
    else {
        tracing::warn!(run = %run_id, "an answered run has no tools to continue with");
        return;
    };
    // A YES on a leave-box action is the person's consent to leave through the tunnel for the
    // rest of this run: one card per run, not one per click. A no is not: this once consented on
    // any answer, so a Deny on the tunnel card let the model's next screen action through with
    // no card at all (21 Sep 2026).
    let runner = runner.with_egress_consented(
        matches!(outcome, opengrok_harness::ResumeOutcome::Approved)
            && answered.reason == opengrok_core::run::SuspendReason::AutoReview
            && opengrok_tools::leaves_the_box(&answered.tool),
    );

    // The system message this turn OPENED with, not a fresh composition: a role edited while the
    // person was answering the card must not change the coworker halfway through. A run journalled
    // before this was captured has none and composes identity+role, matching the desktop resume.
    let system = match run.system_for_resume() {
        Some(captured) => captured,
        None => crate::persona::system_message(
            &coworker.name,
            &crate::persona::of(&state, &coworker_id, coworker.role.clone()).await,
            None,
        ),
    };

    let journal = StoreJournal {
        state: state.clone(),
        thread_id: run.thread_id.clone(),
        account_id: Some(account_id.clone()),
        coworker_id: run.coworker_id.clone(),
        model: run.model.clone(),
        system: Some(system.clone()),
        skill_id: run.skill_id.clone(),
    };

    let request = ModelRequest {
        gateway_key: crate::spend::key_for_opt(&state, run.coworker_id.as_ref(), Some(&account_id))
            .await,
        spend_scope: run.coworker_id.as_ref().map(|c| c.as_str().to_string()),
        // The person who answered the card is the person this continuation is for.
        spend_actor: Some(account_id.as_str().to_string()),
        // The pin the turn started on, not the coworker's current one. A coworker that was
        // repinned while this run waited on a card must not change what the continuation thinks
        // with. Logs written before the pin was stored fall back to the current pin.
        model: run.pin_for_resume(&coworker.model),
        system: Some(system),
        messages: conversation_from(&run),
        tools: Vec::new(),
    };

    // The run keeps its id, so everything the resumption emits lands in the same log and a client
    // replaying later sees one continuous run rather than two halves.
    let events = opengrok_harness::resume_conversation(
        state.door.as_ref(),
        &runner,
        &journal,
        request,
        opengrok_harness::RunContext::new(&run.thread_id, run_id.as_str(), now_ms()),
        opengrok_harness::Resumption {
            approved: opengrok_tools::ToolCall {
                id: answered.call_id,
                name: answered.tool,
                arguments: answered.arguments,
            },
            message_seq: resumed_seq,
            outcome,
        },
    )
    .await;

    tracing::info!(run = %run_id, events = events.len(), "continued an answered run");
    // The continued run may pause again — on a user form, a saved-login request — and that
    // pause needs its card in the transcript exactly as a fresh turn's does: without the
    // card there is no `entryId`, and NativeChat cannot submit what the person typed.
    let agent_id = coworker_id.as_str().to_string();
    super::resume::emit_user_form_suspensions(&host, &coworker_id, &account_id, &agent_id, &events)
        .await;
}

/// Runs waiting on this person.
///
/// A suspended run nobody can find is a run nobody will answer, which is the same as a lost one.
pub async fn list_awaiting(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    // The queue the person is shown, not the one the machinery walks: a card is the loudest
    // surface in the app, and a turn they deleted must not come back asking to be looked at.
    match state
        .auth
        .store
        .awaiting_approval_to_show(&account_id)
        .await
    {
        Ok(runs) => {
            let mut waiting = Vec::new();
            for run_id in runs {
                if let Ok((run, _)) = state.auth.store.load_run(&run_id).await
                    && let Some(pending) = run.pending.clone()
                {
                    waiting.push(serde_json::json!({
                        "runId": run_id.as_str(),
                        "threadId": run.thread_id,
                        "callId": pending.call_id,
                        "tool": pending.tool,
                        // What is actually being approved. A person asked to approve "shell"
                        // without seeing the command is being asked to approve nothing.
                        "arguments": pending.arguments,
                        // WHICH QUESTION IS BEING ASKED. The judge's ask, a policy grant's, the
                        // machine owner's consent and a form are four different things that all
                        // land in this one queue, and a client that cannot tell them apart can
                        // only offer one word for all four. The run's own word, not a new one.
                        "reason": pending.reason.as_str(),
                        // And why, in a sentence: the ask's own words when the run journalled
                        // them, else a sentence built from the reason — see `why_of`.
                        "why": journalled_why(&run, &pending).unwrap_or_else(|| why_of(&pending)),
                    }));
                }
            }
            // An ARRAY, always: an empty queue is a valid answer.
            Json(waiting).into_response()
        }
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    }
}

/// The ask's own sentence, as the run journalled it on its `run-awaiting-approval` event for
/// this call. The egress tunnel's card and a judge's card are both `auto-review` on the run,
/// and only this sentence tells them apart — a client that rebuilds the card from the queue
/// (NativeChat after a relaunch) needs the same words the stream carried, or the tunnel's card
/// comes back as a judge's. Read from the loaded run's own events; no other row is touched.
fn journalled_why(
    run: &opengrok_core::run::Run,
    pending: &opengrok_core::run::PendingApproval,
) -> Option<String> {
    // Only the one ambiguous kind. A form's or a policy card's journalled words are the
    // ask's bare line ("Waiting for you"), and `why_of` says more for those.
    if pending.reason != opengrok_core::run::SuspendReason::AutoReview {
        return None;
    }
    let text = |event: &serde_json::Value, key: &str| -> Option<String> {
        event
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    run.emitted.iter().rev().find_map(|event| {
        (text(event, "type").as_deref() == Some("CUSTOM")
            && text(event, "name").as_deref() == Some("run-awaiting-approval")
            && text(event, "callId").as_deref() == Some(pending.call_id.as_str()))
        .then(|| text(event, "why"))
        .flatten()
        .map(|why| why.trim().to_string())
        .filter(|why| !why.is_empty())
    })
}

/// Why this call is waiting, in a sentence, when the run journalled no words of its own: which
/// question is being asked, and what the call would do if the answer is yes.
///
/// FROM THE RUN AND NOTHING ELSE. The run already holds the reason, the tool and the arguments,
/// which is what the sentence is made of. The opening line says which of the four questions this is; `cards::summary_for`
/// writes the rest, the same words the card itself uses for what is about to happen.
fn why_of(pending: &opengrok_core::run::PendingApproval) -> String {
    use opengrok_core::run::SuspendReason;
    let what = crate::cards::summary_for(&pending.tool, &pending.arguments);
    let asking = match pending.reason {
        SuspendReason::PolicyApproval => crate::cards::POLICY_ASK_REASON,
        // Deliberately not the judge's default reason text: the ask's own sentence lives on the
        // card, and an egress-tunnel ask has a different one. Saying which judge instruction
        // fired, from a run that does not know, would be a guess printed as a fact.
        SuspendReason::AutoReview => "Auto-review asked about this rather than allowing it.",
        SuspendReason::ExecConsent => {
            "This would run on your own computer, so it needs your consent."
        }
        SuspendReason::UserForm => "The coworker is asking you to fill something in.",
    };
    format!("{asking} {what}")
}

/// `?coworker=cw_…`: whose computer the egress-tunnel question is about.
#[derive(Deserialize)]
pub struct HostSettingsQuery {
    coworker: Option<String>,
}

/// The host's settings record, plus whether the egress tunnel is live for the coworker asked
/// about. What the desktop verbs `getHostSettings` and `isEgressTunnelAvailable` answered, on
/// the door NativeChat already uses and under its own account token — so the seam-A door can
/// close without the settings page losing its host.
///
/// `egressTunnelAvailable` is host intent AND that coworker's box advertising the tunnel. No
/// coworker named, not this account's, no computer, or no box report → false: a client must not
/// paint the toggle live until a laptop client is attached.
pub async fn host_settings(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Query(query): Query<HostSettingsQuery>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    host_settings_reply(&state, &account_id, query.coworker.as_deref()).await
}

/// A partial record: the keys given replace the host's and the rest stay, the merge
/// `setHostSettings` did. Answers the whole record, so the client reads back what it set.
pub async fn patch_host_settings(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Query(query): Query<HostSettingsQuery>,
    Json(patch): Json<serde_json::Value>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let Some(patch) = patch.as_object() else {
        return (StatusCode::BAD_REQUEST, "a settings patch is a JSON object").into_response();
    };
    let Some(lock) = state.host_settings.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "this host keeps no settings",
        )
            .into_response();
    };
    if let Ok(mut settings) = lock.lock() {
        if !settings.is_object() {
            *settings = crate::host_state::default_settings();
        }
        if let Some(record) = settings.as_object_mut() {
            for (key, value) in patch {
                record.insert(key.clone(), value.clone());
            }
        }
    }
    host_settings_reply(&state, &account_id, query.coworker.as_deref()).await
}

async fn host_settings_reply(
    state: &AgUiState,
    account_id: &AccountId,
    coworker: Option<&str>,
) -> Response {
    let mut record = state
        .host_settings
        .as_ref()
        .and_then(|lock| lock.lock().ok().map(|value| value.clone()))
        .unwrap_or_else(crate::host_state::default_settings);
    let available = egress_tunnel_available_for(state, account_id, coworker).await;
    if let Some(record) = record.as_object_mut() {
        record.insert(
            "egressTunnelAvailable".to_string(),
            serde_json::Value::Bool(available),
        );
    }
    Json(record).into_response()
}

/// Host intent, then the coworker: it must be named, be this account's, have a computer in its
/// scope, and that box must say the tunnel is ready. Each miss is a plain false, never an error
/// — the settings page still has its record to show. The box is the SCOPE's live one, through
/// its own provider, the same one the Computer pane and a turn's tools use; this once read the
/// id frozen on the coworker's row with the boot-time provider and disagreed with both.
async fn egress_tunnel_available_for(
    state: &AgUiState,
    account_id: &AccountId,
    coworker: Option<&str>,
) -> bool {
    if !state.egress_tunnel_enabled() {
        return false;
    }
    let Some(coworker) = coworker else {
        return false;
    };
    let coworker_id = CoworkerId::from_stored(coworker.to_string());
    let owns = state
        .auth
        .store
        .coworkers_for(account_id)
        .await
        .map(|roster| roster.iter().any(|view| view.id == coworker_id))
        .unwrap_or(false);
    if !owns {
        return false;
    }
    let Some(scoped) = provision::scoped_box_for(state, account_id, &coworker_id).await else {
        return false;
    };
    state
        .egress_tunnel_for(scoped.computer.as_ref(), &scoped.box_id)
        .await
}

/// The message a reply points at, as the one bracketed line `reply_context` writes for the
/// desktop's own transcript — so a reply reads the same whichever door the turn came through.
///
/// `replyTo` is either the quoted message's id or NativeChat's object (`messageId`, `preview`,
/// `isMe`). The quoted message itself is preferred, with all of its words; the preview is what is
/// left when the client has since dropped the message from the array it sends.
fn reply_quote(
    message: &opengrok_wire::agui::Message,
    sent: &[opengrok_wire::agui::Message],
) -> Option<String> {
    let reply_to = message.extra.get("replyTo")?;
    let (id, preview, is_me) = match reply_to {
        serde_json::Value::String(id) => (Some(id.as_str()), None, None),
        serde_json::Value::Object(_) => (
            reply_to
                .get("messageId")
                .and_then(serde_json::Value::as_str),
            reply_to.get("preview").and_then(serde_json::Value::as_str),
            reply_to.get("isMe").and_then(serde_json::Value::as_bool),
        ),
        _ => return None,
    };
    let quoted = id.and_then(|id| sent.iter().find(|candidate| candidate.id == id));
    let text = quoted
        .and_then(|quoted| quoted.content.as_deref())
        .or(preview)?;
    // Whose words are being quoted, from the model's side of the conversation: the person's own
    // earlier message, or the coworker's.
    let from_person = quoted
        .map(|quoted| quoted.role == "user")
        .or(is_me)
        .unwrap_or(false);
    let who = if from_person {
        "their own earlier message"
    } else {
        "your earlier message"
    };
    crate::agui::resume::reply_quote_line(who, text)
}

/// A user message with the quote it answers ahead of it.
///
/// IDEMPOTENT ON PURPOSE. A client that cannot rely on this field spells the quote into `content`
/// itself — NativeChat does, so a reply works against a server that predates `replyTo` — and
/// saying it twice is worse than not reading the field at all.
pub(super) fn with_reply_context(
    content: &str,
    message: &opengrok_wire::agui::Message,
    sent: &[opengrok_wire::agui::Message],
) -> String {
    if content
        .trim_start()
        .starts_with(crate::agui::resume::REPLY_QUOTE_OPENING)
    {
        return content.to_string();
    }
    match reply_quote(message, sent) {
        Some(quote) => format!("{quote}\n\n{content}"),
        None => content.to_string(),
    }
}

/// AG-UI messages to the model's vocabulary.
///
/// Roles the model door does not understand are dropped rather than passed through: a provider
/// that rejects an unknown role fails the whole turn, and AG-UI carries roles (`developer`) that
/// have no place in a chat completion.
pub fn to_chat_messages(input: &RunAgentInput) -> Vec<ChatMessage> {
    input
        .messages
        .iter()
        .filter_map(|message| match message.role.as_str() {
            "user" | "assistant" | "system" => {
                message.content.as_ref().map(|content| ChatMessage {
                    images: Vec::new(),
                    role: message.role.clone(),
                    // Only a person replies: the field on anything else is not a quote the model
                    // should be read to.
                    content: match message.role.as_str() {
                        "user" => with_reply_context(content, message, &input.messages),
                        _ => content.clone(),
                    },
                })
            }
            // NativeChat continues a frontend tool by POSTing the result as a tool
            // message. The door only speaks user/assistant/system, so this is the
            // same sentence the in-process loop would have appended.
            "tool" => {
                let content = message.content.clone().unwrap_or_default();
                let call_id = message
                    .extra
                    .get("toolCallId")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(message.id.as_str());
                Some(ChatMessage {
                    images: Vec::new(),
                    role: "user".to_string(),
                    content: format!("[tool {call_id} result] {content}"),
                })
            }
            _ => None,
        })
        .collect()
}

/// Live AG-UI frames, forwarded as they are produced. Dropping the HTTP body closes the
/// channel; the spawned turn still runs so a disconnect does not abandon the journal.
///
/// A UserForm CUSTOM is stamped with `entryId` here, **before** the frame is sent: mint the
/// gateway card first (same `user_form_card` / sanitize as `sendPrompt`), then forward
/// `name: run-awaiting-approval` + `reason: user-form` + that id. NativeChat never watches
/// the transcript live stream, so an after-the-fact append does not unblock them.
///
/// `request_user_form` TOOL_CALL frames are held until that stamp: NativeChat paints Website
/// login from TOOL_CALL and falls back to the raw `toolCallId` (`call-…`) when `entryId` is
/// missing. Streaming those frames during the model completion is how a second same-title
/// card stayed `call-*-1` with Continue that could not submit.
struct AgUiSink {
    tx: tokio::sync::mpsc::UnboundedSender<Event>,
    gateway: crate::host_state::HostState,
    coworker_id: Option<CoworkerId>,
    account_id: Option<opengrok_core::id::AccountId>,
    form_hold: Mutex<crate::agui::user_form::UserFormSseHold>,
}

impl AgUiSink {
    fn send(&self, event: Event) -> bool {
        self.tx.send(event).is_ok()
    }

    fn hold_or_pass(&self, event: Event) -> Option<Event> {
        let Ok(mut hold) = self.form_hold.lock() else {
            return Some(event);
        };
        hold.push(event)
    }

    fn release_form(&self, call_id: &str, entry_id: Option<&str>) -> Vec<Event> {
        let Ok(mut hold) = self.form_hold.lock() else {
            return Vec::new();
        };
        hold.release_for(call_id, entry_id)
    }

    fn release_held_forms(&self, clean: bool) -> Vec<Event> {
        let Ok(mut hold) = self.form_hold.lock() else {
            return Vec::new();
        };
        hold.release_at_end(clean)
    }
}

#[async_trait::async_trait]
impl EventSink for AgUiSink {
    async fn emit(&self, events: &[Event]) {
        for event in events {
            let mut event = event.clone();
            if crate::agui::user_form::is_live_user_form_custom(&event) {
                if let (Some(coworker_id), Some(account_id)) = (&self.coworker_id, &self.account_id)
                {
                    crate::agui::resume::stamp_user_form_entry_id(
                        &self.gateway,
                        coworker_id,
                        account_id,
                        &mut event,
                    )
                    .await;
                }
                let call_id = event
                    .extra
                    .get("callId")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let entry_id = event
                    .extra
                    .get("entryId")
                    .and_then(serde_json::Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(str::to_string);
                for held in self.release_form(&call_id, entry_id.as_deref()) {
                    if !self.send(held) {
                        return;
                    }
                }
                if !self.send(event) {
                    return;
                }
                continue;
            }
            if matches!(
                event.event_type,
                opengrok_wire::agui::EventType::RunFinished
                    | opengrok_wire::agui::EventType::RunError
            ) {
                let clean = event.event_type == opengrok_wire::agui::EventType::RunFinished;
                for held in self.release_held_forms(clean) {
                    if !self.send(held) {
                        return;
                    }
                }
                if !self.send(event) {
                    return;
                }
                continue;
            }
            if let Some(event) = self.hold_or_pass(event)
                && !self.send(event)
            {
                return;
            }
        }
    }
}

/// Wrap an event stream in the SSE response openbot expects.
fn sse<S, E>(events: S) -> Response
where
    S: Stream<Item = Result<Event, E>> + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    use futures::StreamExt;

    let body = events.map(|event| {
        event.map(|event| {
            // An event that will not serialise is dropped rather than allowed to panic mid-run;
            // `to_sse_frame` returns None and the stream continues.
            event.to_sse_frame().unwrap_or_default()
        })
    });

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            // Without this a proxy may buffer the whole run and deliver it at the end, which looks
            // exactly like a server that never streamed.
            (header::CACHE_CONTROL, "no-cache"),
            (header::CONNECTION, "keep-alive"),
            (HeaderName::from_static("x-accel-buffering"), "no"),
        ],
        axum::body::Body::from_stream(body),
    )
        .into_response()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use opengrok_harness::MockDoor;
    use opengrok_wire::agui::{EventType, Message};
    use serde_json::json;

    #[test]
    fn a_stopped_turn_continues_and_a_parked_card_does_not() {
        assert!(prior_turn_can_continue(RunStatus::Stopped, &[]));
        assert!(prior_turn_can_continue(RunStatus::Failed, &[]));
        assert!(!prior_turn_can_continue(RunStatus::Finished, &[]));
        assert!(!prior_turn_can_continue(
            RunStatus::Stopped,
            &[json!({"type":"CUSTOM","name":"run-awaiting-approval"})]
        ));

        let tools = unfinished_tool_messages(&[
            json!({"type":"TOOL_CALL_START","toolCallId":"c1","toolCallName":"user_machine_shell"}),
            json!({"type":"TOOL_CALL_ARGS","toolCallId":"c1","delta":"{\"command\":\"gpui-agent invoke profile.create\"}"}),
            json!({"type":"TOOL_CALL_RESULT","toolCallId":"c1","content":"exit 0\n--- stdout ---\n{\"result\":{\"view\":\"profile-manager\"}}"}),
        ]);
        assert_eq!(tools.len(), 1);
        assert!(
            tools[0].content.contains("profile.create"),
            "{}",
            tools[0].content
        );
        assert!(
            tools[0].content.contains("profile-manager"),
            "{}",
            tools[0].content
        );

        let mut messages = vec![
            ChatMessage {
                role: "user".to_string(),
                content: "create Juana Jane".to_string(),
                images: Vec::new(),
            },
            ChatMessage {
                role: "user".to_string(),
                content: "tin number should be 00000000000001".to_string(),
                images: Vec::new(),
            },
        ];
        splice_unfinished_tools(&mut messages, tools);
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].content, "create Juana Jane");
        assert!(messages[1].content.contains("profile.create"));
        assert!(messages[2].content.contains("previous turn"));
        assert_eq!(messages[3].content, "tin number should be 00000000000001");

        let again = messages.clone();
        let mut doubled = again.clone();
        splice_unfinished_tools(&mut doubled, again);
        assert_eq!(
            doubled.len(),
            4,
            "a second splice does not repeat the block"
        );
    }

    fn input(messages: Vec<Message>) -> RunAgentInput {
        RunAgentInput {
            thread_id: "t1".to_string(),
            run_id: "r1".to_string(),
            parent_run_id: None,
            state: json!(null),
            messages,
            tools: json!(null),
            context: json!(null),
            forwarded_props: json!(null),
            extra: Default::default(),
        }
    }

    #[test]
    fn the_recipe_a_person_chose_is_read_off_the_request() {
        let mut bare = input(Vec::new());
        assert!(
            chosen_recipe_from(&bare).is_none(),
            "no props, nothing chosen"
        );

        bare.forwarded_props = json!({ "coworkerId": "cw_1" });
        assert!(chosen_recipe_from(&bare).is_none(), "no recipe key");

        // No values is a chosen recipe all the same: a recipe may declare nothing, and one that
        // declares something still refuses later by name rather than being ignored here.
        bare.forwarded_props = json!({ "recipe": "rcp_1" });
        let (recipe, values) = chosen_recipe_from(&bare).unwrap();
        assert_eq!(recipe, "rcp_1");
        assert!(values.is_empty());

        // A client may send a number or a boolean as itself; a step types text either way.
        bare.forwarded_props = json!({
            "recipe": "rcp_1",
            "recipeValues": { "q": "mundo", "count": 3, "loud": true, "junk": ["no"] }
        });
        let (_, values) = chosen_recipe_from(&bare).unwrap();
        assert_eq!(values.get("q").map(String::as_str), Some("mundo"));
        assert_eq!(values.get("count").map(String::as_str), Some("3"));
        assert_eq!(values.get("loud").map(String::as_str), Some("true"));
        assert!(
            !values.contains_key("junk"),
            "a list is not a value for a field"
        );

        bare.forwarded_props = json!({ "recipe": "   " });
        assert!(
            chosen_recipe_from(&bare).is_none(),
            "a blank id is not an id"
        );
    }

    #[test]
    fn a_refusal_names_the_tool_and_the_card_that_refused_it() {
        let pending = |reason| opengrok_core::run::PendingApproval {
            call_id: "call-1".to_string(),
            tool: "shell".to_string(),
            arguments: json!({}),
            reason,
        };

        // A yes is a yes whatever asked.
        assert!(matches!(
            resume_outcome(
                true,
                &pending(opengrok_core::run::SuspendReason::ExecConsent)
            ),
            opengrok_harness::ResumeOutcome::Approved
        ));

        // A no says which card, because the three cards mean different things and the model can
        // only choose something else if it knows what it ran into.
        // An approval coming back from a no would leave this empty, which every assertion below
        // then fails on — said that way round because this module may not panic.
        let said = |reason| match resume_outcome(false, &pending(reason)) {
            opengrok_harness::ResumeOutcome::Refused(why) => why,
            opengrok_harness::ResumeOutcome::Approved => String::new(),
            opengrok_harness::ResumeOutcome::Settled(why) => why,
        };
        let consent = said(opengrok_core::run::SuspendReason::ExecConsent);
        assert!(!consent.is_empty(), "a no is not an approval");
        assert!(consent.contains("shell"), "{consent}");
        assert!(
            consent.contains("did not run"),
            "the model is told the call did not happen, not merely that somebody said no: \
             {consent}"
        );
        let policy = said(opengrok_core::run::SuspendReason::PolicyApproval);
        assert!(policy.contains("policy"), "{policy}");
        assert!(policy.contains("shell"), "{policy}");
        let review = said(opengrok_core::run::SuspendReason::AutoReview);
        assert!(review.contains("auto-review"), "{review}");
        let form = said(opengrok_core::run::SuspendReason::UserForm);
        assert!(
            form.contains("dismissed") || form.contains("without filling"),
            "{form}"
        );
        assert!(
            matches!(
                resume_outcome(true, &pending(opengrok_core::run::SuspendReason::UserForm)),
                opengrok_harness::ResumeOutcome::Settled(_)
            ),
            "a yes on a user-form must not re-run request_user_form"
        );

        // None of them blames the model or reads as an error. A refusal is a decision somebody
        // made, and a sentence that sounds like a fault invites an apology and a retry.
        for why in [consent, policy, review] {
            let lower = why.to_ascii_lowercase();
            for word in ["error", "failed", "sorry", "invalid"] {
                assert!(
                    !lower.contains(word),
                    "a refusal must not read as a fault ({word}): {why}"
                );
            }
        }
    }

    #[test]
    fn named_tools_are_read_off_the_request_and_kept_to_what_is_offered() {
        let mut bare = input(Vec::new());
        assert!(
            preferred_tools_from(&bare).is_empty(),
            "no props, nothing named"
        );

        bare.forwarded_props = json!({ "coworkerId": "cw_1" });
        assert!(preferred_tools_from(&bare).is_empty(), "no preferTools key");

        bare.forwarded_props = json!({ "preferTools": ["open_url", "  ", "", "computer"] });
        assert_eq!(
            preferred_tools_from(&bare),
            vec!["open_url".to_string(), "computer".to_string()],
            "blank names are not names"
        );

        // Nothing is offered without a runner, so nothing is named to the model — a system
        // message naming a tool the model was not given is an instruction it cannot follow.
        assert!(
            honour_preferences(&["open_url".to_string()], None).is_empty(),
            "no tools this turn means no preference to state"
        );
    }

    fn message(role: &str, content: Option<&str>) -> Message {
        Message {
            id: "m1".to_string(),
            role: role.to_string(),
            content: content.map(str::to_string),
            name: None,
            extra: Default::default(),
        }
    }

    #[test]
    fn chat_roles_the_model_understands_are_kept() {
        let messages = to_chat_messages(&input(vec![
            message("system", Some("be brief")),
            message("user", Some("hello")),
            message("assistant", Some("hi")),
        ]));
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1].content, "hello");
    }

    /// AG-UI carries roles a chat completion has no place for. Passing one through fails the whole
    /// turn on providers that reject unknown roles.
    #[test]
    fn roles_the_model_does_not_understand_are_dropped() {
        let messages = to_chat_messages(&input(vec![
            message("developer", Some("internal")),
            message("user", Some("hello")),
        ]));
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "user");
    }

    #[test]
    fn a_tool_result_becomes_the_in_process_sentence() {
        let mut tool = message("tool", Some("shown in the chat"));
        tool.id = "c1".to_string();
        tool.extra.insert("toolCallId".to_string(), json!("c1"));
        let messages = to_chat_messages(&input(vec![tool]));
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, "[tool c1 result] shown in the chat");
    }

    /// The desktop's reply chip has to reach the model as words, or "what am I replying to?"
    /// arrives with nothing to answer from.
    #[test]
    fn a_reply_is_read_to_the_model_as_a_quote_ahead_of_its_own_words() {
        let mut quoted = message("assistant", Some("The build is green."));
        quoted.id = "m1".to_string();
        let mut reply = message("user", Some("what am I replying to?"));
        reply.id = "m2".to_string();
        reply.extra.insert(
            "replyTo".to_string(),
            json!({"messageId": "m1", "preview": "The build is green.", "isMe": false}),
        );
        let messages = to_chat_messages(&input(vec![quoted, reply]));
        assert_eq!(
            messages[1].content,
            "[Replying to your earlier message: \"The build is green.\"]\n\nwhat am I replying to?"
        );
    }

    /// Answering yourself is a different sentence, and the model has to be able to tell.
    #[test]
    fn a_reply_to_the_persons_own_message_says_whose_it_was() {
        let mut quoted = message("user", Some("remind me at five"));
        quoted.id = "m1".to_string();
        let mut reply = message("user", Some("make that six"));
        reply.id = "m2".to_string();
        reply.extra.insert("replyTo".to_string(), json!("m1"));
        let messages = to_chat_messages(&input(vec![quoted, reply]));
        assert_eq!(
            messages[1].content,
            "[Replying to their own earlier message: \"remind me at five\"]\n\nmake that six"
        );
    }

    /// NativeChat writes the quote into `content` as well, so a reply works against a server that
    /// has never heard of `replyTo`. Reading the field must not say it twice.
    #[test]
    fn a_quote_the_client_already_wrote_is_not_written_again() {
        let mut quoted = message("assistant", Some("The build is green."));
        quoted.id = "m1".to_string();
        let mut reply = message(
            "user",
            Some("[Replying to your earlier message: \"The build is green.\"]\n\nwhy?"),
        );
        reply.id = "m2".to_string();
        reply.extra.insert(
            "replyTo".to_string(),
            json!({"messageId": "m1", "preview": "The build is green.", "isMe": false}),
        );
        let messages = to_chat_messages(&input(vec![quoted, reply]));
        assert_eq!(
            messages[1].content,
            "[Replying to your earlier message: \"The build is green.\"]\n\nwhy?"
        );
    }

    /// The quoted message may be gone from the array the client sends; the preview it saved with
    /// the reply is what is left of it.
    #[test]
    fn a_quote_whose_message_is_not_in_the_array_falls_back_to_the_preview() {
        let mut reply = message("user", Some("why?"));
        reply.id = "m2".to_string();
        reply.extra.insert(
            "replyTo".to_string(),
            json!({"messageId": "gone", "preview": "The build is green.", "isMe": false}),
        );
        let messages = to_chat_messages(&input(vec![reply]));
        assert_eq!(
            messages[0].content,
            "[Replying to your earlier message: \"The build is green.\"]\n\nwhy?"
        );
    }

    /// A message with no content is a placeholder the client is still filling in.
    #[test]
    fn a_message_without_content_is_skipped() {
        let messages = to_chat_messages(&input(vec![message("user", None)]));
        assert!(messages.is_empty());
    }

    /// End to end through the mock door: no provider, no key, and still a complete run.
    #[tokio::test]
    async fn a_run_through_the_mock_door_is_well_formed() {
        let door = MockDoor::echoing();
        let events = opengrok_harness::run_conversation(
            &door,
            None,
            &opengrok_harness::MemoryJournal::new(),
            ModelRequest {
                gateway_key: None,
                spend_scope: None,
                spend_actor: None,
                model: "mock".to_string(),
                system: None,
                messages: to_chat_messages(&input(vec![message("user", Some("ping"))])),
                tools: Vec::new(),
            },
            "t1",
            "r1",
            1,
        )
        .await;

        assert_eq!(events.first().unwrap().event_type, EventType::RunStarted);
        assert_eq!(events.last().unwrap().event_type, EventType::RunFinished);
        assert_eq!(events.first().unwrap().extra.get("threadId").unwrap(), "t1");

        for event in &events {
            let frame = event.to_sse_frame().unwrap();
            assert!(frame.starts_with("data: "));
            assert_eq!(frame.matches("\n\n").count(), 1, "{frame:?}");
        }
    }

    /// NativeChat mounts CUSTOM `run-awaiting-approval` + `reason: user-form`. `entryId` is a
    /// flattened extra field (same envelope as `callId` / `reason`), not a nested object.
    #[test]
    fn a_user_form_custom_frame_carries_entry_id_at_the_top_level() {
        let event = Event::new(EventType::Custom, 42)
            .with("name", "run-awaiting-approval")
            .with("threadId", "thr-1")
            .with("runId", "run-1")
            .with("callId", "mock-form-1")
            .with("tool", "request_user_form")
            .with("reason", "user-form")
            .with("why", "Waiting for you")
            .with("entryId", "e_form")
            .with(
                "arguments",
                json!({
                    "title": "Google account",
                    "instruction": "Enter the address and password.",
                    "liveHost": "accounts.google.com",
                    "fields": [{
                        "id": "email",
                        "label": "Email",
                        "type": "email",
                        "required": true,
                        "secret": false
                    }]
                }),
            )
            .with(
                "formRequest",
                json!({
                    "title": "Google account",
                    "instruction": "Enter the address and password.",
                    "liveHost": "accounts.google.com",
                    "fields": [{
                        "id": "email",
                        "label": "Email",
                        "type": "email",
                        "required": true,
                        "secret": false
                    }]
                }),
            );
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(wire["type"], "CUSTOM");
        assert_eq!(wire["name"], "run-awaiting-approval");
        assert_eq!(wire["reason"], "user-form");
        assert_eq!(wire["entryId"], "e_form");
        assert_eq!(wire["formRequest"]["title"], "Google account");
        assert_eq!(wire["arguments"]["title"], "Google account");
        assert!(wire.get("values").is_none());
    }

    fn a_run(status: RunStatus, failure: Option<&str>) -> opengrok_core::run::Run {
        opengrok_core::run::Run {
            thread_id: "th".to_string(),
            status,
            failure: failure.map(str::to_string),
            ..opengrok_core::run::Run::default()
        }
    }

    fn closer_types(events: &[Event]) -> Vec<(EventType, Option<String>)> {
        events
            .iter()
            .map(|event| {
                let name = event
                    .extra
                    .get("name")
                    .or_else(|| event.extra.get("message"))
                    .and_then(|value| value.as_str())
                    .map(str::to_string);
                (event.event_type, name)
            })
            .collect()
    }

    /// AN OWNER THE STORE COULD NOT CONFIRM IS TOLD TO RETRY, NOT TO START OVER. A 409 sends the
    /// owner of a dropped stream to a new run id, and the turn runs twice.
    #[test]
    fn a_failed_ownership_read_is_a_503_not_run_exists() {
        assert_eq!(
            not_attached(Err(opengrok_store::StoreError::Corrupt("gone".to_string()))).status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(not_attached(Ok(false)).status(), StatusCode::CONFLICT);
    }

    /// AN ATTACHED STREAM ENDS THE WAY THE RUN ENDED, NOT THE WAY ITS LOG LAST CLOSED. A parked
    /// run that was stopped, or swept after its card was answered, still ends its log with the
    /// park's `RUN_FINISHED`; the closer used to be that one, which left a live card on screen.
    #[test]
    fn an_attached_stream_closes_with_what_the_run_is() {
        let run_id = RunId::from_stored("run-1".to_string());
        let park_closer = Event::new(EventType::RunFinished, 1);
        let run_error = Event::new(EventType::RunError, 1).with("message", "the model hung up");

        let stopped = attached_closer(
            &a_run(RunStatus::Stopped, None),
            &run_id,
            Some(&park_closer),
            false,
            2,
        );
        assert_eq!(
            closer_types(&stopped),
            vec![
                (EventType::Custom, Some("run-stopped".to_string())),
                (EventType::RunFinished, None)
            ]
        );

        let swept = attached_closer(
            &a_run(RunStatus::Failed, Some("interrupted by a restart")),
            &run_id,
            Some(&park_closer),
            false,
            2,
        );
        assert_eq!(
            closer_types(&swept),
            vec![(
                EventType::RunError,
                Some("interrupted by a restart".to_string())
            )]
        );

        let failed = attached_closer(
            &a_run(RunStatus::Failed, Some("the model hung up")),
            &run_id,
            Some(&run_error),
            false,
            2,
        );
        assert_eq!(failed.len(), 1);
        assert_eq!(
            failed[0].timestamp, run_error.timestamp,
            "the log's own closer"
        );

        // A loop that ended as a stop already sent its notice; only the closer is left.
        let stopped_by_loop = attached_closer(
            &a_run(RunStatus::Stopped, None),
            &run_id,
            Some(&park_closer),
            true,
            2,
        );
        assert_eq!(
            closer_types(&stopped_by_loop),
            vec![(EventType::RunFinished, None)]
        );

        let parked = attached_closer(
            &a_run(RunStatus::AwaitingApproval, None),
            &run_id,
            Some(&park_closer),
            false,
            2,
        );
        assert_eq!(parked.len(), 1);
        assert_eq!(parked[0].timestamp, park_closer.timestamp);
    }

    /// A RUN WITH NO FRAMES YET GETS NO CARDS. Hydration appends the transcript cards it cannot
    /// place, and with nothing logged the run's window is everything, so every card this
    /// coworker ever showed would reach an attached stream — and the next look would skip as
    /// many real frames. Only the log's own frames go out.
    #[test]
    fn an_attached_stream_sends_only_the_logs_own_frames() {
        let card = json!({
            "kind": "send-message",
            "id": "e_old",
            "timestampMs": 5,
            "message": {
                "type": "user-form",
                "formRequest": {"title": "Sign in", "fields": [{"id": "email", "label": "Email"}]}
            }
        });
        let (from, to) = run_time_window(&[]);
        let hydrated = crate::agui::user_form::hydrate_agui_events(
            Vec::new(),
            std::slice::from_ref(&card),
            from,
            to,
        );
        assert_eq!(
            hydrated.len(),
            1,
            "hydration invents a card for an empty run"
        );
        assert!(
            log_frames(hydrated, 0).is_empty(),
            "the attached stream sends none"
        );
    }
}
