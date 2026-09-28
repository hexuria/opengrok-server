#![allow(clippy::unwrap_used, clippy::expect_used)]
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
