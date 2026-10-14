//! Durable event journaling and live attachment.

use super::*;

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
    /// How hard this turn thinks, captured with the pin and for the same reason.
    pub effort: Effort,
    /// Where this turn asks, and on the proxy which way, captured with the pin for its reason.
    pub inference_source: TurnSource,
    /// The composed system message this turn opened with, captured for the same reason as the
    /// pin: a role edited while a person answered an approval card must not change the coworker
    /// halfway through the turn.
    pub system: Option<String>,
    /// The skill quoted into `system`, recorded so the next message on this thread
    /// can reuse it when the client sends no skill id.
    pub skill_id: Option<String>,
    /// The attached skills `system` lists (`RunEvent::Started::offered_skills`): what a resume
    /// of this run offers `use_skill` for.
    pub offered_skills: Vec<OfferedSkill>,
    /// What the person asked this turn (`RunEvent::Started::prompt`). Only a turn that starts the
    /// run writes it; a resume finds the run already started and passes `None`.
    pub prompt: Option<Vec<serde_json::Value>>,
    /// What this run may spend (`RunEvent::Started::limits`), written by the write that starts it.
    /// A resume finds the run started, and is held to what it captured instead.
    pub limits: RunLimits,
    /// The run's generation when this loop began (#91): 0 for a new turn, the run's own for a
    /// continuation. THE FENCE: a write is refused once the run has moved on — resumed by the
    /// sweep while this loop was still alive after a lapsed lease — so the replaced loop can
    /// neither act again nor end the run that carried on (`RunLifecycle.tla` FenceAllWrites).
    pub generation: u32,
}

#[async_trait::async_trait]
impl opengrok_harness::RunJournal for StoreJournal {
    async fn record(
        &self,
        run_id: &str,
        events: &[Event],
    ) -> Result<(), opengrok_harness::JournalError> {
        self.record_spent(run_id, events, &opengrok_core::run::RoundSpent::default())
            .await
    }

