//! The just-rotated-away refresh plaintext, held for [`opengrok_core::REFRESH_GRACE_MS`].
//!
//! The account log stores hashes only. A concurrent `POST /auth/refresh` that presents the old
//! cookie needs the *already-minted* current refresh so both responses Set-Cookie the same pair.
//! That plaintext lives here, keyed by the previous hash, then is dropped. Per replica: a loser
//! on another process still 401s (NativeChat single-flights on one host). Never a durable copy.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use opengrok_core::REFRESH_GRACE_MS;
use opengrok_core::id::{AccountId, SessionId};
use tokio::sync::Notify;

/// How long a race loser waits for this replica's winner to stash the current refresh.
///
/// `remember` runs in the same task after append commits, with no await between them — but the
/// loser's `append` Conflict (or empty `decide`) can be polled first. `yield_now` is not enough
/// on a busy runtime; we subscribe to [`Notify`] and fail closed after this cap. Never a second
/// rotate while waiting: that would invalidate the winner's cookies.
const REUSE_WAIT: Duration = Duration::from_millis(100);

#[derive(Clone)]
pub(crate) struct GraceSlot {
    pub current_refresh: String,
    pub account_id: AccountId,
    pub session_id: SessionId,
    pub email: String,
    pub plan: String,
}

pub(crate) struct RefreshGrace {
    slots: Mutex<HashMap<String, (GraceSlot, i64)>>,
    notify: Notify,
}

impl std::fmt::Debug for RefreshGrace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RefreshGrace(<redacted>)")
    }
}

impl Default for RefreshGrace {
    fn default() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
            notify: Notify::new(),
        }
    }
}

impl RefreshGrace {
    pub(crate) fn remember(&self, previous_hash: String, slot: GraceSlot, at_ms: i64) {
        {
            let Ok(mut slots) = self.slots.lock() else {
                return;
            };
            slots.retain(|_, (_, stored_at)| at_ms.saturating_sub(*stored_at) <= REFRESH_GRACE_MS);
            slots.insert(previous_hash, (slot, at_ms));
        }
        self.notify.notify_waiters();
    }

    pub(crate) fn reuse(&self, previous_hash: &str, at_ms: i64) -> Option<GraceSlot> {
        let Ok(mut slots) = self.slots.lock() else {
            return None;
        };
        slots.retain(|_, (_, stored_at)| at_ms.saturating_sub(*stored_at) <= REFRESH_GRACE_MS);
        slots.get(previous_hash).map(|(slot, _)| slot.clone())
    }

    /// Wait until [`Self::remember`] stashes this previous hash, or [`REUSE_WAIT`] elapses.
    pub(crate) async fn wait_for_reuse(
        &self,
        previous_hash: &str,
        at_ms: i64,
    ) -> Option<GraceSlot> {
        let deadline = tokio::time::Instant::now() + REUSE_WAIT;
        loop {
            if let Some(slot) = self.reuse(previous_hash, at_ms) {
                return Some(slot);
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return self.reuse(previous_hash, at_ms);
            }
            // Subscribe before the second lookup so a remember between the two is not lost.
            let notified = self.notify.notified();
            if let Some(slot) = self.reuse(previous_hash, at_ms) {
                return Some(slot);
            }
            tokio::select! {
                () = notified => {}
                () = tokio::time::sleep(remaining) => {
                    return self.reuse(previous_hash, at_ms);
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn slot(refresh: &str) -> GraceSlot {
        GraceSlot {
            current_refresh: refresh.to_string(),
            account_id: AccountId::from_stored("acct_1"),
            session_id: SessionId::from_stored("sess_1"),
            email: "a@b.c".to_string(),
            plan: "pro".to_string(),
        }
    }

    #[test]
    fn reuse_returns_the_current_refresh_inside_grace() {
        let grace = RefreshGrace::default();
        grace.remember("hash-1".into(), slot("ogr_current"), 1_000);
        let reused = grace.reuse("hash-1", 1_000 + REFRESH_GRACE_MS).unwrap();
        assert_eq!(reused.current_refresh, "ogr_current");
    }

    #[test]
    fn reuse_is_gone_after_grace() {
        let grace = RefreshGrace::default();
        grace.remember("hash-1".into(), slot("ogr_current"), 1_000);
        assert!(
            grace
                .reuse("hash-1", 1_000 + REFRESH_GRACE_MS + 1)
                .is_none()
        );
    }

    #[tokio::test]
    async fn wait_for_reuse_wakes_when_the_winner_remembers() {
        let grace = Arc::new(RefreshGrace::default());
        let waiter = Arc::clone(&grace);
        let wait = tokio::spawn(async move { waiter.wait_for_reuse("hash-1", 1_000).await });
        // The waiter must subscribe before remember; a single yield is not a guarantee, but the
        // 100ms cap plus Notify covers both orderings. Give the task a tick to start waiting.
        tokio::task::yield_now().await;
        grace.remember("hash-1".into(), slot("ogr_current"), 1_000);
        let reused = wait
            .await
            .expect("join")
            .expect("winner stashed the current refresh");
        assert_eq!(reused.current_refresh, "ogr_current");
    }
}
