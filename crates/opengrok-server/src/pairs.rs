//! Bots messaging each other (#314): `message_bot`'s server half, and the receiving Bot's turn in
//! the side thread the two share (`formal/tla/PairDelivery.tla` maps each step to its function).
//!
//! A CALL WRITES; A DRAIN STARTS. `Mail::send` writes the call's messages to the outbox, one row
//! per receiver, with the sender's `messaged` row, in one transaction, and asks each pair to drain.
//! A drain claims the pair's next message, unless one of its runs is still in flight, and starts
//! that message's run as `autonomy::fire` starts a routine's: as the receiving Bot, for its owner.
//! Drains are asked for when a message is written (`drain_soon`), when a pair's run ends (`ended`)
//! and on every recovery sweep (`sweep`), which also starts a claimed message whose drain died.
//!
//! WHO MAY SEND, AND HOW DEEP, IS THE STORE'S, NEVER THE CALL'S (CLAUDE.md #7). The sender is the
//! turn's coworker, the person its owner, the chain and the hop are read from the outbox row that
//! started the sending run, and the caps are counted from the outbox under the owner's lock.

use std::sync::Arc;

use opengrok_core::id::{AccountId, CoworkerId, RunId};
use opengrok_harness::ToolRunner;
use opengrok_store::BotMessageRow;
use opengrok_store::pairs::{Receiver, Send};
use opengrok_tools::message_bot::{self, BotMail, BotOffer, Delivered, MESSAGE_BOT};
use opengrok_wire::pair;
use serde_json::Value;

use crate::agui::routes::{AgUiState, StoreJournal, now_ms};
use crate::host_state::HostState;

pub use crate::seams::message_bot_runner;

/// How long a claimed message may wait for its run before the sweep starts it: past this its drain
/// died with its process, or is that slow, and the run's id makes a second start a no-op.
const START_GRACE_MS: i64 = crate::recovery::LEASE_MS;

/// Where a chat turn's frames go live: the sender's stream, for its `messaged` row.
pub(crate) type Live = tokio::sync::mpsc::UnboundedSender<opengrok_wire::agui::Event>;

/// What a call is refused in when its words cannot be written.
const UNWRITTEN: &str =
    "the message could not be written down just now, so nothing was sent; try again in a moment";

/// The person's Bots a message can reach, `(id, name)`: their own, hired, not a room.
async fn roster(state: &AgUiState, person: &AccountId) -> Result<Vec<(String, String)>, String> {
    let rows = state.auth.store.coworkers_for(person).await;
    let rows = rows.map_err(|error| error.to_string())?;
    let bots = rows.into_iter().filter(|bot| bot.members.is_empty());
    Ok(bots
        .map(|bot| (bot.id.as_str().to_string(), bot.name))
        .collect())
}

/// Everything a send needs, asked whenever it is offered and again at every call: the roster, and
/// the hop and chain the sending run writes at. `Err` is the sentence a call is refused in. The hop
/// is the caller's to judge: at the limit the tool is not advertised, and a call is refused.
async fn sending(
    state: &AgUiState,
    person: &AccountId,
    sender: &CoworkerId,
    run_id: Option<&str>,
) -> Result<(Vec<(String, String)>, i32, String), String> {
    let roster = roster(state, person)
        .await
        .map_err(|_| UNWRITTEN.to_string())?;
    if !roster.iter().any(|(id, _)| id == sender.as_str()) || roster.len() < 2 {
        return Err("only another Bot of your person's can be messaged".to_string());
    }
    let ceiling = state.auth.store.ceiling_at(sender).await;
    let (ceiling, _) = ceiling.map_err(|_| UNWRITTEN.to_string())?;
    if !ceiling.allows(MESSAGE_BOT) {
        let why = "messaging your person's other Bots is switched off for you, so nothing was sent";
        return Err(why.to_string());
    }
    let incoming = match run_id {
        Some(run) => state.auth.store.bot_message_of_run(run).await,
        None => Ok(None),
    };
    let incoming = incoming.map_err(|_| UNWRITTEN.to_string())?;
    let hop = incoming.as_ref().map_or(0, |row| row.hop);
    let chain = match (incoming, run_id) {
        (Some(row), _) => row.chain_id,
        (None, run) => format!("chain-{}", run.unwrap_or_default()),
    };
    Ok((roster, hop, chain))
}

