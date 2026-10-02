//! ONE DESK for a routine's writes (#316). `POST`, `PATCH` and `DELETE /schedules` are thin
//! adapters over it, and so are a Bot's routine tools (`Tools`, below): what one may store the
//! other may, refused in the same words, so the two cannot drift.
//!
//! A REFUSAL IS A STATUS AND A SENTENCE. The routes answer it as `{error}`, the shape NativeChat
//! reads (#262); a tool hands the sentence to the model, which can act on it (CLAUDE.md #8).
//!
//! THE FLOOR (#315's contract): a cron that can wake more often than once a minute is refused on
//! create, and on an edit that sets one. `OG_ROUTINE_SECOND_CRON=1` lifts it for the tests and
//! smokes that schedule in seconds, only behind a mock model door (the binary refuses to start
//! otherwise): a billed model woken every second is a bill, not a test.

use axum::http::StatusCode;
use opengrok_core::id::{AccountId, CoworkerId, ScheduleId};
use opengrok_core::limits::RunLimits;
use opengrok_core::schedule::{
    Schedule, ScheduleCommand, ScheduleError, ScheduleEvent, ScheduleView, UTC, Wake, WakeKind,
};
use opengrok_tools::ToolContext;
use opengrok_tools::routine::{self, Ask, Bot, Fields, RoutineDesk};
use serde_json::{Value, json};

use crate::agui::routes::AgUiState;
use crate::now_ms;

/// A status and the sentence that goes with it.
pub(crate) type Refusal = (StatusCode, String);

/// The floor's refusal, in the contract's words.
pub(crate) const FLOOR: &str =
    "a routine can wake at most once a minute: use 5 fields, like */5 * * * *.";

fn refused(status: StatusCode, why: impl Into<String>) -> Refusal {
    (status, why.into())
}

fn storage(error: impl std::fmt::Display) -> Refusal {
    tracing::error!(%error, "a routine could not be read or written");
    refused(StatusCode::INTERNAL_SERVER_ERROR, "storage failed")
}

/// What a create or an edit asks for, as a body or a tool's call says it. A field left out is
/// `None`, and an edit keeps what the routine has for it.
#[derive(Default)]
pub(crate) struct Draft {
    pub coworker: Option<CoworkerId>,
    pub name: Option<String>,
    pub prompt: Option<String>,
    /// `cron` or `webhook`: absent is cron, every body written before webhooks says so, and an
    /// edit may only name the kind the routine already is. A tool never names one.
    pub kind: Option<String>,
    pub cron: Option<String>,
    pub tz: Option<String>,
    pub run_limits: Option<Value>,
}

/// Can this coworker take a routine's work at all: hired, not retired, not a group? A retired
/// coworker's key is revoked at retirement, so its turn would run on the deployment's key outside
/// its spend cap; a group takes no model call, so every firing would fail.
pub(crate) async fn takes_work(state: &AgUiState, coworker_id: &CoworkerId) -> bool {
    state
        .auth
        .store
        .load_coworker(coworker_id)
        .await
        .map(|(coworker, _)| coworker.hired && !coworker.retired && !coworker.is_group())
        .unwrap_or(false)
}

/// May this account point this coworker at anything? It must take work — a schedule for a
/// typo'd id would only ever log refusals — and the account's grant must let it use it, refused
/// through `refuse_use`: one it may not even see is the 404 an unknown id gets.
pub(crate) async fn may_use(
    state: &AgUiState,
    account: &AccountId,
    coworker: &CoworkerId,
) -> Result<(), Refusal> {
    if !takes_work(state, coworker).await {
        return Err(refused(StatusCode::NOT_FOUND, "no such coworker"));
    }
    let policy = state.auth.store.policy_for(account, coworker).await;
    let using = opengrok_policy::Action::UseCoworker;
    let decision = opengrok_policy::decide(account, coworker, using, &policy.unwrap_or_default());
    match decision.reason() {
        Some(reason) => {
            Err(crate::agui::routes::refuse_use(state, account, coworker, reason).await)
        }
        None => Ok(()),
    }
}

