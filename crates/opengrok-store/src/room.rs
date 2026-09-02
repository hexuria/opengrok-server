//! A room paused on a member's card. A member's run inside a group suspends the way a coworker's
//! own run does (the journal holds the run, awaiting a person); what the room adds is WHERE the
//! round stood, so the answer resumes that member and then the members still to speak — not the
//! prompt from the top, and not just the one member.

use opengrok_core::id::{CoworkerId, RunId};
use serde_json::Value;
use sqlx::Row;

use crate::StoreResult;
use crate::postgres::PgStore;

/// The pause a resume takes back: the room, the member whose run it is, and the round cursor
/// the room module wrote (opaque here).
#[derive(Debug, Clone)]
pub struct RoomPause {
    pub group_id: String,
    pub member_id: String,
    pub cursor: Value,
}

impl PgStore {
    /// One pause per room; a newer one replaces an older.
    pub async fn save_room_pause(
        &self,
        group: &CoworkerId,
        run: &RunId,
        member: &CoworkerId,
        cursor: &Value,
        at_ms: i64,
    ) -> StoreResult<()> {
        sqlx::query(
            "insert into room_pause (group_id, run_id, member_id, cursor, at_ms)
             values ($1, $2, $3, $4, $5)
             on conflict (group_id) do update
                set run_id = excluded.run_id, member_id = excluded.member_id,
                    cursor = excluded.cursor, at_ms = excluded.at_ms",
        )
        .bind(group.as_str())
        .bind(run.as_str())
        .bind(member.as_str())
        .bind(cursor)
        .bind(at_ms)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// The pause for this run, taken: `delete … returning`, so two answers to one card (two
    /// devices, a double press) cannot both continue the round.
    pub async fn take_room_pause_for_run(&self, run: &RunId) -> StoreResult<Option<RoomPause>> {
        let row = sqlx::query(
            "delete from room_pause where run_id = $1
             returning group_id, member_id, cursor",
        )
        .bind(run.as_str())
        .fetch_optional(self.pool())
        .await?;
        row.map(|row| {
            Ok(RoomPause {
                group_id: row.try_get("group_id")?,
                member_id: row.try_get("member_id")?,
                cursor: row.try_get("cursor")?,
            })
        })
        .transpose()
    }

    /// A new prompt to the room abandons the pause it was holding.
    pub async fn clear_room_pause(&self, group: &CoworkerId) -> StoreResult<()> {
        sqlx::query("delete from room_pause where group_id = $1")
            .bind(group.as_str())
            .execute(self.pool())
            .await?;
        Ok(())
    }
}
