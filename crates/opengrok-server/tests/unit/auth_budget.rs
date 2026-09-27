#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;

const SMALL: Budget = Budget {
    name: "test",
    per_window: 2,
    window_ms: 10_000,
};

#[test]
fn a_budget_is_spent_after_its_hits_and_frees_when_the_oldest_leaves_the_window() {
    let budgets = Budgets::default();
    assert_eq!(budgets.take_at(&SMALL, "a", 1_000), Ok(()));
    assert_eq!(budgets.take_at(&SMALL, "a", 4_000), Ok(()));
    // Spent: the oldest hit (1s) leaves the 10s window at 11s; at 5s that is 6s away.
    assert_eq!(
        budgets.take_at(&SMALL, "a", 5_000),
        Err(Spent {
            retry_after_secs: 6
        })
    );
    // A refusal does not extend the wait.
    assert_eq!(
        budgets.take_at(&SMALL, "a", 5_500),
        Err(Spent {
            retry_after_secs: 6
        })
    );
    // Another key, and another budget with the same key, are separate buckets.
    assert_eq!(budgets.take_at(&SMALL, "b", 5_000), Ok(()));
    let other = Budget {
        name: "other",
        ..SMALL
    };
    assert_eq!(budgets.take_at(&other, "a", 5_000), Ok(()));
    // At 11s the first hit has left: one more is allowed, then spent again.
    assert_eq!(budgets.take_at(&SMALL, "a", 11_000), Ok(()));
    assert!(budgets.take_at(&SMALL, "a", 11_001).is_err());
}

#[test]
fn check_does_not_spend_and_hit_does_not_ask() {
    let budgets = Budgets::default();
    for _ in 0..10 {
        assert_eq!(budgets.check_at(&SMALL, "a", 1_000), Ok(()));
    }
    budgets.hit_at(&SMALL, "a", 1_000);
    budgets.hit_at(&SMALL, "a", 2_000);
    assert_eq!(
        budgets.check_at(&SMALL, "a", 3_000),
        Err(Spent {
            retry_after_secs: 8
        })
    );
    // Hits past the budget still land (a failure is a failure) without breaking the count.
    budgets.hit_at(&SMALL, "a", 3_000);
    assert!(budgets.check_at(&SMALL, "a", 11_000).is_err());
    assert_eq!(budgets.check_at(&SMALL, "a", 12_000), Ok(()));
}

#[test]
fn a_refund_frees_the_newest_hit_and_never_goes_below_nothing() {
    let budgets = Budgets::default();
    assert_eq!(budgets.take_at(&SMALL, "a", 1_000), Ok(()));
    assert_eq!(budgets.take_at(&SMALL, "a", 2_000), Ok(()));
    assert!(budgets.take_at(&SMALL, "a", 3_000).is_err());
    budgets.refund(&SMALL, "a");
    // The 2s hit went back, so the wait is still set by the 1s one.
    assert_eq!(budgets.take_at(&SMALL, "a", 3_000), Ok(()));
    assert_eq!(
        budgets.take_at(&SMALL, "a", 3_500),
        Err(Spent {
            retry_after_secs: 8
        })
    );
    budgets.refund(&SMALL, "a");
    budgets.refund(&SMALL, "a");
    budgets.refund(&SMALL, "a");
    budgets.refund(&SMALL, "nobody");
    assert_eq!(budgets.take_at(&SMALL, "a", 4_000), Ok(()));
}

#[test]
fn retry_after_is_never_zero() {
    let budgets = Budgets::default();
    budgets.hit_at(&SMALL, "a", 0);
    budgets.hit_at(&SMALL, "a", 0);
    assert_eq!(
        budgets.check_at(&SMALL, "a", 9_999),
        Err(Spent {
            retry_after_secs: 1
        })
    );
    assert_eq!(to_secs(0), 1);
    assert_eq!(to_secs(-5), 1);
    assert_eq!(to_secs(1_001), 2);
}

#[test]
fn the_peer_is_the_first_forwarded_address_or_unknown() {
    let mut headers = HeaderMap::new();
    assert_eq!(peer_of(&headers), "unknown");
    headers.insert("x-forwarded-for", " 10.0.0.9, 192.168.1.1".parse().unwrap());
    assert_eq!(peer_of(&headers), "10.0.0.9");
    assert_eq!(email_key("  Ada@Example.COM "), "email:ada@example.com");
}
