//! The gateway's live layer: ordering stamps and the roster/transcript emits.
//!
//! ORDERING IS A PROMISE, NOT DECORATION. Every roster and transcript frame carries
//! `ordered: {replicaKey, epoch, sequence}` (`client-grok-bot.md` §4.5): the client watches for
//! sequence gaps and epoch changes and treats either as a stale replica. The epoch is minted per
//! process, so a restart *announces itself* instead of quietly renumbering.
//!
//! TWO RULES THE CLIENT'S REPLICA IMPOSES, learned from a night of "no reply until Cmd+R"
//! (2 Sep 2026):
//! 1. **A sequence is minted only together with the frame that carries it, and sent under the
//!    same lock.** `ordered()` used to hand out a number and let the caller send later; two
//!    tasks on one agent could mint 7 and 8 and send 8 first, and a gap is a resync the client
//!    could not complete. `emit_ordered` is now the only way to mint.
//! 2. **A frame written to ONE subscriber must not consume a sequence.** The `/events` opening
//!    roster snapshot used to mint one; every other open stream then saw N, N+2 and resynced the
//!    roster after every send. The opener now carries `current()` — the sequence everyone has
//!    already seen — which the replica installs as its baseline and nobody else notices.

use serde_json::{Value, json};

use super::{GatewayState, summaries};

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn stamp(state: &GatewayState, replica_key: &str, sequence: i64) -> Value {
    json!({ "replicaKey": replica_key, "epoch": state.epoch, "sequence": sequence })
}

/// The stamp a NEW subscriber is seeded with: the sequence every existing subscriber has already
/// seen, NOT a fresh one. A per-subscriber frame that minted would be a gap for everyone else.
pub fn current(state: &GatewayState, replica_key: &str) -> Value {
    let sequence = state
        .seqs
        .lock()
        .ok()
        .and_then(|seqs| seqs.get(replica_key).copied())
        .unwrap_or(0);
    stamp(state, replica_key, sequence)
}

/// Mint the next sequence for `replica_key`, build the frame around it, and send it to every open
/// `/events` stream — all under the sequence lock, so two emits on one key reach the broadcast in
/// the order they were numbered. `send` is synchronous and never blocks, which is what makes
/// holding a std mutex across it acceptable. No subscriber is an ordinary morning, not an error.
pub fn emit_ordered(
    state: &GatewayState,
    channel: &str,
    replica_key: &str,
    build: impl FnOnce(Value) -> Value,
) {
    let Ok(mut seqs) = state.seqs.lock() else {
        // A poisoned sequence lock is a bug elsewhere; a frame with a made-up number would only
        // teach the client to distrust the replica. Say so and drop this one.
        tracing::error!(
            channel,
            replica_key,
            "live: sequence lock poisoned; frame dropped"
        );
        return;
    };
    let seq = seqs.entry(replica_key.to_string()).or_insert(0);
    *seq += 1;
    let payload = build(stamp(state, replica_key, *seq));
    let _ = state.events_tx.send((channel.to_string(), payload));
}

fn active_agent_id(state: &GatewayState) -> Value {
    state
        .active_agent
        .lock()
        .ok()
        .and_then(|active| active.clone())
        .map(Value::from)
        .unwrap_or(Value::Null)
}

/// One §8.1 row with the live run-state and the stored profile overlaid — what a static
/// projection cannot know, and what the profile row knows better.
async fn live_summary(state: &GatewayState, view: &opengrok_core::coworker::CoworkerView) -> Value {
    let mut row = summaries::summary(view);
    if let Ok(Some(profile)) = state.agui.auth.store.seamb_profile(&view.id).await {
        // The model stays the description's fallback — it is also the blank-agent defence.
        if let Some(description) = profile.get("description").and_then(Value::as_str)
            && !description.is_empty()
        {
            row["description"] = json!(description);
        }
        for (key, target) in [
            ("title", "title"),
            ("avatarShape", "avatarShape"),
            ("avatarColor", "avatarColor"),
            ("avatarVersion", "avatarVersion"),
            ("avatarDataUrl", "avatarDataUrl"),
        ] {
            if let Some(value) = profile.get(key)
                && !value.is_null()
            {
                row[target] = value.clone();
            }
        }
    }
    let running = state
        .running
        .lock()
        .map(|set| set.contains(view.id.as_str()))
        .unwrap_or(false);
    row["isRunning"] = json!(running);
    row["isRunningTurn"] = json!(running);
    if running {
        row["currentActivity"] = json!({ "kind": "thinking" });
    }
    if let Ok(active) = state.active_agent.lock()
        && active.as_deref() == Some(view.id.as_str())
    {
        row["isActive"] = json!(true);
    }
    row
}

/// The full-roster snapshot frame, and the rows it carried — `agents` channel.
pub async fn emit_roster(state: &GatewayState) {
    let Ok(rows) = roster_rows(state).await else {
        return;
    };
    let active = active_agent_id(state);
    emit_ordered(state, "agents", "roster", |ordered| {
        json!({
            "activeAgentId": active,
            "agents": rows,
            "ordered": ordered,
            "coverage": { "kind": "complete-roster" },
        })
    });
}