    async fn record_spent(
        &self,
        run_id: &str,
        events: &[Event],
        spent: &opengrok_core::run::RoundSpent,
    ) -> Result<(), opengrok_harness::JournalError> {
        append_events(self, run_id, events, spent)
            .await
            .map_err(|error| match error {
                AppendError::Ended => opengrok_harness::JournalError::Ended(format!(
                    "run {run_id} ended before its card could be recorded"
                )),
                AppendError::Fenced => opengrok_harness::JournalError::Fenced(format!(
                    "run {run_id} was resumed by another loop"
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

    /// `ToolStarted` in the run's log, before the tool runs (#91). The aggregate refuses it on a
    /// run that has ended, which answers `Ended`: the loop stops, as `stopped` would have told it
    /// at its next boundary.
    ///
    /// RETRIED ON ANY STORE ERROR, unlike a round: this only adds calls to a set, so a second
    /// copy after a lost reply changes nothing, and a blip would otherwise fail the run at every
    /// tool round. It costs a replay (`load_run`) the projection could answer in one statement.
    async fn tools_starting(
        &self,
        run_id: &str,
        tools: &[opengrok_core::run::StartedTool],
    ) -> Result<(), opengrok_harness::JournalError> {
        self.bookkeep(run_id, "its tool could start", |at_ms| {
            RunCommand::StartTools {
                tools: tools.to_vec(),
                at_ms,
            }
        })
        .await
    }
}

impl StoreJournal {
    /// A log-only write the loop makes before a round's tools run (#91): fenced by generation
    /// like every journal write, refused on an ended run, and tried `APPEND_ATTEMPTS` times.
    async fn bookkeep(
        &self,
        run_id: &str,
        before: &str,
        command: impl Fn(i64) -> RunCommand,
    ) -> Result<(), opengrok_harness::JournalError> {
        let id = RunId::from_stored(run_id.to_string());
        let mut last_error = String::new();
        for _ in 0..APPEND_ATTEMPTS {
            let (mut run, seq) = match self.state.auth.store.load_run(&id).await {
                Ok(loaded) => loaded,
                Err(error) => {
                    last_error = error.to_string();
                    continue;
                }
            };
            if run.generation != self.generation {
                return Err(opengrok_harness::JournalError::Fenced(format!(
                    "run {run_id} was resumed by another loop"
                )));
            }
            let at_ms = now_ms();
            let events = match run.decide(command(at_ms)) {
                Ok(events) => events,
                Err(opengrok_core::run::RunError::AlreadyEnded) => {
                    return Err(opengrok_harness::JournalError::Ended(format!(
                        "run {run_id} ended before {before}"
                    )));
                }
                Err(error) => {
                    return Err(opengrok_harness::JournalError::Unwritable(
                        error.to_string(),
                    ));
                }
            };
            for event in &events {
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
                .append_run(&id, seq, &events, &view, self.account_id.as_ref())
                .await
            {
                Ok(_) => return Ok(()),
                Err(error) => last_error = error.to_string(),
            }
        }
        Err(opengrok_harness::JournalError::Unwritable(last_error))
    }
}

impl StoreJournal {
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
            .decide(start_command(self, at_ms))
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

/// The `Start` a run records about itself: whose turn, and what it opened with — the journal's
/// own fields, which are one fact and only ever travel together.
fn start_command(start: &StoreJournal, at_ms: i64) -> RunCommand {
    let trimmed = |text: &Option<String>| {
        let text = text.as_deref().map(str::trim);
        text.filter(|text| !text.is_empty()).map(str::to_string)
    };
    RunCommand::Start {
        thread_id: start.thread_id.clone(),
        coworker_id: start.coworker_id.clone(),
        model: trimmed(&start.model),
        effort: start.effort,
        inference_source: start.inference_source,
        system: start.system.clone().filter(|text| !text.is_empty()),
        skill_id: trimmed(&start.skill_id),
        offered_skills: start.offered_skills.clone(),
        prompt: start.prompt.clone(),
        limits: start.limits,
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
    start: &StoreJournal,
    run_id: &str,
    events: &[Event],
    spent: &opengrok_core::run::RoundSpent,
) -> Result<(), AppendError> {
    for _ in 1..APPEND_ATTEMPTS {
        match append_events_once(start, run_id, events, spent).await {
            Err(AppendError::Store(opengrok_store::StoreError::Conflict)) => continue,
            other => return other,
        }
    }
    append_events_once(start, run_id, events, spent).await
}

/// Why a batch was not written.
#[derive(Debug)]
enum AppendError {
    Store(opengrok_store::StoreError),
    /// The batch parks, and the run it parks has ended: the Stop won the race the loop's last
    /// `stopped` question could not see. Nothing was written (`RunJournal::record`).
    Ended,
    /// The run was resumed into a newer generation than this loop's (#91). Nothing was written.
    Fenced,
}

impl From<opengrok_store::StoreError> for AppendError {
    fn from(error: opengrok_store::StoreError) -> Self {
        Self::Store(error)
    }
}

/// One attempt: read the run, decide what this batch appends, write it at the seq it read. `start`
/// is what the run records about itself at its first batch.
async fn append_events_once(
    start: &StoreJournal,
    run_id: &str,
    events: &[Event],
    spent: &opengrok_core::run::RoundSpent,
) -> Result<(), AppendError> {
    let (state, thread_id, account_id) =
        (&start.state, &start.thread_id, start.account_id.as_ref());
    if events.is_empty() {
        return Ok(());
    }
    let run_id = RunId::from_stored(run_id.to_string());
    let at_ms = now_ms();

    let (mut run, seq) = state.auth.store.load_run(&run_id).await?;
    if run.started && run.generation != start.generation {
        return Err(AppendError::Fenced);
    }
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

    // WHAT THE ROUND SPENT GOES DOWN WITH ITS FRAMES (#256), ahead of them in the same append:
    // the result that closes a recipe's call and the record that it played land together, or
    // neither does. Refused only on a run that has ended, whose frames are refused below too.
    if !spent.is_empty()
        && let Ok(recorded) = run.decide(RunCommand::RecordSpent {
            spent: spent.clone(),
            at_ms,
        })
    {
        for event in &recorded {
            run.apply(event);
        }
        to_append.extend(recorded);
    }

    for event in events {
        // The card's sentence goes into the log with the card, so a replay carries it (#249).
        let mut stamped = event.clone();
        crate::cards::stamp_summary(&mut stamped);
        let event = &stamped;
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
                Some(
                    run.decide(RunCommand::Finish {
                        at_ms,
                        reason: event
                            .extra
                            .get("reason")
                            .and_then(serde_json::Value::as_str)
                            .and_then(opengrok_core::run::FinishReason::parse),
                    }),
                )
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
    crate::pairs::ended(state, thread_id, view.status);
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
pub(super) async fn answer_for_existing_run(
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
pub(super) fn not_attached(owned: Result<bool, opengrok_store::StoreError>) -> Response {
    match owned {
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
        Ok(_) => run_taken(),
    }
}

pub(super) fn run_taken() -> Response {
    (
        StatusCode::CONFLICT,
        // `error` IS THE SENTENCE, as on every other refusal a client shows; the word a client
        // branches on is `code` (NativeChat, reading the wire corpus, showed "run-exists").
        Json(serde_json::json!({
            "error": "this run id already has a run; a new turn needs a new run id",
            "code": "run-exists",
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
            let window = run_time_window(&run.emitted);
            let frames = log_frames(
                events_for_client(&state, &account_id, &run, window).await,
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
pub(super) fn log_frames(
    mut hydrated: Vec<serde_json::Value>,
    logged: usize,
) -> Vec<serde_json::Value> {
    hydrated.truncate(logged);
    hydrated
}

/// The closer an attached stream ends with, from what the run IS rather than from the last closer
/// its log happens to hold. `last` is the log's last frame and `stop_notice` whether its
/// `run-stopped` was already sent.
pub(super) fn attached_closer(
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
