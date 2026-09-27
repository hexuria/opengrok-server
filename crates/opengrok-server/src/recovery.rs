//! Picking up runs that a restart abandoned.
//!
//! "A COWORKER KEEPS WORKING WHEN YOU CLOSE THE TAB" IS ONLY HALF TRUE UNTIL THIS EXISTS. A run
//! whose process died is durable — every event reached the log — but durable is not the same as
//! *continuing*. Without this, an interrupted run sits at `running` forever: the work is safe, and
//! nobody is doing it.
//!
//! HOW A RESTART IS TOLD FROM A RUN THAT IS SIMPLY STILL GOING: a lease. A live process pushes the
//! expiry out as it works; a dead one cannot. Anything whose lease has passed had no process behind
//! it when the clock ran out. Claiming is one `update … returning`, so two replicas booting
//! together cannot both take the same run.
//!
//! THE HONEST PART, AND THE REASON THIS FILE IS NOT SHORTER. A run interrupted *between* a tool
//! call and its result is genuinely ambiguous: the command may have run, may have half-run, may
//! never have started. We cannot know, and re-running it would repeat whatever it did. So we do not
//! guess — the model is told plainly that the call's outcome is unknown, and it decides. A resumed
//! run that silently re-ran a `rm` would be worse than one that stopped.

use std::sync::Arc;
use std::time::Duration;

use opengrok_core::id::RunId;
use opengrok_core::run::{RunCommand, RunStatus, RunView};

use crate::agui::routes::AgUiState;

/// How long a claim is good for. Long enough that a slow model call does not lose its own run,
/// short enough that a crash is picked up while somebody still cares.
pub const LEASE_MS: i64 = 60_000;

/// How often to look for abandoned runs.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// How many to take at once, so one replica cannot swallow every orphan on a bad day.
const CLAIM_LIMIT: i64 = 10;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Sweep forever. Started by the binary; stops when the process does. Takes the host state
/// because a run it carries on can park on a card, and a card is minted through it.
pub async fn sweep_forever(state: crate::host_state::HostState) {
    // A first sweep immediately: the most likely moment to find an abandoned run is just after the
    // restart that abandoned it.
    loop {
        if let Err(error) = sweep_once(&state).await {
            // A failed sweep is not fatal. The runs stay claimable and the next sweep tries again;
            // taking the process down over it would turn a database hiccup into an outage.
            tracing::warn!(%error, "a recovery sweep failed; will try again");
        }
        tokio::time::sleep(SWEEP_INTERVAL).await;
    }
}

/// Claim what has been abandoned and resolve each one.
pub async fn sweep_once(
    host: &crate::host_state::HostState,
) -> Result<usize, opengrok_store::StoreError> {
    let state = &host.agui;
    let claimed = state
        .auth
        .store
        .claim_abandoned_runs(now_ms(), LEASE_MS, CLAIM_LIMIT)
        .await?;

    if claimed.is_empty() {
        return Ok(0);
    }
    tracing::info!(count = claimed.len(), "picking up runs a restart abandoned");

    let mut resolved = 0;
    for run_id in claimed {
        match resolve(host, &run_id).await {
            Ok(()) => resolved += 1,
            Err(error) => {
                // Left claimed; the lease expires and a later sweep tries again. A run that cannot
                // be resolved must not be silently dropped.
                tracing::warn!(run = %run_id, %error, "could not resolve an abandoned run");
            }
        }
    }
    Ok(resolved)
}

/// What the sweep does with a run a restart abandoned (#91).
#[derive(Debug, PartialEq)]
pub(crate) enum Verdict {
    /// Carry it on in its next generation. `Some` is a person's answer whose call never started:
    /// the resume must carry that answer out, not ask the model again.
    Resume(Option<opengrok_core::run::AnsweredCall>),
    /// End it, saying why.
    Fail(String),
}

