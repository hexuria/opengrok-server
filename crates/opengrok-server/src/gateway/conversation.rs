//! P4: one conversation, from the real client's point of view.
//!
//! `sendPrompt` answers `{accepted: true}` the moment the turn is DURABLY accepted — never when
//! it completes — because the client only retries a send against an endpoint that has proven
//! nonce dedupe, and a slow answer here is a stuck composer there. The idempotency ledger is
//! Postgres, not memory: a repeated nonce with the same input digest is answered `accepted`
//! again without running anything, and the same nonce with DIFFERENT input is refused loudly
//! (`NONCE_DIGEST_MISMATCH`) — silently absorbing it would send a message the user rewrote.
//!
//! The turn itself is the harness we already have: the coworker's own model through
//! `run_conversation`, journaled like every other run. The transcript entries here are the
//! client-shaped projection of that turn, and the choreography — user echo with `clientNonce`,
//! streaming placeholder, final update, roster pulses — is `client-grok-bot.md` §2.4 step for
//! step.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

use opengrok_core::id::{CoworkerId, RunId};
use opengrok_harness::{ChatMessage, ModelRequest, run_conversation};

use super::{GatewayState, live};
use crate::agui::routes::StoreJournal;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// The system prompt that keeps a coworker honest about WHOSE computer it is using. A bot has its
/// OWN box (sandboxed, on the server); the user has their own machine; these are different, and a
/// bot must never present work done on its box as done on the user's computer. When it has no
/// computer, it should say so rather than pretend. Written for the day a reverse channel makes "my
/// computer" name two real machines — the distinction has to be in the model's head before then.
fn computer_system_prompt(
    has_computer: bool,
    reaches_user_machine: bool,
    user_machine_label: Option<&str>,
) -> String {
    if has_computer {
        // NO OS OR HARDWARE NAMES HERE. The box's runtime varies (Linux today; other kinds later)
        // and the user's machine is whatever they enrolled — naming either ("Linux box", "their
        // Mac") turns the prompt into a lie the day the fleet changes. The distinction that must
        // survive is WHOSE machine, not what it runs.
        let mut prompt = "You have your OWN computer: a sandboxed box running on the server. It is a DIFFERENT \
         machine from the user's own computer. Your shell, read_file and \
         write_file tools act ONLY on your own box — they cannot touch the user's machine. When you \
         run a command or create, read or change a file, it happens on YOUR box, and you must say so \
         plainly, e.g. \"I created /tmp/foo on my own computer (the box), not on your machine.\" \
         Never describe work done on your box as done on the user's computer."
            .to_string();
        // The two halves of this prompt MUST track the tool list. The refusal wording below was
        // shipped while `user_machine_shell` was being offered, and the model believed the prompt
        // over the tool: it answered "I can't access your Mac" without ever calling it. A system
        // prompt that contradicts the offering silently disables the tool.
        if reaches_user_machine {
            // The enrolled label (e.g. "Uriah's-MacBook-Pro.local") is the one name for the
            // user's machine that stays TRUE whatever it runs — the daemon reported it at
            // enrolment. Guessing an OS instead ("their Mac") becomes a lie the day a Windows
            // or second machine enrolls. No label ⇒ stay generic, never invent one.
            let machine = match user_machine_label {
                Some(label) => format!("the computer they enrolled, \"{label}\""),
                None => "the real computer they enrolled".to_string(),
            };
            prompt.push_str(&format!(
                " You ALSO have the `user_machine_shell` tool, which runs a command on the USER'S \
                 OWN machine — {machine} — with their consent: a command may \
                 run, be refused, or wait for the user to approve it, and waiting is normal — the \
                 user may answer minutes or hours later, so never retry or give up on a waiting \
                 command. When the user asks you to do something on THEIR computer, use \
                 `user_machine_shell` rather than telling them to do it themselves, and refer to \
                 their machine by that name."
            ));
        } else {
            prompt.push_str(
                " If the user asks you to do something on THEIR computer, tell them you can only \
                 use your own box and cannot reach their machine, and offer to do it on your box \
                 instead.",
            );
        }
        prompt
    } else {
        "You do NOT currently have a computer, so you cannot run shell commands or read or write \
         files anywhere. Do not claim to run commands or access any machine. If the user needs \
         something run, explain that your computer is not available yet and, where useful, give them \
         the exact command to run themselves."
            .to_string()
    }
}

fn entry_id() -> String {
    format!("e_{}", uuid::Uuid::now_v7())
}

/// The named agent, or the one the client last opened.
pub fn agent_or_active(state: &GatewayState, args: &Value) -> Option<String> {
    // The client names the agent under different keys on different verbs: `agentId` on sendPrompt,
    // but the transcript reads (getAgentTranscriptTail / openAgentTail / getAgentThread) pass it as
    // `id` or `threadId`. Accept them all before falling back to the last-opened agent — otherwise a
    // read that DID name its agent was answered "no agent named and none active" and the reply could
    // never paint.
    ["agentId", "id", "threadId", "agent_id"]
        .iter()
        .find_map(|key| args.get(*key).and_then(Value::as_str))
        .map(str::to_string)
        .or_else(|| {
            state
                .active_agent
                .lock()
                .ok()
                .and_then(|active| active.clone())
        })
}

