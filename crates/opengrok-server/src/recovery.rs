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

/// Sweep forever. Started by the binary; stops when the process does.
pub async fn sweep_forever(state: AgUiState) {
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
pub async fn sweep_once(state: &AgUiState) -> Result<usize, opengrok_store::StoreError> {
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
        match resolve(state, &run_id).await {
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

/// Bring one abandoned run to an ending.
///
/// Ending it is the point. A run that stays `running` is one a person watches forever; whether it
/// finishes or fails, the client gets something it can render.
async fn resolve(state: &AgUiState, run_id: &RunId) -> Result<(), opengrok_store::StoreError> {
    let (run, seq) = state.auth.store.load_run(run_id).await?;

    // Already settled by somebody else between the claim and now.
    if matches!(run.status, RunStatus::Finished | RunStatus::Failed) {
        return Ok(());
    }
    // Waiting on a person is not abandonment — it is the run doing exactly what it should.
    if run.status == RunStatus::AwaitingApproval {
        return Ok(());
    }

    let at_ms = now_ms();
    let unresolved = unresolved_tool_call(&run);

    // WE DO NOT KNOW WHETHER IT RAN, SO WE SAY SO. Re-running would repeat whatever it did, and
    // pretending it failed would be a claim we cannot support.
    let reason = match &unresolved {
        Some(call) => format!(
            "this run was interrupted by a restart while `{call}` was in flight; \
             whether it completed is unknown, so it was not run again"
        ),
        None => "this run was interrupted by a restart and did not continue".to_string(),
    };

    let mut run = run;
    let events = run
        .decide(RunCommand::Fail {
            reason: reason.clone(),
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
    close_streaming_entries(state, run_id, &run, &reason).await;

    tracing::info!(run = %run_id, unresolved = ?unresolved, "ended an abandoned run");
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
        // to the live bus (`GatewayState` owns it, and it owns `AgUiState`, not the reverse) — and
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
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use opengrok_core::run::{Run, RunEvent};
    use serde_json::json;

    fn run_with(events: Vec<serde_json::Value>) -> Run {
        let mut log = vec![RunEvent::Started {
            thread_id: "t1".to_string(),
            coworker_id: None,
            model: None,
            system: None,
            at_ms: 1,
        }];
        for (index, payload) in events.into_iter().enumerate() {
            log.push(RunEvent::Emitted {
                seq: index as i64,
                payload,
                at_ms: 2,
            });
        }
        Run::replay(&log)
    }

    /// The ambiguous case: a call went out and no result came back.
    #[test]
    fn a_tool_call_without_a_result_is_the_unresolved_one() {
        let run = run_with(vec![
            json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "shell"}),
        ]);
        assert_eq!(unresolved_tool_call(&run).as_deref(), Some("shell"));
    }

    /// A completed call is settled and must not be reported as in flight — that would tell a
    /// person their command might have run when the log says it did.
    #[test]
    fn a_tool_call_with_a_result_is_settled() {
        let run = run_with(vec![
            json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "shell"}),
            json!({"type": "TOOL_CALL_RESULT", "toolCallId": "c1", "content": "done"}),
        ]);
        assert_eq!(unresolved_tool_call(&run), None);
    }

    /// With several calls, the one still open is the one that matters.
    #[test]
    fn the_open_call_is_found_among_settled_ones() {
        let run = run_with(vec![
            json!({"type": "TOOL_CALL_START", "toolCallId": "c1", "toolCallName": "read_file"}),
            json!({"type": "TOOL_CALL_RESULT", "toolCallId": "c1", "content": "ok"}),
            json!({"type": "TOOL_CALL_START", "toolCallId": "c2", "toolCallName": "shell"}),
        ]);
        assert_eq!(unresolved_tool_call(&run).as_deref(), Some("shell"));
    }

    /// A run that only talked has nothing in flight.
    #[test]
    fn a_run_with_no_tools_has_nothing_unresolved() {
        let run = run_with(vec![
            json!({"type": "TEXT_MESSAGE_CONTENT", "delta": "hello"}),
        ]);
        assert_eq!(unresolved_tool_call(&run), None);
    }
}