/// Resume or fail, from the log alone (`formal/tla/RunLifecycle.tla` Recover).
///
/// A LOG FROM BEFORE TOOL STARTS WERE JOURNALED cannot prove no tool was in flight: its round's
/// `TOOL_CALL_START` with no result is the ambiguous case this file always failed, and still
/// does. A run whose start of a tool is on record without the tool's result fails the same way,
/// naming the tool (`RunError::ToolOutcomeUnknown`). Everything else was interrupted between two
/// steps — nothing may have acted that the log does not show — and is carried on, at most
/// `MAX_RESUMES` times.
pub(crate) fn verdict(run: &opengrok_core::run::Run) -> Verdict {
    let in_flight = |tools: &str| {
        format!(
            "this run was interrupted by a restart while `{tools}` was in flight; \
             whether it completed is unknown, so it was not run again"
        )
    };
    if let Some(call) = unresolved_tool_call(run) {
        return Verdict::Fail(in_flight(&call));
    }
    match run.decide(RunCommand::Resume {
        reason: String::new(),
        at_ms: 0,
    }) {
        Ok(_) => Verdict::Resume(run.unstarted_answer.clone()),
        Err(opengrok_core::run::RunError::ToolOutcomeUnknown(tools)) => {
            Verdict::Fail(in_flight(&tools.join("`, `")))
        }
        Err(opengrok_core::run::RunError::ResumedTooOften) => Verdict::Fail(format!(
            "this run was interrupted by a restart again after being carried on {} times, \
             so it was not carried on again",
            opengrok_core::run::MAX_RESUMES
        )),
        Err(other) => Verdict::Fail(format!(
            "this run was interrupted by a restart and could not continue: {other}"
        )),
    }
}

/// Bring one abandoned run to an ending, or carry it on.
///
/// A run that stays `running` is one a person watches forever: whichever the verdict, it is
/// settled here — resumed under a new generation, or failed with the reason.
async fn resolve(
    host: &crate::host_state::HostState,
    run_id: &RunId,
) -> Result<(), opengrok_store::StoreError> {
    let state = &host.agui;
    let (run, seq) = state.auth.store.load_run(run_id).await?;

    // Already settled by somebody else between the claim and now.
    //
    // A STOPPED RUN COUNTS AS SETTLED, and this line is what makes a stop a stop. The projection
    // already keeps stopped runs out of `claim_abandoned_runs` (it claims `status = 'running'`
    // only), so reaching here with one means the claim and the stop crossed — and failing it now
    // would rewrite somebody's deliberate stop as "interrupted by a restart", which is both untrue
    // and exactly the answer the person was trying not to get.
    if run.status.is_terminal() {
        return Ok(());
    }
    // Waiting on a person is not abandonment — it is the run doing exactly what it should.
    if run.status == RunStatus::AwaitingApproval {
        return Ok(());
    }

    let answered = match verdict(&run) {
        Verdict::Fail(reason) => return fail_run(state, run_id, run, seq, &reason).await,
        Verdict::Resume(answered) => answered,
    };
    // Carried on only for an owner and a coworker that can still take work: a resume on a
    // retired coworker's key would bill the deployment, and a run nobody owns has no one to
    // continue for.
    let owner = state.auth.store.run_account(run_id).await?;
    let coworker = run.coworker_id.clone();
    let (account_id, coworker_id) = match (owner, coworker) {
        (Some(account), Some(coworker))
            if crate::autonomy::routes::takes_work(state, &coworker).await =>
        {
            (account, coworker)
        }
        _ => {
            let reason = "this run was interrupted by a restart and could not be carried on: \
                          its coworker can no longer take work";
            return fail_run(state, run_id, run, seq, reason).await;
        }
    };

    let at_ms = now_ms();
    let mut run = run;
    let resumed = run
        .decide(RunCommand::Resume {
            reason: "interrupted by a restart".to_string(),
            at_ms,
        })
        .map_err(|error| opengrok_store::StoreError::Corrupt(error.to_string()))?;
    for event in &resumed {
        run.apply(event);
    }
    let view = RunView {
        id: run_id.clone(),
        thread_id: run.thread_id.clone(),
        status: run.status,
        event_count: run.emitted.len() as i64,
        updated_at_ms: at_ms,
    };
    state
        .auth
        .store
        .append_run(run_id, seq, &resumed, &view, None)
        .await?;
    tracing::info!(run = %run_id, generation = run.generation, answered = answered.is_some(), "carried on a run a restart interrupted");

    match answered {
        // The person answered and the call never started: carry the answer out, exactly as the
        // answer's own continuation would have.
        Some(answered) => {
            let outcome = if answered.approved {
                opengrok_harness::ResumeOutcome::Approved
            } else {
                opengrok_harness::ResumeOutcome::Refused("the person refused this call".to_string())
            };
            tokio::spawn(crate::agui::resume::resume_where_it_lives(
                false,
                host.clone(),
                account_id,
                run_id.clone(),
                coworker_id,
                answered.call,
                run.emitted.len() as u32,
                outcome,
            ));
        }
        None => {
            tokio::spawn(crate::agui::resume::resume_interrupted_run(
                host.clone(),
                account_id,
                run_id.clone(),
                coworker_id,
            ));
        }
    }
    Ok(())
}

