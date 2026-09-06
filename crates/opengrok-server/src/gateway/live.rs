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

/// The COUNTER key for a stamped frame delivered to one account — never the wire value.
///
/// The client knows exactly two replica keys, `"roster"` and `` `transcript:${agentId}` ``
/// (`client-grok-bot.md` §4.5, from `source/shared/ordering.ts:1-5`), and invents nothing from
/// them: an unrecognised key is a shape we do not compile (CLAUDE.md #1). So the per-person split
/// that scoping demands lives HERE, in the server's own map, and the `replicaKey` on the wire is
/// unchanged. Each account then sees a contiguous run under the key it already understands, and
/// two accounts' runs never meet because neither ever receives the other's frames.
///
/// This is what "per-person rosters mean per-person sequences" turned out to cost: a map key,
/// not a change to the replica contract. The `\u{1}` separator cannot occur in either half.
fn counter_key(replica_key: &str, audience: &opengrok_core::id::AccountId) -> String {
    format!("{replica_key}\u{1}{}", audience.as_str())
}

/// The stamp a NEW subscriber is seeded with: the sequence that subscriber's account has already
/// seen, NOT a fresh one. A per-subscriber frame that minted would be a gap for everyone else.
pub fn current_for(
    state: &GatewayState,
    replica_key: &str,
    audience: &opengrok_core::id::AccountId,
) -> Value {
    let sequence = state
        .seqs
        .lock()
        .ok()
        .and_then(|seqs| seqs.get(&counter_key(replica_key, audience)).copied())
        .unwrap_or(0);
    stamp(state, replica_key, sequence)
}

/// Mint the next sequence for `replica_key`, build the frame around it, and send it to every open
/// `/events` stream — all under the sequence lock, so two emits on one key reach the broadcast in
/// the order they were numbered. `send` is synchronous and never blocks, which is what makes
/// holding a std mutex across it acceptable. No subscriber is an ordinary morning, not an error.
pub fn emit_ordered_to(
    state: &GatewayState,
    channel: &str,
    replica_key: &str,
    audience: &opengrok_core::id::AccountId,
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
    let seq = seqs.entry(counter_key(replica_key, audience)).or_insert(0);
    *seq += 1;
    let payload = build(stamp(state, replica_key, *seq));
    // ADDRESSED. This used to send with no audience, which put one account's roster — and every
    // transcript frame, message content included — on every open stream. The comment here said
    // filtering per person would spend sequence numbers other streams expect, and that making the
    // roster per person meant making the SEQUENCES per person: a change to the replica contract
    // and its own piece of work (#58).
    //
    // It was half right. Per-person sequences were indeed required — and they cost a map key
    // (`counter_key`), not a contract change, because the split belongs in the server's counter
    // and not in the `replicaKey` the client reads. Each account keeps a contiguous run under the
    // key it already knows; the gap the old comment feared cannot occur, because an account never
    // receives the frames that would have skipped its numbers.
    send(state, channel, payload, Some(audience.clone()));
}