/// What a routine may set as its limits, or the 422 that says why not: not three whole numbers
/// within the server's budget, or one above the org's ceiling as it stands now, named. A
/// ceiling lowered later binds at run time instead (`autonomy::fire`).
async fn limits(
    state: &AgUiState,
    account: &AccountId,
    coworker: Option<&CoworkerId>,
    sent: &Value,
) -> Result<RunLimits, Refusal> {
    let most = opengrok_harness::RunBudget::default().limits();
    let unprocessable = |why: String| refused(StatusCode::UNPROCESSABLE_ENTITY, why);
    let limits = RunLimits::from_json(sent, &most).map_err(unprocessable)?;
    let ceiling = state.auth.store.org_run_ceiling(account, coworker).await;
    match limits.over_ceiling(&ceiling.map_err(storage)?) {
        Some(why) => Err(unprocessable(why)),
        None => Ok(limits),
    }
}

/// Unnamed is the pre-pane shape of this API, and the pane shows the prompt's first words — on
/// create, and on an edit that blanks the name.
fn name_or_first_words(name: Option<&str>, prompt: &str) -> String {
    match name.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => name.to_string(),
        None => prompt
            .split_whitespace()
            .take(6)
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// A cron the person may store: present, and no faster than the floor unless this server lets
/// tests schedule in seconds. The aggregate still holds it to the parser, in its zone.
fn cron_wake(state: &AgUiState, cron: Option<String>) -> Result<Wake, Refusal> {
    let unprocessable = |why: &str| refused(StatusCode::UNPROCESSABLE_ENTITY, why);
    let cron = cron.filter(|cron| !cron.trim().is_empty());
    let cron = cron.ok_or_else(|| unprocessable("a cron routine needs a cron expression"))?;
    if opengrok_core::schedule::under_a_minute(&cron) && !state.auth.second_cron {
        return Err(unprocessable(FLOOR));
    }
    Ok(Wake::Cron { cron })
}

/// What the aggregate refuses: the caller's input is a 422 (its cron, prompt or zone), anything
/// else the routine's state — deleted, paused, a hook written before its key was stored — a 409.
fn decided(error: ScheduleError) -> Refusal {
    let input = matches!(
        error,
        ScheduleError::BadCron(_) | ScheduleError::EmptyPrompt | ScheduleError::UnknownTimeZone(_)
    );
    let status = match input {
        true => StatusCode::UNPROCESSABLE_ENTITY,
        false => StatusCode::CONFLICT,
    };
    refused(status, error.to_string())
}

/// The zone a create left out: the person's own (`PUT /account {timeZone}`), else UTC.
async fn zone_of(state: &AgUiState, account: &AccountId) -> Result<String, Refusal> {
    let (account, _) = state
        .auth
        .store
        .load_account(account)
        .await
        .map_err(storage)?;
    Ok(account.time_zone.unwrap_or_else(|| UTC.to_string()))
}

/// Make a routine for `account`, and the id it is under. The wake is a cron within the floor, or
/// a webhook whose hook id and key are minted HERE rather than taken from the caller: a hook id
/// somebody else may pick is a namespace they can collide with, and a key somebody else may pick
/// is a password they chose for us.
pub(crate) async fn create(
    state: &AgUiState,
    account: &AccountId,
    draft: Draft,
) -> Result<ScheduleId, Refusal> {
    let coworker = draft
        .coworker
        .unwrap_or_else(|| CoworkerId::from_stored(""));
    may_use(state, account, &coworker).await?;
    let wake = match draft.kind.as_deref().unwrap_or("cron") {
        "cron" => cron_wake(state, draft.cron)?,
        "webhook" => {
            let key = crate::hooks::mint_webhook_key();
            Wake::Webhook {
                hook_id: crate::hooks::mint_hook_id(),
                secret_hash: crate::hooks::hash_webhook_key(&key),
                webhook_key: key,
            }
        }
        // NAMED, NOT ECHOED. Handing the caller's own bytes back is how a refusal becomes a
        // reflector: whatever they sent lands in our log line, their console and its renderer.
        _ => {
            let why = "kind must be \"cron\" or \"webhook\"";
            return Err(refused(StatusCode::UNPROCESSABLE_ENTITY, why));
        }
    };
    let sent = draft.run_limits.unwrap_or_default();
    let run_limits = limits(state, account, Some(&coworker), &sent).await?;
    let tz = match draft.tz {
        Some(tz) => tz,
        None => zone_of(state, account).await?,
    };
    let (prompt, at_ms) = (draft.prompt.unwrap_or_default(), now_ms());
    let events = Schedule::default()
        .decide(ScheduleCommand::Create {
            coworker_id: coworker,
            name: name_or_first_words(draft.name.as_deref(), &prompt),
            prompt,
            wake,
            run_limits,
            tz,
            at_ms,
        })
        .map_err(decided)?;
    let (id, after) = (ScheduleId::new(), Schedule::replay(&events));
    let store = &state.auth.store;
    let appended = store.append_schedule(&id, account, 0, &events, &after, at_ms);
    appended.await.map_err(storage)?;
    Ok(id)
}

/// A routine `account` owns, or the 404 that hides whether it exists: a wrong guess and somebody
/// else's real id must read the same, or the id space is enumerable.
pub(crate) async fn owned(
    state: &AgUiState,
    account: &AccountId,
    id: &ScheduleId,
) -> Result<Schedule, Refusal> {
    let unknown = || refused(StatusCode::NOT_FOUND, "no such schedule");
    match state.auth.store.schedule_owner(id).await {
        Ok(Some(owner)) if &owner == account => {}
        _ => return Err(unknown()),
    }
    let loaded = state.auth.store.load_schedule(id).await;
    loaded.map(|(schedule, _)| schedule).map_err(|_| unknown())
}

/// Edit a routine in place, `loaded` being it as `owned` read it: same id, same thread, same
/// history. THE WAKE IS REBUILT FROM THE ROUTINE, NEVER FROM THE DRAFT: a webhook's hook id, hash
/// and key come back from the aggregate unchanged — a prompt edit must not rotate the key somebody
/// pasted into another app. A handover is checked only when it is one: a client echoing the whole
/// row sends the coworker it already has, and a grant that lapsed must not stop a rename.
pub(crate) async fn edit(
    state: &AgUiState,
    (account, id): (&AccountId, &ScheduleId),
    loaded: &Schedule,
    draft: Draft,
) -> Result<Schedule, Refusal> {
    let unprocessable = |why: &str| Err(refused(StatusCode::UNPROCESSABLE_ENTITY, why));
    if draft
        .kind
        .as_deref()
        .is_some_and(|kind| kind != loaded.kind.as_str())
    {
        return unprocessable("a routine's kind cannot change; create a new routine instead");
    }
    if draft.cron.is_some() && loaded.kind == WakeKind::Webhook {
        return unprocessable("a webhook routine has no clock to set");
    }
    // An empty edit would still write an `Updated` event — and copy the hook's key into the log
    // again — while answering 200: a success that changed nothing reads as a save that worked.
    let Draft {
        coworker,
        name,
        prompt,
        cron,
        tz,
        run_limits,
        ..
    } = draft;
    let some = [
        name.is_some(),
        prompt.is_some(),
        cron.is_some(),
        tz.is_some(),
    ];
    if !some.contains(&true) && coworker.is_none() && run_limits.is_none() {
        return unprocessable(
            "nothing to change: send name, prompt, cron, tz, coworkerId or runLimits",
        );
    }
    if loaded.kind == WakeKind::Webhook && loaded.webhook_key.is_empty() {
        let why =
            "this webhook routine was made before its key was stored; rotate its key, then edit it";
        return Err(refused(StatusCode::CONFLICT, why));
    }
    let wake = cron.map(|cron| cron_wake(state, Some(cron))).transpose()?;
    let coworker = coworker.filter(|coworker| loaded.coworker_id.as_ref() != Some(coworker));
    if let Some(coworker) = &coworker {
        may_use(state, account, coworker).await?;
    }
    // Against the ceiling over the coworker the routine will have once this edit lands.
    let run_limits = match &run_limits {
        Some(sent) => {
            let whose = coworker.as_ref().or(loaded.coworker_id.as_ref());
            Some(limits(state, account, whose, sent).await?)
        }
        None => None,
    };
    let at_ms = now_ms();
    mutate_schedule(state, account, id, at_ms, |loaded| {
        let prompt = prompt.clone().unwrap_or_else(|| loaded.prompt.clone());
        let name = match name.as_deref() {
            Some(name) => name_or_first_words(Some(name), &prompt),
            None => loaded.name.clone(),
        };
        let wake = match (loaded.kind, &wake) {
            (WakeKind::Cron, Some(wake)) => wake.clone(),
            (WakeKind::Cron, None) => Wake::Cron {
                cron: loaded.cron.clone(),
            },
            (WakeKind::Webhook, _) => Wake::Webhook {
                hook_id: loaded.hook_id.clone(),
                secret_hash: loaded.secret_hash.clone(),
                webhook_key: loaded.webhook_key.clone(),
            },
        };
        let (coworker_id, tz) = (coworker.clone(), tz.clone());
        let edit = ScheduleCommand::Update {
            name,
            prompt,
            wake,
            coworker_id,
            run_limits,
            tz,
            at_ms,
        };
        loaded.decide(edit).map_err(decided)
    })
    .await
}

/// Pause, resume or delete a routine `account` owns: the aggregate's refusal (already paused,
/// deleted) is a 409. The aggregate after.
pub(crate) async fn change(
    state: &AgUiState,
    (account, id): (&AccountId, &ScheduleId),
    command: fn(i64) -> ScheduleCommand,
) -> Result<Schedule, Refusal> {
    let at_ms = now_ms();
    mutate_schedule(state, account, id, at_ms, |loaded| {
        loaded
            .decide(command(at_ms))
            .map_err(|why| refused(StatusCode::CONFLICT, why.to_string()))
    })
    .await
}

/// Load the schedule, decide with `decide`, append at the loaded seq — and if another writer got
/// there first, re-read and try ONCE more before answering 409. Why: the desktop's Routines pane
/// autosaves an edit on blur at the same instant a person clicks "Test run", so two mutations on
/// one schedule a few milliseconds apart are the ordinary case, not a race to design away. The
/// loser used to answer 500 "storage failed" (seen live 2 Sep 2026); now it decides again against
/// the winner's state, which is what the person meant anyway.
///
/// Every write to a routine comes through here — an edit, a tool's, run now, pause, resume,
/// delete, rotation, a skip, the webhook door and the clock sweep — because any two of them a
/// few milliseconds apart is exactly the race this retry exists for. The aggregate after.
pub(crate) async fn mutate_schedule<F>(
    state: &AgUiState,
    account_id: &AccountId,
    schedule_id: &ScheduleId,
    at_ms: i64,
    mut decide: F,
) -> Result<Schedule, Refusal>
where
    F: FnMut(&Schedule) -> Result<Vec<ScheduleEvent>, Refusal>,
{
    let lost = "another change to this routine landed first; reload and retry";
    for attempt in 0..2 {
        // NOT A 404. Every caller checked the routine exists; a load that fails now is the store
        // (down, or an event this binary cannot read), and "no such routine" would tell the
        // owner it was deleted.
        let (loaded, seq) = state
            .auth
            .store
            .load_schedule(schedule_id)
            .await
            .map_err(storage)?;
        let events = decide(&loaded)?;
        let mut after = loaded;
        for event in &events {
            after.apply(event);
        }
        let store = &state.auth.store;
        match store
            .append_schedule(schedule_id, account_id, seq, &events, &after, at_ms)
            .await
        {
            Ok(_) => return Ok(after),
            Err(opengrok_store::StoreError::Conflict) if attempt == 0 => {
                tracing::info!(schedule = %schedule_id, "a routine write lost a race; re-reading and retrying once");
            }
            Err(opengrok_store::StoreError::Conflict) => break,
            Err(error) => return Err(storage(error)),
        }
    }
    Err(refused(StatusCode::CONFLICT, lost))
}

/// A routine as its projection has it now, read back after a write: `nextDueMs` is whatever the
/// sweep will claim by, which is the projection's word, and a reply must agree with the next
/// `GET /schedules`.
pub(crate) async fn view_of(
    state: &AgUiState,
    account: &AccountId,
    id: &ScheduleId,
) -> Result<ScheduleView, Refusal> {
    let views = state
        .auth
        .store
        .schedules_for(account)
        .await
        .map_err(storage)?;
    let view = views.into_iter().find(|view| view.id == id.as_str());
    view.ok_or_else(|| storage(format!("routine {id} was not read back")))
}

/// The routine tools' desk (#316): a Bot's calls, answered as the session's account through the
/// functions above, in their words; `opengrok_tools::routine` has the tools' own.
pub(crate) struct Tools {
    pub state: AgUiState,
}

/// A desk's refusal, as a tool hands it to the model: the sentence.
fn said((_, why): Refusal) -> String {
    why
}

impl Tools {
    /// The person's own Bots that can take a routine: theirs, hired, not a group.
    async fn bots(&self, account: &AccountId) -> Result<Vec<Bot>, String> {
        let owned = self.state.auth.store.coworkers_for(account).await;
        let owned = owned.map_err(|error| said(storage(error)))?.into_iter();
        let plan = Some(opengrok_core::inference::SourceKind::LocalProxy);
        let bots = owned.filter(|bot| bot.members.is_empty());
        let bots = bots.map(|bot| (bot.id.to_string(), bot.name, bot.source == plan));
        Ok(routine::bots(bots.collect()))
    }

    /// A routine of the person's, or the contract's refusal for any other id.
    async fn routine(
        &self,
        context: &ToolContext,
        id: &str,
    ) -> Result<(ScheduleId, Schedule), String> {
        let id = ScheduleId::from_stored(id);
        match owned(&self.state, &context.account_id, &id).await {
            Ok(loaded) => Ok((id, loaded)),
            Err((StatusCode::NOT_FOUND, _)) => Err(routine::not_yours(id.as_str())),
            Err(refusal) => Err(said(refusal)),
        }
    }

    /// The routine as the tools carry it, read back from its projection after a write.
    async fn reply(&self, account: &AccountId, id: &ScheduleId) -> Result<Value, String> {
        let view = view_of(&self.state, account, id).await.map_err(said)?;
        Ok(routine::row(&view, &self.bots(account).await?))
    }
}

#[async_trait::async_trait]
impl RoutineDesk for Tools {
    async fn answer(&self, context: &ToolContext, ask: Ask) -> Result<Value, String> {
        let (state, account) = (&self.state, &context.account_id);
        let bots = self.bots(account).await?;
        let session = context.coworker_id.as_str();
        let draft = |fields: Fields, coworker: Option<&Bot>| Draft {
            coworker: coworker.map(|bot| CoworkerId::from_stored(bot.id.clone())),
            name: fields.name,
            prompt: fields.prompt,
            cron: fields.when,
            tz: fields.tz,
            ..Draft::default()
        };
        match ask {
            Ask::List => {
                let views = state.auth.store.schedules_for(account).await;
                let views = views.map_err(|error| said(storage(error)))?;
                Ok(json!(
                    views
                        .iter()
                        .map(|view| routine::row(view, &bots))
                        .collect::<Vec<_>>()
                ))
            }
            Ask::Create(fields) => {
                let bot = routine::resolve(&bots, session, fields.bot.as_deref())?;
                let id = create(state, account, draft(fields, Some(bot)))
                    .await
                    .map_err(said)?;
                let mut row = self.reply(account, &id).await?;
                // A Bot on the person's own plan runs a routine only while the plan can answer.
                if bot.on_plan {
                    let setting = opengrok_harness::local_proxy::Saved::setting(state, account);
                    let plan = Some(opengrok_core::inference::SourceKind::LocalProxy);
                    let by_mac = setting
                        .await
                        .is_some_and(|setting| setting.by_mac(None, plan));
                    row["note"] = json!(routine::ONLY_WHILE[usize::from(!by_mac)]);
                }
                Ok(row)
            }
            Ask::Update {
                routine: asked,
                mut fields,
            } => {
                let (id, loaded) = self.routine(context, &asked).await?;
                let bot = fields.bot.take();
                let bot = bot.map(|bot| routine::resolve(&bots, session, Some(&bot)).cloned());
                let (active, at) = (fields.active.take(), (account, &id));
                if fields != Fields::default() || bot.is_some() {
                    let bot = bot.transpose()?;
                    edit(state, at, &loaded, draft(fields, bot.as_ref()))
                        .await
                        .map_err(said)?;
                }
                // Already where it was asked to be is no change, and no refusal.
                let command: Option<fn(i64) -> ScheduleCommand> = match active {
                    Some(false) if !loaded.paused => Some(|at_ms| ScheduleCommand::Pause { at_ms }),
                    Some(true) if loaded.paused => Some(|at_ms| ScheduleCommand::Resume { at_ms }),
                    _ => None,
                };
                if let Some(command) = command {
                    change(state, at, command).await.map_err(said)?;
                }
                self.reply(account, &id).await
            }
            Ask::Delete { routine: asked } => {
                let (id, loaded) = self.routine(context, &asked).await?;
                let delete = |at_ms| ScheduleCommand::Delete { at_ms };
                change(state, (account, &id), delete).await.map_err(said)?;
                Ok(json!({ "deleted": id.as_str(), "name": loaded.name }))
            }
        }
    }

    async fn stored_name(&self, context: &ToolContext, id: &str) -> Result<String, String> {
        Ok(self.routine(context, id).await?.1.name)
    }
}
