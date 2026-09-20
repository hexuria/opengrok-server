//! The one piece of the agent lifecycle that outlived seam A: the schedule write.
//!
//! This module was the client's vocabulary for everything — createAgent, updateAgent, avatars,
//! unread, the automations verbs, rooms. All of it went with the door. `mutate_schedule` stays
//! because the webhook door (`hooks.rs`) fires routines through it, and the compare-and-set
//! retry it does is the reason a routine edit and a "run now" a few milliseconds apart do not
//! answer 500.

use serde_json::{Value, json};

use opengrok_core::id::AccountId;
use opengrok_core::id::ScheduleId;
use opengrok_core::schedule::Schedule;

use crate::host_state::HostState;

/// Load the schedule, decide with `decide`, append at the loaded seq — and if another writer got
/// there first, re-read and try ONCE more before answering 409. Why: the desktop's Routines pane
/// autosaves an edit on blur at the same instant a person clicks "Test run", so two mutations on
/// one schedule a few milliseconds apart are the ordinary case, not a race to design away. The
/// loser used to answer 500 "storage failed" (seen live 2 Sep 2026); now it decides again against
/// the winner's state, which is what the person meant anyway.
///
/// `decide` sees the fresh aggregate and returns the events to append, or a refusal already
/// shaped for the wire. Returns the aggregate after the append and the seq it landed at.
pub(crate) async fn mutate_schedule<F>(
    state: &HostState,
    account_id: &AccountId,
    schedule_id: &ScheduleId,
    at_ms: i64,
    mut decide: F,
) -> Result<Schedule, (u16, Value)>
where
    F: FnMut(&Schedule) -> Result<Vec<opengrok_core::schedule::ScheduleEvent>, (u16, Value)>,
{
    for attempt in 0..2 {
        let Ok((loaded, seq)) = state.agui.auth.store.load_schedule(schedule_id).await else {
            return Err((404, json!({ "error": "no such routine" })));
        };
        let events = decide(&loaded)?;
        let mut after = loaded;
        for event in &events {
            after.apply(event);
        }
        match state
            .agui
            .auth
            .store
            .append_schedule(schedule_id, account_id, seq, &events, &after, at_ms)
            .await
        {
            Ok(_) => return Ok(after),
            Err(opengrok_store::StoreError::Conflict) if attempt == 0 => {
                tracing::info!(schedule = %schedule_id, "a routine write lost a race; re-reading and retrying once");
                continue;
            }
            Err(opengrok_store::StoreError::Conflict) => {
                return Err((
                    409,
                    json!({ "error": "another change to this routine landed first; reload and retry" }),
                ));
            }
            Err(error) => {
                tracing::error!(%error, schedule = %schedule_id, "could not write a routine change");
                return Err((500, json!({ "error": "storage failed" })));
            }
        }
    }
    Err((
        409,
        json!({ "error": "another change to this routine landed first; reload and retry" }),
    ))
}