/// Fail a run a continuation could not carry on after the sweep resumed it (#91), saying why.
/// Best effort: the run is already claimed, and a failure to write leaves it for the next sweep.
pub(crate) async fn fail_interrupted(state: &AgUiState, run_id: &RunId, why: &str) {
    let reason =
        format!("this run was interrupted by a restart and could not be carried on: {why}");
    let result = match state.auth.store.load_run(run_id).await {
        Ok((run, seq)) => fail_run(state, run_id, run, seq, &reason).await,
        Err(error) => Err(error),
    };
    if let Err(error) = result {
        tracing::warn!(run = %run_id, %error, "could not fail a run that could not be carried on");
    }
}

/// End a run as `Failed` with `reason`, and close the bubble its dead process left streaming.
async fn fail_run(
    state: &AgUiState,
    run_id: &RunId,
    mut run: opengrok_core::run::Run,
    seq: i64,
    reason: &str,
) -> Result<(), opengrok_store::StoreError> {
    let at_ms = now_ms();
    let events = run
        .decide(RunCommand::Fail {
            reason: reason.to_string(),
            at_ms,
        })
        .map_err(|error| opengrok_store::StoreError::Corrupt(error.to_string()))?;
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

    state
        .auth
        .store
        .append_run(run_id, seq, &events, &view, None)
        .await?;

    // THE RUN IS FAILED; THE BUBBLE IS NOT. Everything above settles the run aggregate, and until
    // this existed that was the whole of recovery — which left the thing the person is actually
    // looking at untouched. `sendPrompt` appends an entry marked `streaming: true` before the turn
    // starts and clears the flag when it finishes, so a process that died in between leaves it set
    // with nothing coming to clear it.
    //
    // The client has NO timeout for that state (verified against the packaged app by the client
    // session: `hasText = content.trim().length > 0 || streaming` keeps an empty bubble on screen,
    // with typing dots and `aria-busy`, until a frame says otherwise). So the person watches a
    // coworker type forever, after every restart mid-answer, and the only escape is a new
    // conversation. Failing the run without closing the entry is a half-fix that looks complete
    // from the server's own logs.
    //
    // Best effort on purpose: the run is already correctly failed, and a transcript that cannot be
    // reached must not turn a tidy-up into a failed sweep that retries forever.
    close_streaming_entries(state, run_id, &run, reason).await;

    tracing::info!(run = %run_id, %reason, "ended an abandoned run");
    Ok(())
}