/// Whether a run whose own message had `hop` may send: under `MAX_HOPS` (Lean `Chain`).
fn under_the_limit(hop: i32) -> bool {
    u32::try_from(hop).is_ok_and(|hop| hop < message_bot::MAX_HOPS)
}

/// The cap's refusal, the contract's words.
fn capped(person: &str) -> String {
    format!(
        "this exchange between your Bots has reached its limit, so nothing was sent; tell {person} \
         in your main chat instead"
    )
}

/// `runner`, offering `message_bot` when `person` owns `sender`, its ceiling allows it and there is
/// another Bot to name, advertised while the run is under `MAX_HOPS`; a turn with no runner gets
/// one for it. `run` is the sending run (none for a listing); `live`, the stream its `messaged`
/// row goes out on.
pub(crate) async fn onto(
    state: &AgUiState,
    (person, sender): (&AccountId, &CoworkerId),
    run: Option<&str>,
    live: Option<Live>,
    runner: Option<ToolRunner>,
) -> Option<ToolRunner> {
    let Ok((roster, hop, _)) = sending(state, person, sender, run).await else {
        return runner;
    };
    let mail = Mail {
        state: state.clone(),
        person: person.clone(),
        sender: sender.clone(),
        run_id: run.unwrap_or_default().to_string(),
        live,
    };
    let runner = runner.unwrap_or_else(ToolRunner::local_only);
    let offers = message_bot::offers(&roster);
    let me = (sender.to_string(), under_the_limit(hop));
    Some(runner.with_bots(offers, me, Arc::new(mail)))
}

/// Where one run's calls are written: as `person`, from `sender`, by `run_id`.
struct Mail {
    state: AgUiState,
    person: AccountId,
    sender: CoworkerId,
    run_id: String,
    live: Option<Live>,
}

#[async_trait::async_trait]
impl BotMail for Mail {
    async fn send(
        &self,
        call_id: &str,
        to: &[BotOffer],
        message: &str,
    ) -> Result<Vec<Delivered>, String> {
        let (state, sender) = (&self.state, self.sender.as_str());
        let run = Some(self.run_id.as_str()).filter(|run| !run.is_empty());
        let (roster, hop, chain) = sending(state, &self.person, &self.sender, run).await?;
        if !under_the_limit(hop) {
            return Err(capped(&crate::persona::caller(state, &self.person).await));
        }
        // A Bot retired or given away since the turn began is no longer the person's to message.
        if let Some(gone) = to
            .iter()
            .find(|bot| !roster.iter().any(|(id, _)| *id == bot.id))
        {
            let offers = message_bot::offers(&roster);
            let still = offers.iter().filter(|bot| bot.id != sender);
            let still: Vec<&str> = still.map(|bot| bot.label.as_str()).collect();
            let (gone, still) = (&gone.label, still.join(", "));
            return Err(format!(
                "no Bot of yours is called \"{gone}\"; you can message: {still}"
            ));
        }
        let ids: Vec<(String, String, String)> = to
            .iter()
            .map(|bot| {
                let fresh = uuid::Uuid::now_v7();
                (
                    pair::pair_thread(sender, &bot.id),
                    format!("bm_{fresh}"),
                    RunId::new().to_string(),
                )
            })
            .collect();
        let receivers: Vec<Receiver<'_>> = to
            .iter()
            .zip(&ids)
            .map(|(bot, (thread, message_id, run_id))| Receiver {
                receiver_id: &bot.id,
                thread_id: thread,
                message_id,
                run_id,
            })
            .collect();
        let named: Vec<pair::Messaged<'_>> = to
            .iter()
            .zip(&ids)
            .map(|(bot, (thread, _, _))| pair::Messaged {
                coworker_id: &bot.id,
                name: &bot.name,
                thread_id: thread,
            })
            .collect();
        let (entry_id, at_ms) = (format!("tl_{}", uuid::Uuid::now_v7()), now_ms());
        let entry = pair::messaged_entry(&entry_id, at_ms, sender, &named, &self.run_id);
        let send = Send {
            owner_id: self.person.as_str(),
            sender_id: sender,
            sender_run_id: &self.run_id,
            call_id,
            chain_id: &chain,
            hop: hop + 1,
            body: message,
            to: &receivers,
            entry: (&entry_id, &entry),
            caps: (message_bot::PER_CHAIN, message_bot::PER_HOUR),
            at_ms,
        };
        let rows = match state.auth.store.enqueue_bot_messages(&send).await {
            Ok(Some(rows)) => rows,
            Ok(None) => return Err(capped(&crate::persona::caller(state, &self.person).await)),
            Err(error) => {
                tracing::warn!(%error, sender, "a Bot's message could not be written");
                return Err(UNWRITTEN.to_string());
            }
        };
        // Live only when this call wrote the row: one carried out again has the first one's.
        let written = rows.iter().any(|(_, fresh)| *fresh);
        if let Some(live) = self.live.as_ref().filter(|_| written) {
            let thread = pair::chat_thread(sender);
            let frame = pair::timeline_frame(pair::TIMELINE_CREATED, &thread, &entry, at_ms);
            let _ = live.send(frame);
        }
        Ok(rows
            .into_iter()
            .map(|(row, _)| {
                drain_soon(state, &row.thread_id);
                Delivered {
                    coworker_id: row.receiver_id,
                    thread_id: row.thread_id,
                    message_id: row.id,
                }
            })
            .collect())
    }
}