/// A single-row delta — `agent-upserted`. `patch` overlays fields the caller knows better than
/// the projection does (a fresh `lastMessagePreview`, a moved `updatedAt`).
pub async fn emit_agent_upserted(state: &GatewayState, coworker_id: &str, patch: Value) {
    let Ok(rows) = roster_rows(state).await else {
        return;
    };
    let Some(mut row) = rows.into_iter().find(|row| row["id"] == coworker_id) else {
        return;
    };
    if let (Some(target), Some(overlay)) = (row.as_object_mut(), patch.as_object()) {
        for (key, value) in overlay {
            target.insert(key.clone(), value.clone());
        }
    }
    let active = active_agent_id(state);
    emit_ordered(state, "agent-upserted", "roster", |ordered| {
        json!({
            "activeAgentId": active,
            "agent": row,
            "ordered": ordered,
        })
    });
}

/// A frame that carries NO ordering stamp, on a channel no replica tracks (`agents-automation`:
/// the coordinator maps it to the renderer's `automations` family, whose own emitter posts
/// `{agentId, automations}` with no stamp — `routed-automations.ts:180`). Minting a sequence for
/// such a frame on a replica key would be a gap for that replica on every send, the class #14
/// removed; this is the deliberate way to say "not ordered".
pub fn emit_unstamped(state: &GatewayState, channel: &str, payload: Value) {
    let _ = state.events_tx.send((channel.to_string(), payload));
}

/// A transcript frame for one agent — `appended` or `updated`, stamped on that agent's replica.
pub fn emit_transcript(state: &GatewayState, agent_id: &str, kind: &str, entry: Value) {
    emit_ordered(
        state,
        "transcript",
        &format!("transcript:{agent_id}"),
        |ordered| {
            json!({
                "type": kind,
                "entry": entry,
                "agentId": agent_id,
                "ordered": ordered,
            })
        },
    );
}

/// A transcript `removed` frame for one entry id, stamped on that agent's replica.
pub fn emit_transcript_removed(state: &GatewayState, agent_id: &str, entry_id: &str) {
    emit_ordered(
        state,
        "transcript",
        &format!("transcript:{agent_id}"),
        |ordered| {
            json!({
                "type": "removed",
                "id": entry_id,
                "agentId": agent_id,
                "ordered": ordered,
            })
        },
    );
}

/// The live roster rows for the gateway's default account (`state.email`). Used by the SSE emitters,
/// which fire outside any request and so have no caller — the desktop is one person per connection.
pub async fn roster_rows(state: &GatewayState) -> Result<Vec<Value>, opengrok_store::StoreError> {
    roster_rows_for(state, &state.email).await
}

/// The live roster rows for a specific account email — the per-account pivot's core. Request
/// handlers pass the CALLER's email (resolved from the account token) so `listAgents` returns the
/// signed-in person's coworkers, not the fixed `OG_GATEWAY_EMAIL`.
pub async fn roster_rows_for(
    state: &GatewayState,
    email: &str,
) -> Result<Vec<Value>, opengrok_store::StoreError> {
    let Some(account) = state.agui.auth.store.account_by_email(email).await? else {
        return Ok(Vec::new());
    };
    // Ownership, not visibility. `coworkers_for` is ALSO the authorisation primitive that
    // `owned_coworker` gates every per-coworker route on, so widening it here would silently make
    // every org member able to write every other member's limits. The org-visible rows will come
    // from a separate `roster_for`, added when transcripts are keyed per member — until then a
    // shared coworker would put two people in one conversation, which is the thing S2 forbids.
    let coworkers = state.agui.auth.store.coworkers_for(&account.id).await?;

    // The account's provisioning error (if any) is stamped on its BOXLESS agents, so the roster can
    // say why a bot has no computer. An agent that has a box carries null.
    let account_error = state
        .agui
        .auth
        .store
        .account_computer_error(account.id.as_str())
        .await
        .ok()
        .flatten();
    let mut rows = Vec::new();
    for view in coworkers.iter().filter(|view| !view.retired) {
        let mut row = live_summary(state, view).await;
        // A group has members instead of a computer: the account's provisioning error is not
        // its problem, and a "no computer" note on a group would send a person chasing one.
        row["computerError"] = if view.box_id.is_none() && view.members.is_empty() {
            crate::agui::provision::error_json(&account_error)
        } else {
            Value::Null
        };
        // The permission fields, decided by the server on every row (S2). `mine` is ownership;
        // `canManage` is the owner or the org's admin; `owner` names the hirer so a shared row
        // can say whose it is. Every row here is the caller's own today, so `mine` is true —
        // the fields exist and are honest now so the desktop needs no change when the roster
        // widens.
        row["visibility"] = json!(view.visibility.as_str());
        row["mine"] = json!(true);
        row["canManage"] = json!(true);
        row["owner"] = json!({
            "id": account.id.as_str(),
            "name": format!("{} {}", account.first_name, account.last_name).trim(),
        });
        rows.push(row);
    }
    Ok(rows)
}

/// Flip a coworker's running state and tell the roster about it.
pub async fn set_running(state: &GatewayState, coworker_id: &str, running: bool, patch: Value) {
    if let Ok(mut set) = state.running.lock() {
        if running {
            set.insert(coworker_id.to_string());
        } else {
            set.remove(coworker_id);
        }
    }
    let mut overlay = patch;
    if overlay.as_object().is_none() {
        overlay = json!({});
    }
    if let Some(map) = overlay.as_object_mut() {
        map.insert("updatedAt".to_string(), json!(now_ms()));
    }
    emit_agent_upserted(state, coworker_id, overlay).await;
}