/// The input digest the ledger compares. Mirrors the client's field list
/// (`prompt-acceptance-ledger.ts:34-49`): agentId, prompt, richText, replyToId, isFork, and the
/// attachment name/path arrays.
pub fn input_digest(args: &Value, agent_id: &str) -> String {
    let mut hasher = Sha256::new();
    let mut take = |value: &str| {
        hasher.update(value.as_bytes());
        hasher.update([0]);
    };
    take(agent_id);
    take(
        args.get("prompt")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    take(
        args.get("richText")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    take(
        args.get("replyToId")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    take(
        if args.get("isFork").and_then(Value::as_bool).unwrap_or(false) {
            "1"
        } else {
            "0"
        },
    );
    for key in ["attachmentPaths", "attachmentNames"] {
        if let Some(items) = args.get(key).and_then(Value::as_array) {
            for item in items {
                take(item.as_str().unwrap_or_default());
            }
        }
    }
    hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Accept, echo, and run — the whole sendPrompt path after auth.
///
/// Returns `(status, body)` so `routes.rs` can wrap it in the wire mechanics.
pub async fn send_prompt(state: &GatewayState, args: &Value, caller: &str) -> (u16, Value) {
    let Some(prompt) = args
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return (400, json!({ "error": "sendPrompt needs a prompt" }));
    };
    let Some(nonce) = args
        .get("clientNonce")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        // Required for retry-safety; a send with no nonce cannot be deduped and must not be
        // accepted as if it could be.
        return (400, json!({ "error": "sendPrompt needs a clientNonce" }));
    };
    let Some(agent_id) = agent_or_active(state, args) else {
        return (400, json!({ "error": "no agent named and none active" }));
    };

    let coworker_id = CoworkerId::from_stored(agent_id.clone());
    let Ok((coworker, _)) = state.agui.auth.store.load_coworker(&coworker_id).await else {
        return (404, json!({ "error": format!("no agent {agent_id}") }));
    };
    let Ok(Some(account)) = state.agui.auth.store.account_by_email(caller).await else {
        return (
            500,
            json!({ "error": "the gateway account does not exist yet" }),
        );
    };

    // The ledger speaks first. A duplicate is answered `accepted` with nothing re-run; a nonce
    // reused for different input is the one refusal that must never be quiet.
    let digest = input_digest(args, &agent_id);
    let record = json!({
        "clientNonce": nonce,
        "agentId": agent_id,
        "acceptedAtMs": now_ms(),
    });
    match state
        .agui
        .auth
        .store
        .accept_nonce(caller, &nonce, &digest, &record, now_ms())
        .await
    {
        Ok(Ok(stored)) => {
            if stored.get("acceptedAtMs") != record.get("acceptedAtMs") {
                // The earlier acceptance stands; the turn it accepted is already running or done.
                return (200, json!({ "accepted": true }));
            }
        }
        Ok(Err(())) => {
            return (409, json!({ "error": "NONCE_DIGEST_MISMATCH" }));
        }
        Err(error) => {
            tracing::error!(%error, "the acceptance ledger is unreachable");
            return (500, json!({ "error": "acceptance ledger unavailable" }));
        }
    }

    // The user's own message, durably in the transcript and echoed over SSE carrying the
    // clientNonce — that echo is what settles the optimistic bubble in the renderer.
    // The reply link (`replyToId` on the send, `replyTo` on the entry — `transcript.rs:53`): the
    // page draws its "quoted" header from the entry, so the echo that replaces the optimistic
    // bubble must carry it, or the header vanishes the moment the send is accepted.
    let reply_to = args
        .get("replyToId")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .map(str::to_string);
    let mut user_entry = json!({
        "kind": "message",
        "id": entry_id(),
        "role": "user",
        "content": prompt,
        "isStreaming": false,
        "timestampMs": now_ms(),
        "clientNonce": nonce,
    });
    if let Some(id) = &reply_to {
        user_entry["replyTo"] = json!(id);
    }
    if let Err(error) = state
        .agui
        .auth
        .store
        .append_gateway_entry(&coworker_id, &user_entry, now_ms())
        .await
    {
        tracing::error!(%error, "could not append the user's message");
        return (500, json!({ "error": "transcript unavailable" }));
    }
    live::emit_transcript(state, &agent_id, "appended", user_entry.clone());

    // A group answers as a ROOM: its members take turns (`group.rs`), each posting under its
    // own name, so there is no single answer bubble to grow into.
    if coworker.is_group() {
        live::set_running(state, &agent_id, true, json!({})).await;
        let handle = tokio::spawn(super::group::run_group_turn(
            state.clone(),
            account.id,
            coworker_id,
            coworker,
        ));
        if let Ok(mut cancels) = state.cancels.lock() {
            cancels.insert(agent_id.clone(), handle.abort_handle());
        }
        return (200, json!({ "accepted": true }));
    }

    // The streaming placeholder the answer will grow into.
    let answer_id = entry_id();
    let mut placeholder = json!({
        "kind": "send-message",
        "id": answer_id,
        "message": { "type": "text", "content": "" },
        "timestampMs": now_ms(),
        "streaming": true,
    });
    if let Some(id) = &reply_to {
        placeholder["replyTo"] = json!(id);
    }
    let answer_seq = match state
        .agui
        .auth
        .store
        .append_gateway_entry(&coworker_id, &placeholder, now_ms())
        .await
    {
        Ok(seq) => seq,
        Err(error) => {
            tracing::error!(%error, "could not append the answer placeholder");
            return (500, json!({ "error": "transcript unavailable" }));
        }
    };
    live::emit_transcript(state, &agent_id, "appended", placeholder);

    live::set_running(state, &agent_id, true, json!({})).await;

    // The turn, off this request's clock. `accepted` means accepted, not answered. Keep the task's
    // abort handle so stopAgentTurn can cancel it; run_turn removes itself when it ends.
    let task_state = state.clone();
    let history = history_for(state, &coworker_id).await;
    let handle = tokio::spawn(run_turn(
        task_state,
        account.id,
        coworker_id,
        coworker.model.clone(),
        history,
        Answer {
            id: answer_id,
            seq: answer_seq,
            reply_to,
        },
    ));
    if let Ok(mut cancels) = state.cancels.lock() {
        cancels.insert(agent_id.clone(), handle.abort_handle());
    }

    (200, json!({ "accepted": true }))
}

/// `stopAgentTurn` — end an in-flight turn for an agent, or clear a phantom "working" flag. Aborts
/// the running task if there is one (its drop-guard then clears the flag), and force-clears the
/// `running` flag + emits a roster update either way. SAFE and idempotent when nothing is running:
/// that is exactly the way out of a stale flag with no turn behind it. Never an error.
pub async fn stop_agent_turn(state: &GatewayState, args: &Value, _caller: &str) -> (u16, Value) {
    let Some(agent_id) = agent_or_active(state, args) else {
        return (200, Value::Null);
    };
    // Abort the live turn, if any. The drop-guard inside run_turn clears the flag on the way down;
    // we also clear it directly below so a PHANTOM flag (no task) is resolved too.
    if let Some(handle) = state
        .cancels
        .lock()
        .ok()
        .and_then(|mut cancels| cancels.remove(&agent_id))
    {
        handle.abort();
    }
    live::set_running(state, &agent_id, false, json!({})).await;
    (200, json!({ "agentId": agent_id, "isRunning": false }))
}

/// `getForeverBoxStatus` — the caller's agent's LIVE box health, in the client's `BoxStatus` shape
/// (`{agentId, state, vncUrl}`). This is the signal that stops the app spinning "Booting up the
/// computer" forever: a running box says `running`, a released one `absent`, and a dead one its real
/// word (`exited`/`stopped`/…), so a box that died says it died instead of pretending to boot.
///
/// `vncUrl` is the noVNC desktop URL when the box is running and the provider has a screen
/// (`Computer::screen_url` → ASCII `POST /boxes/{id}/desktop?vnc=1`); otherwise null. A first
/// poll after ensure can be `running` with `vncUrl: null` while the desktop is still
/// provisioning — a later poll carries the link. We never invent a URL. Do not log it: it
/// carries a password / `_token`.
pub async fn box_status(state: &GatewayState, args: &Value, caller: &str) -> (u16, Value) {
    use crate::agui::provision;

    let Some(agent_id) = agent_or_active(state, args) else {
        // No agent named and none active — nothing to report. Null is a shape the client accepts.
        return (200, Value::Null);
    };
    let absent = || json!({ "agentId": agent_id, "state": "absent", "vncUrl": Value::Null });
    // A box is reported per SCOPE (per-account shares one box across the account's agents), so
    // resolve the scope's box for the caller — but only for an agent that actually exists. An
    // unknown id replays to a default (empty-named) coworker; that is no coworker, so `absent`,
    // rather than borrowing the account's shared box.
    match state
        .agui
        .auth
        .store
        .load_coworker(&CoworkerId::from_stored(agent_id.clone()))
        .await
    {
        Ok((coworker, _)) if !coworker.name.is_empty() => {}
        _ => return (200, absent()),
    }
    let Ok(Some(account)) = state.agui.auth.store.account_by_email(caller).await else {
        return (200, absent());
    };
    let (mode, org_id) = provision::resolve_mode(&state.agui, &account.id).await;
    let (scope, scope_id, _) =
        provision::scope_for(&mode, account.id.as_str(), org_id.as_deref(), &agent_id);
    let Ok(Some((box_id, kind, stopped))) = state
        .agui
        .auth
        .store
        .scoped_computer_full(scope, &scope_id)
        .await
    else {
        return (200, absent());
    };

    // Ask the provider for the box's REAL state rather than trusting our stopped flag: the flag is
    // idle-stop's bookkeeping and can lag reality (a turn resumes the box), and a stale flag once
    // reported a running box as "stopped" for the whole computer panel. A mapped box whose
    // provider cannot be built (KEK rotated, key missing) is NOT absent — the mapping is still
    // there — so we say so with computerError instead of pretending there is no computer.
    let lookup = provision::lookup_provider(&state.agui, org_id.as_deref(), &kind).await;
    let Some(provider) = lookup.computer else {
        let (code, message) = lookup.error.unwrap_or_else(|| {
            (
                "unknown".into(),
                "the computer's provider is not available".into(),
            )
        });
        return (
            200,
            json!({
                "agentId": agent_id,
                "state": if stopped { "stopped" } else { "unknown" },
                "vncUrl": Value::Null,
                "computerError": { "code": code, "message": message },
            }),
        );
    };
    let live_state = provider
        .state(&box_id)
        .await
        .unwrap_or_else(|_| "unknown".to_string());
    // A running box may have a screen (box.ascii.dev provisions a noVNC desktop); headless boxes and
    // not-running ones have none. Only ask when it is up, so a poll of a stopped box costs nothing.
    let vnc_url = if live_state == "running" {
        provider.screen_url(&box_id).await.ok().flatten()
    } else {
        None
    };

    (
        200,
        json!({
            "agentId": agent_id,
            "state": live_state,
            "vncUrl": vnc_url,
        }),
    )
}

/// What `box_control` should do to the caller's agent's box.
pub enum BoxAction {
    /// `ensureForeverBox` — make the box RUNNING: resume an idle/stopped one in place, or provision
    /// and assign a fresh one when there is none.
    Ensure,
    /// `handBackForeverBox` — release the box: stop it (disk kept, billing paused). Recoverable by
    /// Ensure or the next message.
    HandBack,
    /// `resetForeverBox` — destroy the box and provision a fresh one in its place.
    Reset,
}

/// The box-control verbs, for real — no stub that lies "running". ensure/handBack/reset ACT on the
/// caller's agent's box and answer with its resulting live `BoxStatus`. A box brought up in place
/// keeps the same id, so the coworker's executor binding stays valid; provisioning a NEW box (Ensure
/// from absent, or Reset) re-assigns the coworker so the binding follows. `updateForeverBox` has no
/// image-update mechanism for our boxes, so it reports the current status honestly rather than
/// claiming a change that did not happen — its handler simply calls `box_status`.
pub async fn box_control(
    state: &GatewayState,
    args: &Value,
    caller: &str,
    action: BoxAction,
) -> (u16, Value) {
    use crate::agui::provision;

    let Some(agent_id) = agent_or_active(state, args) else {
        return (200, Value::Null);
    };
    let coworker_id = CoworkerId::from_stored(agent_id.clone());
    let error_status = |code: String, message: String| {
        json!({
            "agentId": agent_id,
            "state": "absent",
            "vncUrl": Value::Null,
            "computerError": { "code": code, "message": message },
        })
    };
    let Ok(Some(account)) = state.agui.auth.store.account_by_email(caller).await else {
        return (
            200,
            error_status("unknown".into(), "no such account".into()),
        );
    };
    let (mode, org_id) = provision::resolve_mode(&state.agui, &account.id).await;
    let (scope, scope_id, _) =
        provision::scope_for(&mode, account.id.as_str(), org_id.as_deref(), &agent_id);
    let existing = state
        .agui
        .auth
        .store
        .scoped_computer_full(scope, &scope_id)
        .await
        .ok()
        .flatten();

    match action {
        BoxAction::HandBack => {
            if let Some((box_id, kind, _)) = &existing
                && let Some(provider) =
                    provision::provider_for(&state.agui, org_id.as_deref(), kind).await
            {
                let _ = provider.stop(box_id).await;
                let _ = state
                    .agui
                    .auth
                    .store
                    .mark_scoped_stopped(scope, &scope_id)
                    .await;
            }
        }
        BoxAction::Ensure => match &existing {
            Some((box_id, kind, stopped)) => {
                let lookup = provision::lookup_provider(&state.agui, org_id.as_deref(), kind).await;
                if let Some(provider) = lookup.computer {
                    let live = provider.state(box_id).await.ok();
                    let running = live.as_deref() == Some("running");
                    if *stopped || !running {
                        // Resume AND wait. On box.ascii.dev a sleeping box is `archived`, and an
                        // accepted resume is a 202 + `provisioning` — not a running box. `wake`
                        // polls until it is `running` (bounded), so the status we answer with is
                        // one the client can draw a screen for rather than one it must guess at.
                        // Past the bound the box is still on its way; the client keeps polling.
                        let woke = provider.wake(box_id, WAKE_PATIENCE).await;
                        match woke {
                            Err(error) => {
                                return (
                                    200,
                                    json!({
                                        "agentId": agent_id,
                                        "state": live.unwrap_or_else(|| "stopped".into()),
                                        "vncUrl": Value::Null,
                                        "computerError": {
                                            "code": error.code(),
                                            "message": error.to_string(),
                                        },
                                    }),
                                );
                            }
                            Ok(reached) => {
                                let _ = state
                                    .agui
                                    .auth
                                    .store
                                    .mark_scoped_used(scope, &scope_id, now_ms())
                                    .await;
                                if reached == "running" {
                                    // The desktop provisions after the box does; give it a moment
                                    // so the first status after ensure can carry the screen.
                                    wait_for_screen(provider.as_ref(), box_id, SCREEN_PATIENCE)
                                        .await;
                                }
                            }
                        }
                    }
                }
                // No provider: fall through to box_status, which attaches computerError
                // and does not pretend the mapped box is absent.
            }
            None => {
                if let Err((code, message)) = reprovision(state, &account.id, &coworker_id).await {
                    return (200, error_status(code, message));
                }
            }
        },
        BoxAction::Reset => {
            if let Some((box_id, kind, _)) = &existing
                && let Some(provider) =
                    provision::provider_for(&state.agui, org_id.as_deref(), kind).await
            {
                let _ = provider.destroy(box_id).await;
            }
            let _ = state
                .agui
                .auth
                .store
                .clear_scoped_computer(scope, &scope_id)
                .await;
            if let Err((code, message)) = reprovision(state, &account.id, &coworker_id).await {
                return (200, error_status(code, message));
            }
        }
    }

    // Answer with the box's real resulting state.
    box_status(state, args, caller).await
}

/// How long `ensureForeverBox` waits for a resumed box to be `running` before answering with
/// whatever state it reached. A box.ascii.dev resume restores a snapshot onto a fresh machine
/// (archived → provisioned → running in 10–15s live, bx_ncfmdpem, 2 Sep 2026); past this bound the
/// client keeps polling `getForeverBoxStatus` instead. The desktop client puts no deadline on this
/// call: `source/node-agent-coordinator/gateway/gateway-client.ts` `request()` fetches with only
/// the caller's optional `init.signal`, and `foreverBoxStatusCommand` passes none; nothing on our
/// side cuts it off either (no `TimeoutLayer` in the router).
const WAKE_PATIENCE: std::time::Duration = std::time::Duration::from_secs(90);

/// How long `ensureForeverBox` waits for the desktop URL once the box is up. `POST /desktop?vnc=1`
/// answers `provisioning: true` (no URL) until noVNC is ready; a later status poll carries it.
const SCREEN_PATIENCE: std::time::Duration = std::time::Duration::from_secs(20);

/// Poll the provider for a screen URL until one exists or `patience` is spent. The URL is not
/// returned: `box_status` asks again (the call is idempotent) and reports it in the status shape.
async fn wait_for_screen(
    provider: &dyn opengrok_box::Computer,
    box_id: &str,
    patience: std::time::Duration,
) {
    let started = std::time::Instant::now();
    loop {
        if let Ok(Some(_)) = provider.screen_url(box_id).await {
            return;
        }
        if started.elapsed() >= patience {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// Provision (or re-provision) a coworker's box and PERSIST the re-assignment to its aggregate, so
/// the executor — which binds `coworker.computer()`, not the scope mapping — points at the new box.
/// Returns the provisioning error as `(code, message)` if it could not get a box.
async fn reprovision(
    state: &GatewayState,
    account_id: &opengrok_core::id::AccountId,
    coworker_id: &CoworkerId,
) -> Result<(), (String, String)> {
    use opengrok_core::coworker::CoworkerView;

    let Ok((mut coworker, seq)) = state.agui.auth.store.load_coworker(coworker_id).await else {
        return Err(("unknown".into(), "could not load the coworker".into()));
    };
    let at_ms = now_ms();
    let provisioned = crate::agui::provision::ensure_computer_for(
        &state.agui,
        account_id,
        coworker_id,
        &mut coworker,
        at_ms,
    )
    .await;
    if let Some(error) = provisioned.error {
        return Err(error);
    }
    let view = CoworkerView {
        id: coworker_id.clone(),
        name: coworker.name.clone(),
        model: coworker.model.clone(),
        box_id: coworker.computer().cloned(),
        retired: false,
        members: Vec::new(),
        updated_at_ms: at_ms,
    };
    let _ = state
        .agui
        .auth
        .store
        .append_coworker(coworker_id, account_id, seq, &provisioned.events, &view)
        .await;
    Ok(())
}

/// How much of a quoted message the model is shown; a reply to a long answer names the answer,
/// it does not replay it.
const REPLY_QUOTE_CHARS: usize = 1_000;

/// The message a reply points at, as one bracketed line the model reads ahead of the prompt —
/// who said it and what, capped — or nothing when there is no link or it names an entry that is
/// not in this transcript (deleted, or from another coworker's history). Shared with the room's
/// history (`group.rs`) so a group member sees the same context.
pub(crate) fn reply_context(entries: &[Value], entry: &Value) -> Option<String> {
    let id = entry.get("replyTo").and_then(Value::as_str)?;
    let quoted = entries
        .iter()
        .find(|candidate| candidate.get("id").and_then(Value::as_str) == Some(id))?;
    let (who, text) = match quoted.get("kind").and_then(Value::as_str) {
        Some("message") => (
            if quoted.get("role").and_then(Value::as_str) == Some("user") {
                "their own earlier message".to_string()
            } else {
                "your earlier message".to_string()
            },
            quoted
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        ),
        Some("send-message") => (
            quoted
                .pointer("/author/name")
                .and_then(Value::as_str)
                .map_or_else(
                    || "your earlier message".to_string(),
                    |name| format!("{name}'s earlier message"),
                ),
            quoted
                .pointer("/message/content")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        ),
        _ => return None,
    };
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let clipped = if text.chars().count() > REPLY_QUOTE_CHARS {
        let head: String = text.chars().take(REPLY_QUOTE_CHARS).collect();
        format!("{head}…")
    } else {
        text.to_string()
    };
    Some(format!("[Replying to {who}: \"{clipped}\"]"))
}

/// An answer entry, carrying the turn's reply link when it has one — every rebuild of the answer
/// (the suspension, the final text, the resumed turn) goes through here so no update drops it.
fn answer_entry(id: &str, text: &str, reply_to: Option<&str>) -> Value {
    let mut entry = json!({
        "kind": "send-message",
        "id": id,
        "message": { "type": "text", "content": text },
        "timestampMs": now_ms(),
    });
    if let Some(reply_to) = reply_to {
        entry["replyTo"] = json!(reply_to);
    }
    entry
}

/// The reply link of the turn in progress: the latest user message's `replyTo`. For the path
/// that answers after an approval card, which has no `sendPrompt` arguments in hand.
async fn reply_link_of_the_turn(state: &GatewayState, coworker: &CoworkerId) -> Option<String> {
    let entries = state
        .agui
        .auth
        .store
        .gateway_transcript(coworker)
        .await
        .ok()?;
    entries
        .iter()
        .rev()
        .find(|entry| {
            entry.get("kind").and_then(Value::as_str) == Some("message")
                && entry.get("role").and_then(Value::as_str) == Some("user")
        })
        .and_then(|entry| entry.get("replyTo").and_then(Value::as_str))
        .map(str::to_string)
}

/// The transcript so far, as chat messages — so a coworker remembers its own conversation
/// rather than greeting every message as its first.
pub(crate) async fn history_for(state: &GatewayState, coworker: &CoworkerId) -> Vec<ChatMessage> {
    let Ok(entries) = state.agui.auth.store.gateway_transcript(coworker).await else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| match entry.get("kind").and_then(Value::as_str) {
            Some("message") => {
                let content = entry
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                Some(ChatMessage {
                    role: entry
                        .get("role")
                        .and_then(Value::as_str)
                        .unwrap_or("user")
                        .to_string(),
                    content: match reply_context(&entries, entry) {
                        Some(context) => format!("{context}\n\n{content}"),
                        None => content.to_string(),
                    },
                })
            }
            Some("send-message") => {
                let content = entry
                    .pointer("/message/content")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if content.is_empty() {
                    None
                } else {
                    Some(ChatMessage {
                        role: "assistant".to_string(),
                        content: content.to_string(),
                    })
                }
            }
            _ => None,
        })
        .collect()
}

/// The suspension a run's events carry, if any: which call is waiting, with what, and WHY. The
/// reason picks the card — the tool name no longer can, since two cards can come from one tool.
pub(crate) struct Suspension {
    pub(crate) call_id: String,
    pub(crate) tool: String,
    pub(crate) arguments: Value,
    pub(crate) reason: opengrok_core::run::SuspendReason,
    /// The gate's sentence, when it gave one — a policy grant's reason.
    pub(crate) why: Option<String>,
}

pub(crate) fn find_suspension(events: &[opengrok_wire::agui::Event]) -> Option<Suspension> {
    for event in events {
        if event.event_type == opengrok_wire::agui::EventType::Custom
            && event.extra.get("name").and_then(Value::as_str) == Some("run-awaiting-approval")
        {
            let call_id = event
                .extra
                .get("callId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if call_id.is_empty() {
                continue;
            }
            return Some(Suspension {
                call_id,
                tool: event
                    .extra
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                arguments: event.extra.get("arguments").cloned().unwrap_or(Value::Null),
                reason: opengrok_core::run::SuspendReason::from_stored(
                    event
                        .extra
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                ),
                why: event
                    .extra
                    .get("why")
                    .and_then(Value::as_str)
                    .filter(|why| !why.is_empty())
                    .map(str::to_string),
            });
        }
    }
    None
}

/// The card for a suspension, or `None` when this kind of pause has no card yet. requestId =
/// callId for both cards, threaded back onto the run when the card is answered so every gate
/// converges on one id.
pub(crate) fn card_for(suspension: &Suspension) -> Option<Value> {
    use opengrok_core::run::SuspendReason;
    match suspension.reason {
        // The machine owner's consent: the four-button `local-tool-permission` card, byte-identical
        // to what shipped before reasons existed.
        SuspendReason::ExecConsent if suspension.tool == opengrok_tools::USER_MACHINE_SHELL => {
            let command = suspension
                .arguments
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default();
            Some(json!({
                "kind": "send-message",
                "id": entry_id(),
                "timestampMs": now_ms(),
                "message": {
                    "type": "local-tool-permission",
                    "ask": {
                        "requestId": suspension.call_id,
                        "status": "pending",
                        "action": "run-command",
                        "target": command,
                    },
                },
            }))
        }
        SuspendReason::AutoReview => Some(super::cards::auto_review_card(
            &entry_id(),
            &suspension.call_id,
            "pending",
            &suspension.tool,
            &suspension.arguments,
            Some(opengrok_tools::review::REVIEW_ASK_REASON),
            now_ms(),
        )),
        // A policy grant's "needs a human yes": the same auto-review card, carrying the grant's
        // reason and no proposed rule. Answered by `resolveAutoReviewApproval`, which routes the
        // yes to the GATE (not the judge) by this reason.
        SuspendReason::PolicyApproval => Some(super::cards::policy_approval_card(
            &entry_id(),
            &suspension.call_id,
            "pending",
            &suspension.tool,
            &suspension.arguments,
            suspension.why.as_deref(),
            now_ms(),
        )),
        // Reverse-exec consent on anything but user_machine_shell: nothing renders it.
        _ => None,
    }
}

/// Append a suspension's card and pause the agent. `true` when a card went out; the caller then
/// returns without finalising the turn as an answer.
async fn emit_suspension(
    state: &GatewayState,
    coworker_id: &CoworkerId,
    agent_id: &str,
    suspension: &Suspension,
) -> bool {
    let Some(card) = card_for(suspension) else {
        tracing::warn!(
            tool = %suspension.tool,
            reason = suspension.reason.as_str(),
            "a run suspended for a reason that has no card yet; the turn ends as an answer"
        );
        return false;
    };
    if let Err(error) = state
        .agui
        .auth
        .store
        .append_gateway_entry(coworker_id, &card, now_ms())
        .await
    {
        tracing::error!(%error, "could not append the suspension card entry");
    }
    live::emit_transcript(state, agent_id, "appended", card);
    // The turn is paused, not running. It resumes when the card is answered.
    live::set_running(state, agent_id, false, json!({})).await;
    true
}

/// The sentence a failed run leaves for the person, from the run's own failure event: the
/// gateway's words when it refused (the `error.message` inside its JSON body, when it carries
/// one — "no subscription credential for xai on this route" rather than the whole body), the
/// harness's otherwise; capped. `None` when the run did not fail.
pub(crate) fn failure_sentence(events: &[opengrok_wire::agui::Event]) -> Option<String> {
    let message = events
        .iter()
        .rev()
        .find(|event| event.event_type == opengrok_wire::agui::EventType::RunError)?
        .extra
        .get("message")
        .and_then(Value::as_str)?
        .trim();
    if message.is_empty() {
        return None;
    }
    let said = match message.find('{') {
        Some(at) => {
            let inner = serde_json::from_str::<Value>(&message[at..])
                .ok()
                .and_then(|body| {
                    body.pointer("/error/message")
                        .or_else(|| body.get("message"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
            match inner {
                Some(inner) => format!("{} {inner}", message[..at].trim_end()),
                None => message.to_string(),
            }
        }
        None => message.to_string(),
    };
    let said = said.trim().trim_end_matches('.');
    let capped: String = said.chars().take(300).collect();
    Some(if capped.chars().count() < said.chars().count() {
        format!("{capped}…")
    } else {
        capped
    })
}

/// The answer entry a turn grows into: appended as a streaming placeholder before the turn
/// starts, updated in place as it ends. `reply_to` is the turn's reply link, carried by every
/// rebuild of the entry so the page's "quoted" header survives each update.
pub(crate) struct Answer {
    pub id: String,
    pub seq: i64,
    pub reply_to: Option<String>,
}

pub(crate) async fn run_turn(
    state: GatewayState,
    account_id: opengrok_core::id::AccountId,
    coworker_id: CoworkerId,
    model: String,
    messages: Vec<ChatMessage>,
    answer: Answer,
) {
    let Answer {
        id: answer_id,
        seq: answer_seq,
        reply_to,
    } = answer;
    let agent_id = coworker_id.as_str().to_string();
    let run_id = RunId::new();
    let thread_id = format!("gateway-{agent_id}");

    // The happy path clears `running` at the end. This guard clears it on EVERY other way out —
    // a panic, an error return, or a stopAgentTurn abort — so a turn that dies before its final line
    // can never leave the bot wedged "working" with no run behind it. `finished` is set true once the
    // clean clear has run, so the guard doesn't clear twice.
    let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _turn_guard = TurnGuard {
        state: state.clone(),
        agent_id: agent_id.clone(),
        finished: finished.clone(),
    };

    let tools = crate::agui::routes::tools_for_coworker(
        &state.agui,
        &account_id,
        &coworker_id,
        &[],
        &[],
        crate::agui::routes::TURN_WAKE_PATIENCE,
    )
    .await;
    // Whether this turn can actually reach the user's machine — read from the offered schemas so
    // the prompt can never contradict the tool list again. The enrolled label is decoration on top
    // of that schema-derived fact: fetched only when the tool is truly offered, and its absence
    // just means the prompt speaks generically.
    let reaches_user_machine = tools.as_ref().is_some_and(|runner| {
        runner
            .tool_schemas()
            .iter()
            .any(|schema| schema["function"]["name"] == opengrok_tools::USER_MACHINE_SHELL)
    });
    let user_machine_label = if reaches_user_machine {
        crate::local_exec::enabled_machine(&state.agui.auth.store, account_id.as_str())
            .await
            .map(|(_machine_id, label)| label)
            .filter(|label| !label.trim().is_empty())
    } else {
        None
    };
    let journal = StoreJournal {
        state: state.agui.clone(),
        thread_id: thread_id.clone(),
        account_id: Some(account_id),
        coworker_id: Some(coworker_id.clone()),
        model: Some(model.clone()),
    };
    let request = ModelRequest {
        gateway_key: crate::spend::key_for(&state.agui, &coworker_id).await,
        spend_scope: Some(coworker_id.as_str().to_string()),
        model,
        // A coworker gets a computer of its own, and the user has theirs. Nothing else told the model
        // these are different machines, so it would run a command on its box and call the box "your
        // computer" — the exact confusion a person hits when a file lands "on their machine" that is
        // really on the server. This says, plainly, whose machine the tools touch and to say so.
        system: Some(computer_system_prompt(
            tools.is_some(),
            reaches_user_machine,
            user_machine_label.as_deref(),
        )),
        messages,
        tools: Vec::new(),
    };

    let _lease =
        crate::recovery::Lease::new(crate::recovery::hold(state.agui.clone(), run_id.clone()));
    let events = run_conversation(
        state.agui.door.as_ref(),
        tools.as_ref(),
        &journal,
        request,
        &thread_id,
        run_id.as_str(),
        now_ms(),
    )
    .await;

    // The answer is whatever the run's message deltas add up to; a run that produced nothing
    // still ends its bubble, with the failure said out loud rather than a spinner forever.
    let mut text = String::new();
    for event in &events {
        // Only message content: TOOL_CALL_ARGS frames carry a `delta` too, and gluing tool
        // arguments into the visible answer would be nonsense the user reads.
        if event.event_type == opengrok_wire::agui::EventType::TextMessageContent
            && let Some(delta) = event.extra.get("delta").and_then(Value::as_str)
        {
            text.push_str(delta);
        }
    }
    // A reverse-exec tool suspension: the run paused for the user to approve a command on their OWN
    // machine. That is NOT "no answer" — it is the inline approval card. Finalise whatever text
    // preceded the tool call, emit the pending `local-tool-permission` card entry (the client renders
    // it as the four-button card), and hold the turn. The run aggregate is already suspended by the
    // journal, so resolveLocalToolPermission can resume it. requestId = callId, threaded onto the
    // exec frame when the run resumes so both gates converge on one id.
    if let Some(suspension) = find_suspension(&events) {
        let answer_entry = answer_entry(&answer_id, &text, reply_to.as_deref());
        let _ = state
            .agui
            .auth
            .store
            .update_gateway_entry(&coworker_id, answer_seq, &answer_entry)
            .await;
        live::emit_transcript(&state, &agent_id, "updated", answer_entry);
        if emit_suspension(&state, &coworker_id, &agent_id, &suspension).await {
            finished.store(true, std::sync::atomic::Ordering::SeqCst);
            return;
        }
    }

    if text.is_empty() {
        // The reason in the bubble, not a pointer to the run log: a refusal from the gateway is
        // one sentence the person can act on ("no subscription credential for xai on this
        // route"), and the log line would otherwise be a database dig away.
        text = failure_sentence(&events)
            .map(|why| format!("The turn failed: {why}"))
            .unwrap_or_else(|| {
                "The turn produced no answer. Its run log has the reason.".to_string()
            });
    }

    let final_entry = answer_entry(&answer_id, &text, reply_to.as_deref());
    if let Err(error) = state
        .agui
        .auth
        .store
        .update_gateway_entry(&coworker_id, answer_seq, &final_entry)
        .await
    {
        tracing::error!(%error, "could not finalise the answer entry");
    }
    live::emit_transcript(&state, &agent_id, "updated", final_entry);

    let preview: String = text.chars().take(120).collect();
    live::set_running(
        &state,
        &agent_id,
        false,
        json!({
            "lastMessagePreview": preview,
            "lastEntry": { "kind": "text", "text": preview },
        }),
    )
    .await;
    // The clean clear ran; tell the guard not to clear again.
    finished.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Clears an agent's `running` flag on any abnormal exit of `run_turn` (panic, error, or a
/// stopAgentTurn abort) and drops the turn's cancel handle. See the note where it is constructed.
struct TurnGuard {
    state: GatewayState,
    agent_id: String,
    finished: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        if let Ok(mut cancels) = self.state.cancels.lock() {
            cancels.remove(&self.agent_id);
        }
        if !self.finished.load(std::sync::atomic::Ordering::SeqCst) {
            let state = self.state.clone();
            let agent_id = self.agent_id.clone();
            tokio::spawn(async move {
                live::set_running(&state, &agent_id, false, json!({})).await;
            });
        }
    }
}

/// `openAgentTail` / `getAgentTranscriptTail` / windows / pages — every read is the same tail
/// with different dressing.
pub async fn transcript_reply(
    state: &GatewayState,
    args: &Value,
    activate: bool,
    with_thread_counts: bool,
) -> (u16, Value) {
    let Some(agent_id) = agent_or_active(state, args) else {
        return (400, json!({ "error": "no agent named and none active" }));
    };
    let coworker = CoworkerId::from_stored(agent_id.clone());
    let before = args.get("beforeSeq").and_then(Value::as_i64);
    let limit = args
        .get("limit")
        .and_then(Value::as_i64)
        .unwrap_or(200)
        .clamp(1, 1000);

    let (entries, next_before) = match state
        .agui
        .auth
        .store
        .gateway_tail(&coworker, before, limit)
        .await
    {
        Ok(page) => page,
        Err(error) => {
            tracing::error!(%error, "could not read a transcript tail");
            return (500, json!({ "error": "transcript unavailable" }));
        }
    };

    if activate {
        if let Ok(mut active) = state.active_agent.lock() {
            *active = Some(agent_id.clone());
        }
        // Opening an agent is a roster fact (the active pip moved), and §8.3 allows the
        // escalation to a full emit.
        live::emit_roster(state).await;
    }

    let mut reply = json!({ "entries": entries });
    if let Some(next) = next_before {
        reply["nextBeforeSeq"] = json!(next);
    }
    if with_thread_counts {
        // Required by the window validator even when no entry is threaded: absent means the
        // whole reply is malformed and the call rejects.
        reply["threadCounts"] = json!({});
    }
    (200, reply)
}

/// `getAgentTranscript` / `getTranscript` / `openAgent` — the unbounded array forms.
pub async fn full_transcript(state: &GatewayState, args: &Value, activate: bool) -> (u16, Value) {
    let Some(agent_id) = agent_or_active(state, args) else {
        return (400, json!({ "error": "no agent named and none active" }));
    };
    let coworker = CoworkerId::from_stored(agent_id.clone());
    match state.agui.auth.store.gateway_transcript(&coworker).await {
        Ok(entries) => {
            if activate {
                if let Ok(mut active) = state.active_agent.lock() {
                    *active = Some(agent_id);
                }
                live::emit_roster(state).await;
            }
            (200, Value::Array(entries))
        }
        Err(error) => {
            tracing::error!(%error, "could not read a transcript");
            (500, json!({ "error": "transcript unavailable" }))
        }
    }
}

/// `promptAcceptanceStatus` — what the ledger remembers about a nonce.
pub async fn acceptance_status(state: &GatewayState, args: &Value, caller: &str) -> (u16, Value) {
    let Some(nonce) = args.get("clientNonce").and_then(Value::as_str) else {
        return (
            400,
            json!({ "error": "promptAcceptanceStatus needs a clientNonce" }),
        );
    };
    match state.agui.auth.store.nonce_record(caller, nonce).await {
        Ok(Some(record)) => (200, json!({ "outcome": "found", "record": record })),
        Ok(None) => (200, json!({ "outcome": "not-found" })),
        Err(error) => {
            tracing::error!(%error, "the acceptance ledger is unreachable");
            (500, json!({ "error": "acceptance ledger unavailable" }))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The inline approval card's answer (reverse-exec increment B). The card the bot posted on Ask is
// answered here: allow → resume the turn so the command dispatches (with approvalId = callId, which
// the card already recorded the machine-side approval under) and the bot's own summary lands in the
// transcript; deny → cancel. Either way the card entry is re-emitted with its outcome status, which
// flips the client's four buttons into the outcome line.
// ---------------------------------------------------------------------------------------------

/// `resolveLocalToolPermission` — `{ entryId, requestId(=callId), resolution, agentId }`.
/// A card already settled — by an earlier press, or by another device — answers "already
/// answered" rather than being healed to "expired". The run itself has left the awaiting list by
/// then, so the card's own durable status is the only thing that can tell a double-click from a
/// dead request. `path` is the card's status field: `ask` for the exec card, `approval` for the
/// auto-review card.
async fn card_already_settled(
    state: &GatewayState,
    coworker_id: &CoworkerId,
    entry_id: &str,
    path: &str,
) -> bool {
    if entry_id.is_empty() {
        return false;
    }
    match state
        .agui
        .auth
        .store
        .find_gateway_entry(coworker_id, entry_id)
        .await
    {
        Ok(Some((_, entry))) => entry["message"][path]["status"]
            .as_str()
            .is_some_and(|status| status != "pending"),
        _ => false,
    }
}

pub async fn resolve_local_tool_permission(
    state: &GatewayState,
    args: &Value,
    caller: &str,
) -> (u16, Value) {
    let entry_id = args
        .get("entryId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let request_id = args
        .get("requestId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let resolution = args
        .get("resolution")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if request_id.is_empty() || resolution.is_empty() {
        return (
            400,
            json!({ "error": "requestId and resolution are required" }),
        );
    }
    let Some(agent_id) = agent_or_active(state, args) else {
        return (400, json!({ "error": "no agent named and none active" }));
    };
    let coworker_id = CoworkerId::from_stored(agent_id.clone());
    let Ok(Some(account)) = state.agui.auth.store.account_by_email(caller).await else {
        return (
            401,
            json!({ "error": "the gateway account does not exist yet" }),
        );
    };
    let account_id = account.id;

    // Find the suspended run whose pending call is this requestId (= the tool call id).
    let run_ids = state
        .agui
        .auth
        .store
        .awaiting_approval(&account_id)
        .await
        .unwrap_or_default();
    let mut found = None;
    for run_id in run_ids {
        if let Ok((run, seq)) = state.agui.auth.store.load_run(&run_id).await
            && run_belongs_to(&run, &coworker_id)
            && run.pending.as_ref().map(|p| p.call_id.as_str()) == Some(request_id.as_str())
            // The wrong verb must not settle the other card: this one answers the machine
            // owner's consent, never an auto-review ask and never a policy ask (both ride the
            // auto-review card and its verb).
            && run.pending.as_ref().map(|p| p.reason)
                == Some(opengrok_core::run::SuspendReason::ExecConsent)
        {
            found = Some((run_id, run, seq));
            break;
        }
    }
    let Some((run_id, mut run, seq)) = found else {
        // A second press on a card that already settled — the run has left the awaiting list,
        // which is exactly what an answered run does. Not a dead request; do not heal it.
        if card_already_settled(state, &coworker_id, &entry_id, "ask").await {
            return (200, json!({ "alreadyAnswered": true }));
        }
        // The card names a request no run is waiting on — its run died (a crash, a sweep, a
        // restart) without ever answering. Left alone the card is a trap: every control on it
        // posts here, and a bare 404 leaves it rendered as answerable, eating presses forever.
        // Flip ONLY the entry's ask.status so the press itself heals the card (the command it
        // showed stays intact), and say plainly what happened. Asks never expire on their own —
        // this path is only for a request whose run is genuinely gone.
        if !entry_id.is_empty()
            && let Ok(Some(card)) = state
                .agui
                .auth
                .store
                .set_gateway_ask_status(&coworker_id, &entry_id, "expired")
                .await
        {
            live::emit_transcript(state, &agent_id, "updated", card);
        }
        return (
            410,
            json!({ "error": "this request is no longer pending — ask the bot again" }),
        );
    };

    // Allow once / always / this-session → approve; never / deny → refuse. (A client may
    // downgrade a blocked "always" to "allow-once" before it reaches here — both approve.)
    let approved = matches!(
        resolution.as_str(),
        "always" | "allow-once" | "allow-session"
    );
    let pending = run.pending.clone();
    let command = pending
        .as_ref()
        .and_then(|p| p.arguments.get("command"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let resumed_seq = run.emitted.len() as u32;

    // Answer the run aggregate — exactly once; a second click is refused by the aggregate.
    let at_ms = now_ms();
    let events = match run.decide(opengrok_core::run::RunCommand::Answer {
        call_id: request_id.clone(),
        approved,
        by: account_id.to_string(),
        at_ms,
    }) {
        Ok(events) => events,
        Err(opengrok_core::run::RunError::AlreadyAnswered) => {
            return (200, json!({ "alreadyAnswered": true }));
        }
        Err(error) => return (409, json!({ "error": error.to_string() })),
    };
    for event in &events {
        run.apply(event);
    }
    let view = opengrok_core::run::RunView {
        id: run_id.clone(),
        thread_id: run.thread_id.clone(),
        status: run.status,
        event_count: run.emitted.len() as i64,
        updated_at_ms: at_ms,
    };
    if let Err(error) = state
        .agui
        .auth
        .store
        .append_run(&run_id, seq, &events, &view, Some(&account_id))
        .await
    {
        return (503, json!({ "error": error.to_string() }));
    }

    // "Always" and "never" are standing decisions, not one-offs: persist them as policy rules so
    // THE GATE answers this command itself next time — allow-once/denied stay per-call. The rule
    // pattern is the exact command (word-boundary prefix match in `decide`), so it can only cover
    // this command and its argument extensions, never a lookalike that shares a prefix. Written
    // only after the answer is durably recorded, and best-effort: a failed write just means the
    // gate asks again next time, which is the narrower outcome. `standing_rule_refusal` is the
    // same skip for sudo-as-allow (a standing allow on `sudo` would cover `sudo rm -rf /`);
    // deny of sudo still persists.
    let standing = match resolution.as_str() {
        "always" if !crate::local_exec::prefer_session_allow(&command) => Some("allow"),
        "never" => Some("deny"),
        _ => None,
    };
    if let Some(kind) = standing
        && !command.is_empty()
        && crate::local_exec::standing_rule_refusal(kind, &command).is_none()
        && let Some((machine_id, _label)) =
            crate::local_exec::enabled_machine(&state.agui.auth.store, account_id.as_str()).await
    {
        let _ = state
            .agui
            .auth
            .store
            .add_local_exec_rule(account_id.as_str(), &machine_id, kind, &command, at_ms)
            .await;
    }
    let session = resolution == "allow-session"
        || (resolution == "always" && crate::local_exec::prefer_session_allow(&command));
    if session
        && !command.is_empty()
        && crate::local_exec::standing_rule_refusal("allow", &command).is_none()
        && let Some((machine_id, _label)) =
            crate::local_exec::enabled_machine(&state.agui.auth.store, account_id.as_str()).await
    {
        state
            .agui
            .auth
            .local_exec
            .remember_session_allow(account_id.as_str(), &machine_id, &command)
            .await;
    }

    // Flip the card to its outcome status — same entry id, new ask.status, so the client re-renders
    // the row from buttons to the outcome line.
    let status = match resolution.as_str() {
        "always" => "always",
        "allow-session" => "allow-session",
        "allow-once" => "allow-once",
        "never" => "never",
        _ => "denied",
    };
    let mut card = json!({
        "kind": "send-message",
        "id": entry_id,
        "timestampMs": now_ms(),
        "message": {
            "type": "local-tool-permission",
            "ask": {
                "requestId": request_id,
                "status": status,
                "action": "run-command",
                "target": command,
            },
        },
    });
    // A member's card in a room keeps its author through the flip.
    if let Ok(Some((_, existing))) = state
        .agui
        .auth
        .store
        .find_gateway_entry(&coworker_id, &entry_id)
        .await
        && let Some(author) = existing.get("author")
    {
        card["author"] = author.clone();
    }
    let _ = state
        .agui
        .auth
        .store
        .update_gateway_entry_by_id(&coworker_id, &entry_id, &card)
        .await;
    live::emit_transcript(state, &agent_id, "updated", card);

    // Carry the turn on in the background EITHER WAY: on approval the resumed run dispatches the
    // command and the model's own summary lands in the transcript; on refusal the model is told
    // the owner declined, as a result it can reason about (CLAUDE.md #8), rather than the run
    // being left answered-but-silent.
    if let Some(pending) = pending {
        let outcome = if approved {
            opengrok_harness::ResumeOutcome::Approved
        } else {
            opengrok_harness::ResumeOutcome::Refused(
                "the machine's owner declined to run this command".to_string(),
            )
        };
        let state = state.clone();
        tokio::spawn(resume_where_it_lives(
            in_a_room(&run, &coworker_id),
            state,
            account_id,
            run_id,
            coworker_id,
            agent_id,
            pending,
            resumed_seq,
            outcome,
        ));
    }

    (200, json!({ "ok": true }))
}

/// `resolveAutoReviewApproval {entryId, requestId, resolution, agentId}` — the auto-review card's
/// answer. Mirrors `resolve_local_tool_permission` step for step; the differences are the card
/// path it flips (`message.approval.status`), the suspension reasons it will settle (the judge's
/// ask AND a policy grant's ask — both ride this card; the resume routes the yes by reason), and
/// that it writes no standing rule — the card's "Always" is client-side, which appends the
/// proposed rule to the coworker tier through `PUT /auto-review/policy` and then sends
/// `approved`; a policy card carries no rule, so there "Always" is a plain approve.
pub async fn resolve_auto_review_approval(
    state: &GatewayState,
    args: &Value,
    caller: &str,
) -> (u16, Value) {
    let entry_id = args
        .get("entryId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let request_id = args
        .get("requestId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    // `resolution` on the wire; `status` accepted too (the transcription table records the
    // answer as {requestId, status}) — one line, and it cannot break the contract.
    let word = args
        .get("resolution")
        .or_else(|| args.get("status"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if request_id.is_empty() || word.is_empty() {
        return (
            400,
            json!({ "error": "requestId and resolution are required" }),
        );
    }
    let Some(agent_id) = agent_or_active(state, args) else {
        return (400, json!({ "error": "no agent named and none active" }));
    };
    let coworker_id = CoworkerId::from_stored(agent_id.clone());
    let Ok(Some(account)) = state.agui.auth.store.account_by_email(caller).await else {
        return (
            401,
            json!({ "error": "the gateway account does not exist yet" }),
        );
    };
    let account_id = account.id;

    let run_ids = state
        .agui
        .auth
        .store
        .awaiting_approval(&account_id)
        .await
        .unwrap_or_default();
    let mut found = None;
    for run_id in run_ids {
        if let Ok((run, seq)) = state.agui.auth.store.load_run(&run_id).await
            && run_belongs_to(&run, &coworker_id)
            && run.pending.as_ref().map(|p| p.call_id.as_str()) == Some(request_id.as_str())
            && matches!(
                run.pending.as_ref().map(|p| p.reason),
                Some(
                    opengrok_core::run::SuspendReason::AutoReview
                        | opengrok_core::run::SuspendReason::PolicyApproval
                )
            )
        {
            found = Some((run_id, run, seq));
            break;
        }
    }
    let Some((run_id, mut run, seq)) = found else {
        if card_already_settled(state, &coworker_id, &entry_id, "approval").await {
            return (200, json!({ "alreadyAnswered": true }));
        }
        // Same heal as the exec card: the press flips ONLY approval.status to "expired" and the
        // client reads 410 as stale. Asks never expire on their own.
        if !entry_id.is_empty()
            && let Ok(Some(card)) = state
                .agui
                .auth
                .store
                .set_gateway_approval_status(&coworker_id, &entry_id, "expired")
                .await
        {
            live::emit_transcript(state, &agent_id, "updated", card);
        }
        return (
            410,
            json!({ "error": "this request is no longer pending — ask the bot again" }),
        );
    };

    let approved = matches!(word.as_str(), "approved" | "always" | "allow-once");
    let pending = run.pending.clone();
    let resumed_seq = run.emitted.len() as u32;
    // Hold the MCP per-coworker lock across Answer/remember/Finish so a retry cannot
    // take-or-run between those steps and leave a leftover yes.
    let mcp_lock = run
        .thread_id
        .starts_with("mcp-")
        .then(|| crate::mcp_door::coworker_lock(&coworker_id));
    let _mcp_guard = if let Some(lock) = mcp_lock.as_ref() {
        Some(lock.lock().await)
    } else {
        None
    };

    let at_ms = now_ms();
    let events = match run.decide(opengrok_core::run::RunCommand::Answer {
        call_id: request_id.clone(),
        approved,
        by: account_id.to_string(),
        at_ms,
    }) {
        Ok(events) => events,
        Err(opengrok_core::run::RunError::AlreadyAnswered) => {
            return (200, json!({ "alreadyAnswered": true }));
        }
        Err(error) => return (409, json!({ "error": error.to_string() })),
    };
    for event in &events {
        run.apply(event);
    }
    let view = opengrok_core::run::RunView {
        id: run_id.clone(),
        thread_id: run.thread_id.clone(),
        status: run.status,
        event_count: run.emitted.len() as i64,
        updated_at_ms: at_ms,
    };
    let seq = match state
        .agui
        .auth
        .store
        .append_run(&run_id, seq, &events, &view, Some(&account_id))
        .await
    {
        Ok(seq) => seq,
        Err(error) => return (503, json!({ "error": error.to_string() })),
    };

    // Flip the card on the SAME entryId — the renderer dedups on requestId:status.
    let status = if approved { "approved" } else { "denied" };
    if !entry_id.is_empty()
        && let Ok(Some(card)) = state
            .agui
            .auth
            .store
            .set_gateway_approval_status(&coworker_id, &entry_id, status)
            .await
    {
        live::emit_transcript(state, &agent_id, "updated", card);
    }

    // An MCP-synthesized run is not a conversation. Resuming it would execute the tool on this
    // side while the MCP client is told to retry (double-run, and a new call id that cannot
    // spend this yes). Finish it here; an approved allow-once is remembered so the retry
    // reuses the answered call id and the judge skip actually fires.
    if run.thread_id.starts_with("mcp-") {
        if approved
            && let Some(pending) = pending.as_ref()
            && let Err(error) = crate::mcp_door::remember_mcp_allow_once(
                &state.agui.auth.store,
                &coworker_id,
                &pending.tool,
                &pending.arguments,
                &pending.call_id,
                // A policy yes releases the GATE on the retry; a judge yes skips the judge.
                pending.reason != opengrok_core::run::SuspendReason::AutoReview,
            )
            .await
        {
            // The card is answered either way; a lost yes means the retry asks again.
            tracing::error!(%error, call_id = %pending.call_id, "approve: the yes could not be remembered for the MCP retry");
        }
        if let Ok(finished) = run.decide(opengrok_core::run::RunCommand::Finish { at_ms }) {
            for event in &finished {
                run.apply(event);
            }
            let view = opengrok_core::run::RunView {
                id: run_id.clone(),
                thread_id: run.thread_id.clone(),
                status: run.status,
                event_count: run.emitted.len() as i64,
                updated_at_ms: at_ms,
            };
            if let Err(error) = state
                .agui
                .auth
                .store
                .append_run(&run_id, seq, &finished, &view, Some(&account_id))
                .await
            {
                // Answer and the card already landed; 503 here would leave the card
                // approved and the retry token set, with a press that never retries Finish.
                tracing::error!(%error, "mcp ask: could not finish the synthesized run");
            }
        }
        return (200, json!({ "ok": true }));
    }

    if let Some(pending) = pending {
        let outcome = if approved {
            opengrok_harness::ResumeOutcome::Approved
        } else if pending.reason == opengrok_core::run::SuspendReason::PolicyApproval {
            // The refusal names the rule, so the model (and the transcript) say why rather than
            // "declined" alone: this was the coworker's policy asking, not the judge.
            opengrok_harness::ResumeOutcome::Refused(format!(
                "the user declined this on the approval card: the coworker's policy needs a \
                 person's yes before it may run {}",
                pending.tool
            ))
        } else {
            opengrok_harness::ResumeOutcome::Refused(
                "the user declined this on the auto-review card".to_string(),
            )
        };
        let state = state.clone();
        tokio::spawn(resume_where_it_lives(
            in_a_room(&run, &coworker_id),
            state,
            account_id,
            run_id,
            coworker_id,
            agent_id,
            pending,
            resumed_seq,
            outcome,
        ));
    }

    (200, json!({ "ok": true }))
}

/// Whether a run is this agent's to answer: its own, or a member's run inside this ROOM. A
/// member's turn in a group runs on the room's thread (`gateway-{group}`) under the member's own
/// id, and its card sits in the room's transcript, so the desktop answers it naming the group.
fn run_belongs_to(run: &opengrok_core::run::Run, agent: &CoworkerId) -> bool {
    run.coworker_id
        .as_ref()
        .is_some_and(|owner| owner.as_str() == agent.as_str())
        || run.thread_id == format!("gateway-{}", agent.as_str())
}

/// A run answered under an agent that is not its owner is a member's run inside that room.
fn in_a_room(run: &opengrok_core::run::Run, agent: &CoworkerId) -> bool {
    run.coworker_id
        .as_ref()
        .is_some_and(|owner| owner.as_str() != agent.as_str())
}

/// A resumed run continues where it lives: a coworker's own turn lands in its transcript; a
/// member's turn goes back to the room, which then finishes its round (`group.rs`).
#[allow(clippy::too_many_arguments)]
async fn resume_where_it_lives(
    in_a_room: bool,
    state: GatewayState,
    account_id: opengrok_core::id::AccountId,
    run_id: RunId,
    coworker_id: CoworkerId,
    agent_id: String,
    pending: opengrok_core::run::PendingApproval,
    resumed_seq: u32,
    outcome: opengrok_harness::ResumeOutcome,
) {
    if in_a_room {
        super::group::resume_member_turn(
            state,
            account_id,
            run_id,
            coworker_id,
            pending,
            resumed_seq,
            outcome,
        )
        .await;
    } else {
        resume_gateway_run(
            state,
            account_id,
            run_id,
            coworker_id,
            agent_id,
            pending,
            resumed_seq,
            outcome,
        )
        .await;
    }
}

/// Resume an approved gateway run: re-run the conversation with the approved tool call (which makes
/// `user_machine_shell` dispatch instead of re-asking), then land the model's summary in the
/// transcript as an ordinary bot message.
#[allow(clippy::too_many_arguments)]
async fn resume_gateway_run(
    state: GatewayState,
    account_id: opengrok_core::id::AccountId,
    run_id: RunId,
    coworker_id: CoworkerId,
    agent_id: String,
    pending: opengrok_core::run::PendingApproval,
    resumed_seq: u32,
    outcome: opengrok_harness::ResumeOutcome,
) {
    let Ok((run, _)) = state.agui.auth.store.load_run(&run_id).await else {
        return;
    };
    let Ok((coworker, _)) = state.agui.auth.store.load_coworker(&coworker_id).await else {
        return;
    };
    // The runner carries the answered call id — as a GATE approval (the machine owner's or the
    // policy's card) or a REVIEW approval, by the suspension's reason. A review yes skips the
    // judge and releases nothing else; a gate yes is what makes user_machine_shell dispatch.
    let (gate_yes, review_yes): (&[String], &[String]) = match pending.reason {
        opengrok_core::run::SuspendReason::AutoReview => {
            (&[], std::slice::from_ref(&pending.call_id))
        }
        _ => (std::slice::from_ref(&pending.call_id), &[]),
    };
    let Some(runner) = crate::agui::routes::tools_for_coworker(
        &state.agui,
        &account_id,
        &coworker_id,
        gate_yes,
        review_yes,
        crate::agui::routes::TURN_WAKE_PATIENCE,
    )
    .await
    else {
        return;
    };
    let journal = crate::agui::routes::StoreJournal {
        state: state.agui.clone(),
        thread_id: run.thread_id.clone(),
        account_id: Some(account_id.clone()),
        coworker_id: Some(coworker_id.clone()),
        model: run.model.clone(),
    };
    let request = ModelRequest {
        gateway_key: crate::spend::key_for(&state.agui, &coworker_id).await,
        spend_scope: Some(coworker_id.as_str().to_string()),
        model: run.pin_for_resume(&coworker.model),
        system: None,
        messages: crate::agui::routes::conversation_from(&run),
        tools: Vec::new(),
    };

    live::set_running(&state, &agent_id, true, json!({})).await;
    let events = opengrok_harness::resume_conversation(
        state.agui.door.as_ref(),
        &runner,
        &journal,
        request,
        opengrok_harness::RunContext::new(&run.thread_id, run_id.as_str(), now_ms()),
        opengrok_harness::Resumption {
            approved: opengrok_tools::ToolCall {
                id: pending.call_id,
                name: pending.tool,
                arguments: pending.arguments,
            },
            message_seq: resumed_seq,
            outcome,
        },
    )
    .await;

    let mut text = String::new();
    for event in &events {
        if event.event_type == opengrok_wire::agui::EventType::TextMessageContent
            && let Some(delta) = event.extra.get("delta").and_then(Value::as_str)
        {
            text.push_str(delta);
        }
    }
    if text.is_empty()
        && let Some(why) = failure_sentence(&events)
    {
        text = format!("The turn failed: {why}");
    }
    if !text.is_empty() {
        let reply_to = reply_link_of_the_turn(&state, &coworker_id).await;
        let answer = answer_entry(&entry_id(), &text, reply_to.as_deref());
        if let Ok(_seq) = state
            .agui
            .auth
            .store
            .append_gateway_entry(&coworker_id, &answer, now_ms())
            .await
        {
            live::emit_transcript(&state, &agent_id, "appended", answer);
        }
    }
    // A resumed run may suspend AGAIN — a second command, or the next reviewed tool. It gets its
    // card exactly like the first turn did; without this the run paused with nothing to press.
    if let Some(suspension) = find_suspension(&events)
        && emit_suspension(&state, &coworker_id, &agent_id, &suspension).await
    {
        return;
    }

    let preview: String = text.chars().take(120).collect();
    live::set_running(
        &state,
        &agent_id,
        false,
        json!({ "lastMessagePreview": preview, "lastEntry": { "kind": "text", "text": preview } }),
    )
    .await;
}
