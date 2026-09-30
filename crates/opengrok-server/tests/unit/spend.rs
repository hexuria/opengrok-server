use super::*;

#[test]
fn money_parses_exactly_and_refuses_what_is_not_money() {
    assert_eq!(micros("5"), Some(5_000_000));
    assert_eq!(micros("5.00"), Some(5_000_000));
    assert_eq!(micros("0.000001"), Some(1));
    assert_eq!(micros("12.345678"), Some(12_345_678));
    assert_eq!(micros(" 1.5 "), Some(1_500_000));
    assert_eq!(
        micros("1."),
        Some(1_000_000),
        "a trailing point is still money"
    );
    for bad in ["", "-1", "1.2345678", "lots", "1e3", ".5", "$5", "1,5"] {
        assert_eq!(micros(bad), None, "{bad}");
    }
}

fn counted(month: i64, day: i64, pool_used: Option<i64>) -> Counted {
    Counted {
        month,
        day,
        day_frees_at: Some("2026-09-03T14:32:10Z".into()),
        resets_at: Some("2026-10-01T00:00:00Z".into()),
        pool_used,
        pool_heaviest: None,
    }
}

fn counted_blaming(month: i64, pool_used: i64, who: &str, spent: i64) -> Counted {
    Counted {
        pool_heaviest: Some((who.to_string(), spent)),
        ..counted(month, 0, Some(pool_used))
    }
}

/// The plan's sentences: the cap with what the pool leaves others, the pool, the day's
/// brake with when it frees up; nothing when there is room everywhere.
#[test]
fn the_sentence_names_the_limit_in_the_way_with_the_numbers_and_when_it_frees_up() {
    let month = chrono::Utc::now().format("%B").to_string();
    let limits = crate::points::Effective {
        cap: Some(100_000),
        day_cap: Some(30_000),
        pool: Some(1_000_000),
        ..crate::points::Effective::none_set()
    };
    assert_eq!(
        over_points("New Bot", &limits, &counted(99_999, 100, Some(500_000))),
        None
    );
    let s = over_points("New Bot", &limits, &counted(102_340, 100, Some(588_000))).unwrap();
    assert_eq!(
        s,
        format!(
            "New Bot has used its 100,000 points for {month} (102,340 used); it resets on \
             1 October. 412,000 of your 1,000,000 remain for other agents."
        )
    );
    let s = over_points("New Bot", &limits, &counted(10, 5, Some(1_000_000))).unwrap();
    assert_eq!(
        s,
        format!(
            "Your pool of 1,000,000 points for {month} is used up (1,000,000 used); it \
             resets on 1 October."
        )
    );
    let s = over_points("New Bot", &limits, &counted(10, 30_000, Some(10))).unwrap();
    assert_eq!(
        s,
        "New Bot has used its 30,000 points for today (30,000 used); it frees up at 14:32 UTC."
    );
    // A cap alone says nothing about a pool.
    let cap_only = crate::points::Effective {
        cap: Some(1_000),
        ..crate::points::Effective::none_set()
    };
    let s = over_points("Ada", &cap_only, &counted(1_000, 0, None)).unwrap();
    assert_eq!(
        s,
        format!("Ada has used its 1,000 points for {month} (1,000 used); it resets on 1 October.")
    );
    assert_eq!(
        over_points(
            "Ada",
            &crate::points::Effective::none_set(),
            &counted(1, 1, None)
        ),
        None
    );
}

/// A pool refusal that names nobody leaves the reader to guess which of their coworkers ate
/// the month. The caller drops the name when the heaviest spender IS the one being refused,
/// so this only ever points somewhere the reader has not already looked.
#[test]
fn the_pool_refusal_says_who_spent_it() {
    let month = chrono::Utc::now().format("%B").to_string();
    let limits = crate::points::Effective {
        pool: Some(1_000_000),
        ..crate::points::Effective::none_set()
    };
    let s = over_points(
        "Bo",
        &limits,
        &counted_blaming(4_000, 1_000_000, "Ada", 812_500),
    )
    .unwrap();
    assert_eq!(
        s,
        format!(
            "Your pool of 1,000,000 points for {month} is used up (1,000,000 used); it \
             resets on 1 October. Ada accounts for 812,500 of it."
        )
    );
    // Nothing named: the caller already decided this reader is the spender.
    let s = over_points("Ada", &limits, &counted(4_000, 0, Some(1_000_000))).unwrap();
    assert!(
        s.ends_with("it resets on 1 October."),
        "no blame when there is nobody else to name: {s}"
    );
}

/// A PERSON'S OWN SUBSCRIPTION IS NOT THE GATEWAY'S TO METER. A proxy turn of a scoped, paid-for
/// coworker goes straight through the guard: no limit is read, no meter asked, no key repaired, so
/// nothing about it can be billed, capped or booked on the gateway. The same turn for the gateway
/// is counted first, and here the store answers nothing, so counting it holds it.
#[tokio::test]
async fn a_proxy_turn_is_never_metered_or_billed_on_the_gateway() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(200))
        .connect_lazy("postgres://nobody@127.0.0.1:1/nowhere")
        .unwrap();
    let inner: Arc<dyn ModelDoor> = Arc::new(opengrok_harness::MockDoor::echoing());
    let guard = GuardedDoor::new(inner, PgStore::new(pool), None);
    let turn = |endpoint| ModelRequest {
        model: "gpt-5.5".to_string(),
        spend_scope: Some("cw_paid".to_string()),
        spend_actor: Some("acct_payer".to_string()),
        gateway_key: None,
        endpoint,
        ..ModelRequest::default()
    };
    let proxy = opengrok_harness::ModelEndpoint::Proxy {
        base_url: "http://127.0.0.1:18080".to_string(),
        auth: None,
    };
    assert!(
        guard.stream(turn(Some(proxy))).await.is_ok(),
        "the proxy turn reached the door untouched"
    );
    let counted = guard
        .stream(turn(None))
        .await
        .err()
        .map(|error| error.to_string());
    assert!(
        counted
            .as_deref()
            .is_some_and(|why| why.contains("could not be read")),
        "a gateway turn is counted first, and held here: {counted:?}"
    );
}