/// Every account that can currently SEE this coworker: its owner, plus the owner's org-mates when
/// it is shared org-wide.
///
/// FOR ROSTER FRAMES ONLY. A roster row is a fact about the coworker, so everyone who can see it
/// wants the update. A TRANSCRIPT is not — it is stored per `(coworker, account)`, so a shared
/// coworker has one per person and this audience would deliver somebody else's conversation. That
/// is why `emit_transcript` takes an account instead of calling this.
///
/// The roster widened to include org-mates' shared coworkers (`roster_for`), so a change to a
/// shared coworker has to reach more than its owner or a colleague's sidebar goes quietly stale.
/// Ownership alone would be a safe under-delivery and a wrong one.
///
/// A read failure yields an EMPTY audience — no frame rather than a broadcast. Silence is a
/// refresh away; the alternative is the disclosure this function exists to end.
async fn audience_for(
    state: &GatewayState,
    coworker: &opengrok_core::id::CoworkerId,
) -> Vec<opengrok_core::id::AccountId> {
    let store = &state.agui.auth.store;
    let Ok(Some(owner)) = store.coworker_owner(coworker).await else {
        return Vec::new();
    };
    let shared = store
        .roster_for(&owner)
        .await
        .ok()
        .and_then(|rows| {
            rows.into_iter()
                .find(|(view, _)| view.id.as_str() == coworker.as_str())
                .map(|(view, _)| view.visibility.as_str() != "private")
        })
        .unwrap_or(false);
    if !shared {
        return vec![owner];
    }
    let org = match store.load_account(&owner).await {
        Ok((account, _)) => account.org_id,
        Err(_) => None,
    };
    let Some(org) = org else { return vec![owner] };
    match store.accounts_by_org(&org).await {
        Ok(members) => {
            let mut all: Vec<_> = members.into_iter().map(|member| member.id).collect();
            if !all.contains(&owner) {
                all.push(owner);
            }
            all
        }
        Err(_) => vec![owner],
    }
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
pub async fn emit_roster_to(state: &GatewayState, audience: &[opengrok_core::id::AccountId]) {
    let active = active_agent_id(state);
    for account in audience {
        // Each account's OWN roster, read per account. The rows are not the same rows: `mine`,
        // `canManage` and `computerError` are decided per viewer, so one build shared between two
        // people would be wrong for at least one of them even with delivery scoped correctly.
        let Ok((account_row, _)) = state.agui.auth.store.load_account(account).await else {
            continue;
        };
        let Ok(rows) = roster_rows_for(state, &account_row.email).await else {
            continue;
        };
        let active = active.clone();
        emit_ordered_to(state, "agents", "roster", account, move |ordered| {
            json!({
                "activeAgentId": active,
                "agents": rows,
                "ordered": ordered,
                "coverage": { "kind": "complete-roster" },
            })
        });
    }
}

/// The roster as ONE person's, for the caller who changed something and anyone sharing with them.
pub async fn emit_roster_for_caller(state: &GatewayState, caller: &str) {
    let Ok(Some(account)) = state.agui.auth.store.account_by_email(caller).await else {
        return;
    };
    // The caller always; their org-mates too, because a hire can enter a shared roster and a
    // colleague whose sidebar never hears about it is the staleness half of the same bug.
    let mut audience = vec![account.id.clone()];
    if let Ok((row, _)) = state.agui.auth.store.load_account(&account.id).await
        && let Some(org) = row.org_id
        && let Ok(members) = state.agui.auth.store.accounts_by_org(&org).await
    {
        for member in members {
            if !audience.contains(&member.id) {
                audience.push(member.id);
            }
        }
    }
    emit_roster_to(state, &audience).await;
}

/// A single-row delta — `agent-upserted`. `patch` overlays fields the caller knows better than
/// the projection does (a fresh `lastMessagePreview`, a moved `updatedAt`).
pub async fn emit_agent_upserted(state: &GatewayState, coworker_id: &str, patch: Value) {
    let active = active_agent_id(state);
    let coworker = opengrok_core::id::CoworkerId::from_stored(coworker_id.to_string());
    for account in audience_for(state, &coworker).await {
        // Read the row as THIS viewer, not once from the deployment account: `mine`, `canManage`
        // and `computerError` are per-viewer, so a shared build would tell a colleague a
        // coworker was theirs to manage.
        let Ok((account_row, _)) = state.agui.auth.store.load_account(&account).await else {
            continue;
        };
        let Ok(rows) = roster_rows_for(state, &account_row.email).await else {
            continue;
        };
        let Some(mut row) = rows.into_iter().find(|row| row["id"] == coworker_id) else {
            // Not in this viewer's roster — it was shared and is not any more, or never was.
            // Sending nothing is right; sending the row would be the disclosure again.
            continue;
        };
        if let (Some(target), Some(overlay)) = (row.as_object_mut(), patch.as_object()) {
            for (key, value) in overlay {
                target.insert(key.clone(), value.clone());
            }
        }
        let active = active.clone();
        // WHO THIS ROW WENT TO, AND WHAT IT SAID. The working dot is drawn from `isRunning` on
        // this frame, so "the dot never appeared" has three possible causes — the row was wrong,
        // the audience was wrong, or the client did not apply it — and from a log that named none
        // of them all three look identical. This line separates the first two from the third.
        tracing::debug!(
            coworker = %coworker_id,
            account = %account.as_str(),
            is_running = %row["isRunning"],
            "roster: agent-upserted"
        );
        emit_ordered_to(
            state,
            "agent-upserted",
            "roster",
            &account,
            move |ordered| {
                json!({
                    "activeAgentId": active,
                    "agent": row,
                    "ordered": ordered,
                })
            },
        );
    }
}

/// A frame that carries NO ordering stamp, on a channel no replica tracks (`agents-automation`:
/// the coordinator maps it to the renderer's `automations` family, whose own emitter posts
/// `{agentId, automations}` with no stamp — `routed-automations.ts:180`). Minting a sequence for
/// such a frame on a replica key would be a gap for that replica on every send, the class #14
/// removed; this is the deliberate way to say "not ordered".
pub fn emit_unstamped(state: &GatewayState, channel: &str, payload: Value) {
    send(state, channel, payload, None);
}

/// The same, for a frame built from ONE person's data. Delivered to that account's streams and
/// no others.
///
/// This exists because `emit_unstamped` was carrying a person's routines to every open stream:
/// the bus is a broadcast and the stream filtered by channel NAME only, so the payload was
/// correct and the delivery was not. The only thing preventing a disclosure was the client
/// declining to render an agent it did not recognise — obscurity, not a check, and it stopped
/// being even that when a coworker could be shared.
pub fn emit_unstamped_to(
    state: &GatewayState,
    channel: &str,
    payload: Value,
    audience: &opengrok_core::id::AccountId,
) {
    send(state, channel, payload, Some(audience.clone()));
}

fn send(
    state: &GatewayState,
    channel: &str,
    payload: Value,
    audience: Option<opengrok_core::id::AccountId>,
) {
    let _ = state.events_tx.send(super::LiveFrame {
        channel: channel.to_string(),
        payload,
        audience,
    });
}

/// A transcript frame for one agent — `appended` or `updated`, stamped on that agent's replica.
/// ADDRESSED TO THE ENTRY'S OWN ACCOUNT, and this one carries CONTENT.
///
/// Before #58 every transcript frame — the entry itself, message text included — went to every
/// open `/events` stream, filtered only by channel name. The roster leak next door disclosed
/// names; this one disclosed what people said.
///
/// The first fix addressed `audience_for`, which is everyone who can SEE the coworker: the owner
/// plus org-mates when it is shared. That was still wrong, because a transcript is not stored per
/// coworker — `append_gateway_entry` keys on `(coworker, account)`, so a shared coworker has one
/// transcript PER PERSON talking to it. Addressing the viewers of the coworker pushed Bob's
/// message and its answer into Alice's and Carol's open transcript for the same coworker: content
/// they cannot read back, and frames for entries that vanish on their next reload.
///
/// So the account is passed in rather than derived. Every caller has it already — it is the same
/// account it just appended or updated with — and taking it as an argument makes the frame and
/// the row that backs it impossible to disagree.
pub fn emit_transcript(
    state: &GatewayState,
    agent_id: &str,
    account: &opengrok_core::id::AccountId,
    kind: &str,
    entry: Value,
) {
    emit_ordered_to(
        state,
        "transcript",
        &format!("transcript:{agent_id}"),
        account,
        move |ordered| {
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
/// The same, for a deletion. Same reasoning: the row was deleted from ONE person's transcript.
pub fn emit_transcript_removed(
    state: &GatewayState,
    agent_id: &str,
    account: &opengrok_core::id::AccountId,
    entry_id: &str,
) {
    emit_ordered_to(
        state,
        "transcript",
        &format!("transcript:{agent_id}"),
        account,
        move |ordered| {
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
    // Owned rows AND the ones this person's org-mates have shared. A SEPARATE read from
    // `coworkers_for`, which stays owner-only because it is the authorisation primitive
    // `owned_coworker` gates management on — repin, retire, limits, computer. Sharing a coworker
    // lets somebody talk to it; it is not a write grant, and widening one query would have made
    // it one.
    let coworkers = state.agui.auth.store.roster_for(&account.id).await?;

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
    for (view, owner) in coworkers.iter().filter(|(view, _)| !view.retired) {
        let mine = owner.id == account.id;
        let mut row = live_summary(state, view).await;
        // A group has members instead of a computer: the account's provisioning error is not
        // its problem, and a "no computer" note on a group would send a person chasing one.
        // Somebody else's coworker carries null too — their provisioning trouble is theirs, and
        // naming it on a shared row would leak the state of an account this reader is not in.
        row["computerError"] = if mine && view.box_id.is_none() && view.members.is_empty() {
            crate::agui::provision::error_json_at(&account_error)
        } else {
            Value::Null
        };
        // The permission fields, decided by the server on every row. `mine` is ownership;
        // `canManage` follows it exactly, because management stayed with the owner when the
        // roster widened; `owner` names the hirer so a shared row can say whose it is.
        row["visibility"] = json!(view.visibility.as_str());
        row["mine"] = json!(mine);
        row["canManage"] = json!(mine);
        row["owner"] = json!({
            "id": owner.id.as_str(),
            "name": format!("{} {}", owner.first_name, owner.last_name).trim(),
        });
        // UNREAD, PER READER. These four were hard-coded `false`/`0`/`updatedAt` since the roster
        // existed, which the renderer reads as "seen everything, always" — so its "New" separator,
        // anchored at `lastViewedAt` whenever `lastActivityAt` is greater, could never appear and
        // no row could ever carry a badge.
        //
        // Read per pair, inside the per-viewer loop, because that is what unread IS: two people
        // sharing one coworker are in different places in it, and a single answer would be wrong
        // for at least one of them.
        //
        // A read failure leaves the row's defaults rather than failing the roster: a badge that is
        // missing is a smaller wrong than a sidebar that will not paint.
        if let Ok(unread) = state
            .agui
            .auth
            .store
            .unread_state(&view.id, &account.id)
            .await
        {
            // NEVER VIEWED STAYS NEVER. The renderer treats a non-positive `lastViewedAt` as
            // never, so `0` says exactly that; inventing `now` here would silence a first unread
            // and inventing the row's `updatedAt` is what the old hard-coded value did.
            row["lastViewedAt"] = json!(unread.last_viewed_ms.unwrap_or(0));
            if let Some(activity) = unread.last_activity_ms {
                row["lastActivityAt"] = json!(activity);
            }
            row["unreadCount"] = json!(unread.unread);
            row["hasUnread"] = json!(unread.unread > 0);
        }
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