/// Clear the `streaming` flag a dead process left set, and say why in the bubble.
///
/// Whatever the coworker had already said is KEPT and the reason appended after it. Once a turn
/// streams its answer progressively there will usually be partial text here, and throwing away
/// what the person already read to replace it with an error would lose the useful half of the
/// turn. An empty bubble simply becomes the sentence.
async fn close_streaming_entries(
    state: &AgUiState,
    run_id: &RunId,
    run: &opengrok_core::run::Run,
    reason: &str,
) {
    let Some(coworker) = run.coworker_id.clone() else {
        // A run with no coworker is the bare `/ag-ui` endpoint, which owns no transcript.
        return;
    };
    // The aggregate knows its coworker but not its account, and a transcript is keyed on the pair
    // — a shared coworker holds one per person. The projection is where the owner is recorded.
    let account = match state.auth.store.run_account(run_id).await {
        Ok(Some(account)) => account,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(%error, run = %run_id, "could not read a run's account to close its bubble");
            return;
        }
    };

    let entries = match state
        .auth
        .store
        .streaming_gateway_entries(&coworker, &account)
        .await
    {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(%error, run = %run_id, "could not read a run's streaming entries");
            return;
        }
    };

    for mut entry in entries {
        let Some(id) = entry
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        let said = entry
            .pointer("/message/content")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let closed = if said.trim().is_empty() {
            format!("This turn did not finish: {reason}.")
        } else {
            format!("{said}\n\n(This turn did not finish: {reason}.)")
        };
        if let Some(object) = entry.as_object_mut() {
            // REMOVED, not set false. The client reads the key's presence through
            // `transcriptStreaming(value)`; the final frame of a healthy turn omits it, and a
            // recovered one should be indistinguishable from that.
            object.remove("streaming");
            object["message"] = serde_json::json!({ "type": "text", "content": closed });
        }
        if let Err(error) = state
            .auth
            .store
            .update_gateway_entry_by_id(&coworker, &account, &id, &entry)
            .await
        {
            tracing::warn!(%error, run = %run_id, entry = %id, "could not close a streaming entry");
            continue;
        }
        // NO LIVE FRAME, AND THAT IS ENOUGH HERE. The sweep holds `AgUiState`, which has no path
        // to the live bus (`HostState` owns it, and it owns `AgUiState`, not the reverse) — and
        // plumbing one through for this would be the wrong trade. The case this exists for is a
        // process that died: the client is reconnecting to a NEW process and re-reads the
        // transcript as it does, so it sees the closed row without ever needing a push.
        //
        // The gap is the multi-replica case — replica A dies mid-turn, replica B sweeps it, and a
        // client still attached to B keeps its dots until it next reloads. Worth fixing when a
        // second replica is actually run; not worth restructuring the sweep for today.
        tracing::info!(run = %run_id, entry = %id, "closed a streaming entry a restart abandoned");
    }
}

/// A tool call that was started and never answered.
///
/// Read from the run's own emitted events, because they are the only record of what the dead
/// process had done. A call with a result is settled; one without is the ambiguous case.
fn unresolved_tool_call(run: &opengrok_core::run::Run) -> Option<String> {
    let mut started: Vec<(String, String)> = Vec::new();
    let mut answered: Vec<String> = Vec::new();

    for payload in &run.emitted {
        let kind = payload.get("type").and_then(|value| value.as_str())?;
        let id = payload
            .get("toolCallId")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        match kind {
            "TOOL_CALL_START" => started.push((
                id,
                payload
                    .get("toolCallName")
                    .and_then(|value| value.as_str())
                    .unwrap_or("a tool")
                    .to_string(),
            )),
            "TOOL_CALL_RESULT" => answered.push(id),
            _ => {}
        }
    }

    started
        .into_iter()
        .find(|(id, _)| !answered.contains(id))
        .map(|(_, name)| name)
}

/// Renew the lease on a run while a process works on it.
///
/// Spawned alongside a run; dropped when it ends. Renewing at a third of the lease means two
/// renewals can be lost before anybody else may claim it.
pub fn hold(state: AgUiState, run_id: RunId) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let interval = Duration::from_millis((LEASE_MS / 3).max(1_000) as u64);
        loop {
            if let Err(error) = state
                .auth
                .store
                .hold_run(&run_id, now_ms() + LEASE_MS)
                .await
            {
                tracing::warn!(run = %run_id, %error, "could not renew a run's lease");
            }
            tokio::time::sleep(interval).await;
        }
    })
}

/// Dropping this releases the run: the renewal stops and the lease simply expires.
pub struct Lease(Arc<tokio::task::JoinHandle<()>>);

impl Lease {
    pub fn new(handle: tokio::task::JoinHandle<()>) -> Self {
        Self(Arc::new(handle))
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
#[path = "../tests/unit/recovery.rs"]
mod tests;