/// Ask a pair to start its next message, beside whatever asked: a call, a run's ending, the sweep.
pub(crate) fn drain_soon(state: &AgUiState, thread_id: &str) {
    let host = HostState::new(state.clone(), None);
    tokio::spawn(drain(host, thread_id.to_string()));
}

/// `claim_pair_message`, then the claimed message's turn. A pair whose run is in flight is left:
/// that run's ending drains it again. A store that does not answer leaves it to the sweep.
async fn drain(host: HostState, thread_id: String) {
    match host
        .agui
        .auth
        .store
        .claim_pair_message(&thread_id, now_ms())
        .await
    {
        Ok(Some(row)) => fire(host, row).await,
        Ok(None) => {}
        Err(error) => tracing::warn!(%error, thread = thread_id, "a pair could not be drained"),
    }
}

/// A pair's run ended, by its loop, a Stop or the sweep: its next message may start.
pub(crate) fn ended(state: &AgUiState, thread_id: &str, status: opengrok_core::run::RunStatus) {
    if pair::is_pair_thread(thread_id) && status.is_terminal() {
        drain_soon(state, thread_id);
    }
}

/// The recovery sweep's pass: every pair with a message waiting, and every claimed message whose
/// run never began. A store that does not answer is the next sweep's.
pub(crate) async fn sweep(host: &HostState) {
    let due = host
        .agui
        .auth
        .store
        .pairs_to_sweep(now_ms(), START_GRACE_MS);
    let (threads, stalled) = match due.await {
        Ok(due) => due,
        Err(error) => return tracing::warn!(%error, "the pairs could not be swept"),
    };
    for row in stalled {
        tokio::spawn(fire(host.clone(), row));
    }
    for thread in threads {
        tokio::spawn(drain(host.clone(), thread));
    }
}

/// The message's turn, as the receiving Bot for its owner, in the pair's thread.
async fn fire(host: HostState, row: BotMessageRow) {
    let firing = crate::autonomy::Firing {
        origin: format!("message {}", row.id),
        account_id: AccountId::from_stored(row.owner_id.clone()),
        coworker_id: CoworkerId::from_stored(row.receiver_id.clone()),
        prompt: row.body.clone(),
        thread_id: row.thread_id.clone(),
        run_id: RunId::from_stored(row.run_id.clone()),
        run_limits: Default::default(),
        message: Some(row),
    };
    crate::autonomy::fire(host, firing).await;
}

/// What a message's turn opens with beyond its coworker: the side thread's line for its system
/// message, and what it is asked (`history::for_message`), or `None` when the message cannot be
/// quoted. `person` is what its owner is called.
pub(crate) async fn opening(
    state: &AgUiState,
    row: &BotMessageRow,
    person: &str,
) -> Option<(String, Vec<opengrok_harness::ChatMessage>)> {
    let sender = CoworkerId::from_stored(row.sender_id.clone());
    let peer = match state.auth.store.load_coworker(&sender).await {
        Ok((bot, _)) => bot.name,
        Err(_) => "another Bot".to_string(),
    };
    let owner = AccountId::from_stored(row.owner_id.clone());
    let said = crate::agui::history::for_message(state, &owner, row, (&peer, person)).await?;
    Some((
        crate::persona::message_line(&peer, person, chrono::Utc::now()),
        said,
    ))
}

