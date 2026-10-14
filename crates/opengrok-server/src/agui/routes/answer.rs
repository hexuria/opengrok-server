//! Answers, continuation, and account event streams.

use super::*;

/// `GET /ag-ui/events`: the account's change notes (`opengrok_events`), ids only. The account is
/// the token's and nothing else; `Last-Event-ID` resumes, and an id it cannot starts with `reset`.
pub async fn account_events(State(state): State<AgUiState>, headers: HeaderMap) -> Response {
    let Some(account) = account_from_bearer(&state, &headers) else {
        return unauthorized("sign in first");
    };
    let last = headers.get("last-event-id").and_then(|id| id.to_str().ok());
    let followed = state.auth.store.events.follow(account.as_str(), last);
    match followed.await {
        Ok(frames) => streaming(axum::body::Body::from_stream(
            crate::shutdown::until_shutdown(frames),
        )),
        Err(error) => unavailable(error),
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
        let generation = run.generation;
        tokio::spawn(async move {
            continue_run(
                host,
                account_id,
                run_id,
                generation,
                pending,
                resumed_seq,
                outcome,
            )
            .await;
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
pub(crate) fn resume_outcome(
    approved: bool,
    pending: &opengrok_core::run::PendingApproval,
) -> opengrok_harness::ResumeOutcome {
    match pending.reason {
        // Re-running `request_user_form` would raise the card again. Submit/dismiss synthesise
        // the tool result; `/answer` is the fallback and must do the same.
        //
        // `/answer` CARRIES NO VALUES, so its yes typed nothing. It used to tell the model the
        // form "was filled into the page", and a coworker told a login is in carries on as if it
        // were — into a page that is still asking. Submit is the door that types (#177).
        opengrok_core::run::SuspendReason::UserForm => {
            opengrok_harness::ResumeOutcome::Settled(if approved {
                "The person answered the form card without entering anything: nothing was typed into the page. If you still need those details, ask again with `request_user_form`; otherwise continue without them, and do not type secrets with `computer`.".to_string()
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
/// A RUN PARKED ON A FORM LEAVES A CARD, and the card holds the coworker's screen: every later turn
/// was refused `computer`, `open_url`, `run_recipe` and `request_user_form` while the answer said
/// the card was closed. So the cards no run waits on any more are settled before the answer is
/// written, and the answer says whether they were. A stop of a run that had already ended does
/// the same, which is how a card an older stop left open gets closed.
///
/// WHAT IT COSTS. A turn that is stopped keeps whatever it has already spent, and that is the
/// correct outcome rather than an oversight: the model calls really happened and the gateway
/// metered each one against the coworker's own key as it completed. Nothing here aborts a task or
/// drops a response mid-stream, and that is deliberate — a half-read model stream would leave
/// tokens spent at the provider and absent from the meter, which is the only way a stop could
/// actually lose money. The frames that spend bought are journaled too, including the round the
/// turn was in the middle of, so `replay_run` still shows what was paid for.
pub async fn stop_run(
    State(host): State<crate::host_state::HostState>,
    headers: axum::http::HeaderMap,
    Path(run_id): Path<String>,
) -> Response {
    let state = &host.agui;
    let run_id = RunId::from_stored(run_id);

    // `replay_run`'s check, byte for byte, and for the same reason: a run holds a whole
    // conversation, so without it a run id is a password — and run ids travel in client URLs and
    // logs. `NOT_FOUND` rather than `FORBIDDEN` for "no such run", "not yours" and "not signed in"
    // alike, so probing ids reveals nothing about which runs exist. The two answers have to be
    // indistinguishable down to the bytes, which is why this says the same words.
    let Some(account_id) = account_from_bearer(state, &headers) else {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    };
    match state.auth.store.run_owned_by(&run_id, &account_id).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::NOT_FOUND, "no such run").into_response(),
        Err(error) => return unavailable(error),
    }

    for _ in 0..STOP_ATTEMPTS {
        let (mut run, seq) = match state.auth.store.load_run(&run_id).await {
            Ok(loaded) => loaded,
            Err(error) => return unavailable(error),
        };
        // A run the log has no start for never took a turn, so there is nothing to stop and
        // nothing to say about it — the same answer `replay_run` gives, for the same reason.
        if !run.started {
            return (StatusCode::NOT_FOUND, "no such run").into_response();
        }

        // Read BEFORE the stop is applied: what the run was doing is what decides how honest the
        // answer can be about when the stop takes hold, and what it was waiting on decides whether
        // it can say the card is closed.
        let was = run.status;
        let on_a_form = run
            .pending
            .as_ref()
            .is_some_and(|pending| pending.reason == opengrok_core::run::SuspendReason::UserForm);

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
            Err(_) => {
                let closed = close_cards(&host, &account_id, &run).await;
                return stopped_answer(&run_id, was, on_a_form, closed);
            }
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
        let store = &state.auth.store;
        match store
            .append_run(&run_id, seq, &events, &view, Some(&account_id))
            .await
        {
            Ok(_) => {
                tracing::info!(run = %run_id, by = %account_id, "a run was stopped");
                crate::pairs::ended(state, &run.thread_id, run.status);
                // A call its person's Mac is carrying is cancelled there, now (`relay`).
                state.auth.relay.stop(run_id.as_str());
                let closed = close_cards(&host, &account_id, &run).await;
                return stopped_answer(&run_id, was, on_a_form, closed);
            }
            // Somebody wrote to this run between the read and the write. Re-read and decide again
            // against what is actually there: either the run is now ended, and the next pass
            // answers with that, or the turn simply journaled a round and this stop still stands.
            Err(opengrok_store::StoreError::Conflict) => continue,
            Err(error) => return unavailable(error),
        }
    }

    unavailable("that run is being written to faster than it can be stopped; try again")
}

/// Settle the cards a stop leaves with nothing waiting on them. Once, after the stop is in the
/// log, however many attempts writing it took: before it, the run still waits on them.
async fn close_cards(
    host: &crate::host_state::HostState,
    account_id: &AccountId,
    run: &opengrok_core::run::Run,
) -> bool {
    match crate::agui::resume::coworker_of(run) {
        Some(coworker) => {
            crate::agui::user_form::settle_dead_holds(host, account_id, &coworker, now_ms()).await
        }
        None => true,
    }
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
fn stopped_answer(run_id: &RunId, was: RunStatus, on_a_form: bool, cards_closed: bool) -> Response {
    let (takes_effect, note) = match was {
        // Nothing was running, and saying "stopped" is still the right answer: the person asked
        // for this run not to be running, and it is not.
        RunStatus::Finished | RunStatus::Failed | RunStatus::Stopped => (
            "already-ended",
            "That run had already ended, so there was nothing left to stop.",
        ),
        // Waiting on a person is not working. No model call is in flight and no tool is running,
        // so the run is over the moment the log says so.
        //
        // Only a form's card is settled by a stop. An approval card (auto-review, policy,
        // local-tool-permission) is left as it was painted, so the answer does not say it is
        // closed — only what is true: the run has left the approvals queue and its card can no
        // longer make anything run.
        RunStatus::AwaitingApproval if !on_a_form => (
            "immediately",
            "That run was waiting on an approval and is stopped: nothing it asked for will run.",
        ),
        RunStatus::AwaitingApproval if cards_closed => (
            "immediately",
            "That run was waiting on an approval, and the card it was waiting on is closed.",
        ),
        // Said rather than implied: the card still shows, and the coworker's next turn is what
        // closes it.
        RunStatus::AwaitingApproval => (
            "immediately",
            "That run is stopped, but the card it was waiting on could not be closed yet; the \
             coworker's next turn closes it.",
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

/// How many tool results from a stopped turn are worth showing the next one.
/// One per model call, so a turn that hit the 8-call cap still shows every result.
pub(crate) const STEER_TOOL_CAP: usize = 8;
/// Bound one result so a shell dump cannot become the next prompt.
pub(crate) const STEER_TOOL_CHARS: usize = 800;

pub(crate) const STEER_CONTINUATION: &str = "[harness] The previous turn on this thread stopped or failed \
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

pub(crate) fn clip_chars(text: &str, max: usize) -> String {
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
                out.push(ChatMessage::text(
                    "user",
                    format!(
                        "[earlier {name} {}] {}",
                        clip_chars(args, 400),
                        clip_chars(content, STEER_TOOL_CHARS)
                    ),
                ));
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
    block.push(ChatMessage::text("user", STEER_CONTINUATION.to_string()));
    let at = messages
        .iter()
        .rposition(|message| message.role == "user")
        .unwrap_or(messages.len());
    messages.splice(at..at, block);
}

/// Newest prior run on this thread, when it stopped or failed with tools and
/// was not parked on a card.
pub(super) async fn continue_stopped_turn(
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
/// that is answered and unfinished, which the recovery sweep carries on (#91).
async fn continue_run(
    host: crate::host_state::HostState,
    account_id: opengrok_core::id::AccountId,
    run_id: RunId,
    generation: u32,
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
    // EVERY EARLY RETURN LEAVES THE RUN FOR THE SWEEP: the answer is durable, and a run answered
    // and unfinished is carried on there (#91). The coworker is the run's own.
    let (run, coworker_id, coworker, limits) =
        match super::resume::loaded(&state, &account_id, (&run_id, generation), None).await {
            Ok(Some(loaded)) => loaded,
            Ok(None) => return,
            Err(why) => {
                tracing::warn!(run = %run_id, why, "an answered run could not be continued");
                return;
            }
        };

    let who = (&account_id, &coworker_id);
    let at = (&run_id, &run, generation, limits);
    let answer = (answered, resumed_seq, outcome);
    let carried = super::resume::carry_out_answer(&state, who, at, &coworker, answer);
    let Some(events) = carried.await else {
        tracing::warn!(run = %run_id, "an answered run has no tools to continue with");
        return;
    };
    tracing::info!(run = %run_id, events = events.len(), "continued an answered run");
    // The continued run may pause again — on a user form, a saved-login request — and that
    // pause needs its card in the transcript exactly as a fresh turn's does: without the
    // card there is no `entryId`, and NativeChat cannot submit what the person typed.
    super::resume::emit_user_form_suspensions(&host, &coworker_id, &account_id, &events).await;
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
                    let origin = match state.auth.store.run_fired_by(&run.thread_id, &run_id).await
                    {
                        Ok(fired_by) => serde_json::json!(origin_word(fired_by)),
                        // Unknown, and said so: "chat" here would put a routine's card in a
                        // conversation the person never had.
                        Err(error) => {
                            tracing::error!(%error, run = %run_id, "could not read what started a waiting run");
                            serde_json::Value::Null
                        }
                    };
                    waiting.push(serde_json::json!({
                        "runId": run_id.as_str(),
                        "threadId": run.thread_id,
                        // WHOSE CARD THIS IS, and what started the run. A routine's thread is its
                        // own id and names no chat, so without these a client cannot put the card
                        // beside any coworker (#177). Additive: every field above keeps its shape.
                        "coworkerId": run.coworker_id.as_ref().map(|id| id.as_str()),
                        "origin": origin,
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
                        // them, else a sentence built from the reason — see `waiting_why`.
                        "why": waiting_why(&run, &pending),
                        // WHAT THE CALL WILL DO, in the card's own words (#249): the sentence
                        // `summary_for` builds and redacts, so a client shows it rather than
                        // rebuilding it. Null for a form, whose card is the form.
                        "summary": crate::cards::summary_for_ask(
                            &pending.tool,
                            &pending.arguments,
                            pending.reason.as_str(),
                        ),
                    }));
                }
            }
            // An ARRAY, always: an empty queue is a valid answer.
            Json(waiting).into_response()
        }
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    }
}

/// `origin` on an approvals row: what started the run, from the log (`run_fired_by`).
pub(super) fn origin_word(fired_by: Option<opengrok_store::FiredBy>) -> &'static str {
    match fired_by {
        Some(opengrok_store::FiredBy::Schedule) => "schedule",
        Some(opengrok_store::FiredBy::Webhook) => "webhook",
        Some(opengrok_store::FiredBy::Monitor) => "monitor",
        None => "chat",
    }
}

/// Why this run is waiting, in a sentence: the ask's own words when the run journalled them,
/// else one built from the reason. The approvals queue and a routine's `lastRun` say the same
/// thing about the same card.
pub(crate) fn waiting_why(
    run: &opengrok_core::run::Run,
    pending: &opengrok_core::run::PendingApproval,
) -> String {
    journalled_why(run, pending).unwrap_or_else(|| why_of(pending))
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
    // ask's bare line ("Waiting for you"), and `why_of` says more for those — except a routine's
    // delete, whose words name the routine as stored, which nothing else on the run does (#316).
    let delete = pending.tool == opengrok_tools::routine::DELETE_ROUTINE;
    if pending.reason != opengrok_core::run::SuspendReason::AutoReview && !delete {
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
    // A form is named by its own title and instruction — the words on the card. The generic
    // tool summary would print its field list as a plugin call's arguments.
    if pending.reason == SuspendReason::UserForm {
        let said = |key: &str| {
            pending
                .arguments
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(|text| text.chars().take(160).collect::<String>())
        };
        return match (said("title"), said("instruction")) {
            (Some(title), Some(instruction)) => {
                format!(
                    "The coworker is asking you to fill in \u{201c}{title}\u{201d}: {instruction}"
                )
            }
            (Some(title), None) => {
                format!("The coworker is asking you to fill in \u{201c}{title}\u{201d}.")
            }
            _ => "The coworker is asking you to fill something in.".to_string(),
        };
    }
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
