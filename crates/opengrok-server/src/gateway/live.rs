//! The gateway's live layer: ordering stamps and the roster/transcript emits.
//!
//! ORDERING IS A PROMISE, NOT DECORATION. Every roster and transcript frame carries
//! `ordered: {replicaKey, epoch, sequence}` (`client-grok-bot.md` §4.5): the client watches for
//! sequence gaps and epoch changes and treats either as a stale replica. The epoch is minted per
//! process, so a restart *announces itself* instead of quietly renumbering.

use serde_json::{Value, json};

use super::{GatewayState, summaries};

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// The next `ordered` stamp for a replica key.
pub fn ordered(state: &GatewayState, replica_key: &str) -> Value {
    let sequence = match state.seqs.lock() {
        Ok(mut seqs) => {
            let seq = seqs.entry(replica_key.to_string()).or_insert(0);
            *seq += 1;
            *seq
        }
        Err(_) => 0,
    };
    json!({ "replicaKey": replica_key, "epoch": state.epoch, "sequence": sequence })
}

/// Send one frame to every open `/events` stream. No subscriber is an ordinary morning, not an
/// error — emits happen whether or not anyone is watching.
pub fn emit(state: &GatewayState, channel: &str, payload: Value) {
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
    let payload = json!({
        "activeAgentId": active_agent_id(state),
        "agents": rows,
        "ordered": ordered(state, "roster"),
        "coverage": { "kind": "complete-roster" },
    });
    emit(state, "agents", payload);
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
    let payload = json!({
        "activeAgentId": active_agent_id(state),
        "agent": row,
        "ordered": ordered(state, "roster"),
    });
    emit(state, "agent-upserted", payload);
}

/// A transcript frame for one agent — `appended` or `updated`, stamped on that agent's replica.
pub fn emit_transcript(state: &GatewayState, agent_id: &str, kind: &str, entry: Value) {
    let payload = json!({
        "type": kind,
        "entry": entry,
        "agentId": agent_id,
        "ordered": ordered(state, &format!("transcript:{agent_id}")),
    });
    emit(state, "transcript", payload);
}

/// The live roster rows, run-state and all.
pub async fn roster_rows(state: &GatewayState) -> Result<Vec<Value>, opengrok_store::StoreError> {
    let Some(account) = state.agui.auth.store.account_by_email(&state.email).await? else {
        return Ok(Vec::new());
    };
    let coworkers = state.agui.auth.store.coworkers_for(&account.id).await?;
    let mut rows = Vec::new();
    for view in coworkers.iter().filter(|view| !view.retired) {
        rows.push(live_summary(state, view).await);
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
