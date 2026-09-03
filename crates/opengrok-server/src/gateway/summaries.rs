//! A coworker, as a roster row the desktop client will render.
//!
//! The shape is `client-grok-bot.md` §8.1 — `minimalAgentSummary` plus `buildSummary`'s durable
//! extras — with the defaults an agent that has never spoken carries. Two fields matter more
//! than they look:
//!
//! - `updatedAt` is the roster's sort key; a row whose `updatedAt` never moves never reorders.
//! - The client drops a row named literally `"Grok"` with no title, description or transcript
//!   (§8.2, the blank-agent suppression rule). Our coworkers are hired with names, so they
//!   survive it — but the `description` fallback below is also what keeps a hypothetical
//!   name-collision visible.

use opengrok_core::coworker::CoworkerView;

/// One roster row, exactly as the renderer validates it.
pub fn summary(view: &CoworkerView) -> serde_json::Value {
    serde_json::json!({
        "id": view.id.as_str(),
        "name": view.name,
        // The model it thinks with is the most useful subtitle we can give a row that has not
        // spoken yet — and a non-empty description defeats blank-agent suppression (§8.2).
        "description": view.model,
        "title": "",
        // What this coworker is FOR, standing across every conversation (`server/persona.rs`).
        // Null rather than absent: the desktop distinguishes "no role" from "field not served".
        "role": view.role,
        // Slim mode nulls this anyway, and we serve no avatar bytes yet (P7).
        "avatarDataUrl": null,
        "avatarVersion": null,
        "avatarShape": null,
        "avatarColor": null,
        "createdAt": view.updated_at_ms,
        "updatedAt": view.updated_at_ms,
        "path": "",
        "isActive": false,
        "isRunning": false,
        "isRunningTurn": false,
        "isComposingMessage": false,
        "isRetrying": false,
        "lastEntry": null,
        "lastMessageId": null,
        "lastMessagePreview": null,
        "newestEntryId": null,
        "hasUnread": false,
        "unreadCount": 0,
        "lastViewedAt": view.updated_at_ms,
        "lastActivityAt": view.updated_at_ms,
        "awaitingUserResponse": null,
        "notificationsEnabled": false,
        "notifyOnUpdatesEnabled": true,
        "isHiddenFromSidebar": false,
        "origin": "user",
        "isGroup": !view.members.is_empty(),
        "memberIds": view.members.iter().map(|m| m.as_str()).collect::<Vec<_>>(),
        "conversationPartnerIds": []
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)]

    use super::*;
    use opengrok_core::id::CoworkerId;

    #[test]
    fn a_row_carries_the_fields_the_renderer_validates() {
        let row = summary(&CoworkerView {
            id: CoworkerId::from_stored("cw_1"),
            name: "Luna".to_string(),
            model: "gpt-5.6-luna".to_string(),
            box_id: None,
            retired: false,
            members: Vec::new(),
            updated_at_ms: 1_000,
            role: None,
            visibility: Default::default(),
        });
        assert_eq!(row["id"], "cw_1");
        assert_eq!(row["name"], "Luna");
        // The sort key, and the blank-agent defence.
        assert_eq!(row["updatedAt"], 1_000);
        assert_eq!(row["description"], "gpt-5.6-luna");
        // Container types the renderer throws on when wrong.
        assert!(row["memberIds"].is_array());
        assert!(row["unreadCount"].is_number());
        assert!(row["hasUnread"].is_boolean());
    }
}