/// `refuse` for a firing that carries a message; nothing for a routine's.
pub(crate) async fn refused(state: &AgUiState, message: Option<&BotMessageRow>, why: String) {
    if let Some(row) = message {
        refuse(state, row, &why).await;
    }
}

/// What the message's run journals as asked: its words as the person reads them in the thread,
/// under the run's own id (`routine_prompt`, so it carries on as a routine does), with the sender.
pub(crate) fn prompt(row: &BotMessageRow, run_id: &RunId) -> Vec<Value> {
    let mut asked = opengrok_core::run::routine_prompt(run_id, &row.body);
    if let Some(Value::Object(said)) = asked.first_mut() {
        said.insert(
            "fromCoworkerId".to_string(),
            Value::from(row.sender_id.clone()),
        );
        said.insert("callId".to_string(), Value::from(row.call_id.clone()));
    }
    asked
}

/// A message that cannot be had: its run, started and failed at once, saying why in the pair's
/// thread, which is how the person sees it was refused. Its ending drains the pair as any does.
pub(crate) async fn refuse(state: &AgUiState, row: &BotMessageRow, why: &str) {
    let run_id = RunId::from_stored(row.run_id.clone());
    let journal = StoreJournal {
        state: state.clone(),
        thread_id: row.thread_id.clone(),
        account_id: Some(AccountId::from_stored(row.owner_id.clone())),
        coworker_id: Some(CoworkerId::from_stored(row.receiver_id.clone())),
        model: None,
        effort: Default::default(),
        inference_source: Default::default(),
        system: None,
        skill_id: None,
        offered_skills: Vec::new(),
        prompt: Some(prompt(row, &run_id)),
        limits: Default::default(),
        generation: 0,
    };
    if !matches!(journal.claim(run_id.as_str()).await, Ok(true)) {
        return;
    }
    let mut projection = opengrok_harness::Projection::new(&row.thread_id, &row.run_id, now_ms());
    let mut frames = projection.start();
    frames.extend(projection.fail(why));
    use opengrok_harness::RunJournal as _;
    if let Err(error) = journal.record(run_id.as_str(), &frames).await {
        tracing::warn!(%error, run = %run_id, "a refused message's run could not be written");
    }
}

/// How many timeline rows a Bot's main chat replays: its newest, as many as a thread's runs.
const TIMELINE_ROWS: i64 = 100;

/// A Bot's timeline rows for `gateway-{bot}`, for anyone who may use that Bot; nothing for any
/// other thread, or a Bot the caller may not see.
pub(crate) async fn timeline(
    state: &AgUiState,
    account: &AccountId,
    thread_id: &str,
) -> Result<Vec<Value>, opengrok_store::StoreError> {
    let Some(bot) = pair::chat_of(thread_id).map(CoworkerId::from_stored) else {
        return Ok(Vec::new());
    };
    let policy = state.auth.store.policy_to_use(account, &bot).await?;
    let using = opengrok_policy::Action::UseCoworker;
    if !opengrok_policy::decide(account, &bot, using, &policy).is_allowed() {
        return Ok(Vec::new());
    }
    state.auth.store.timeline(bot.as_str(), TIMELINE_ROWS).await
}

/// Who a thread is between, `[{id, name}]`: a pair's two Bots, else the Bots its runs were, a
/// Bot's main chat naming that Bot first.
pub(crate) async fn coworkers(state: &AgUiState, thread_id: &str, ran: Vec<String>) -> Vec<Value> {
    let ids: Vec<String> = match (pair::pair_peers(thread_id), pair::chat_of(thread_id)) {
        (Some((lo, hi)), _) => vec![lo, hi],
        (None, Some(bot)) => std::iter::once(bot.to_string()).chain(ran).collect(),
        (None, None) => ran,
    };
    let mut seen = std::collections::BTreeSet::new();
    let mut named = Vec::new();
    for id in ids.into_iter().filter(|id| seen.insert(id.clone())) {
        let bot = state
            .auth
            .store
            .load_coworker(&CoworkerId::from_stored(id.clone()))
            .await;
        let name = bot.map(|(bot, _)| bot.name).unwrap_or_default();
        named.push(serde_json::json!({ "id": id, "name": name }));
    }
    named
}
