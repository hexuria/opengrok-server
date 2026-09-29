use super::*;

fn limits(rounds: Option<u32>, computer: Option<u32>, wall: Option<u64>) -> RunLimits {
    RunLimits {
        max_rounds: rounds.and_then(NonZeroU32::new),
        max_computer_rounds: computer.and_then(NonZeroU32::new),
        max_wall_ms: wall.and_then(NonZeroU64::new),
    }
}

#[test]
fn a_limit_reads_as_a_person_would_say_it() {
    assert_eq!(spoken(Duration::from_millis(100)), "100 ms");
    assert_eq!(spoken(Duration::from_secs(90)), "90 seconds");
    assert_eq!(spoken(Duration::from_secs(15 * 60)), "15 minutes");
}

/// Limits only ever narrow the server's budget: a limit above it is the server's, one left unset
/// is the server's, and a model call's own clocks never move.
#[test]
fn a_budget_held_to_limits_only_narrows_the_servers() {
    let server = RunBudget::default();
    assert_eq!(RunBudget::held_to(&RunLimits::default()), server);
    let held = RunBudget::held_to(&limits(Some(3), Some(500), Some(60_000)));
    assert_eq!(held.max_rounds, 3);
    assert_eq!(
        held.max_computer_rounds, server.max_computer_rounds,
        "above the server's budget is the server's budget"
    );
    assert_eq!(held.max_wall_ms, 60_000);
    assert_eq!(
        (held.call_timeout_ms, held.idle_ms),
        (server.call_timeout_ms, server.idle_ms)
    );
}

/// The server's budget read as limits is every limit set, at the server's own values, and a run
/// held to exactly those is held to the server's budget.
#[test]
fn the_servers_budget_reads_back_as_limits() {
    let server = RunBudget::default();
    assert_eq!(
        server.limits(),
        limits(
            Some(u32::try_from(crate::MAX_ROUNDS).unwrap()),
            Some(u32::try_from(crate::MAX_COMPUTER_ROUNDS).unwrap()),
            Some(server.max_wall_ms)
        )
    );
    assert_eq!(RunBudget::held_to(&server.limits()), server);
}
