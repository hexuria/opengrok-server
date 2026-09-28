#![allow(clippy::unwrap_used)]
use super::*;

#[test]
fn an_alias_prices_at_its_base_model() {
    assert_eq!(base_model("xai/grok-4.6@sub"), "xai/grok-4.6");
    assert_eq!(base_model("openai/gpt-5.5@api"), "openai/gpt-5.5");
    assert_eq!(base_model("xai/grok-4.6"), "xai/grok-4.6");
    assert_eq!(base_model("oag/cheap"), "oag/cheap");
}

#[test]
fn points_are_written_with_commas() {
    assert_eq!(commas(0), "0");
    assert_eq!(commas(999), "999");
    assert_eq!(commas(1_000), "1,000");
    assert_eq!(commas(1_234_567), "1,234,567");
    assert_eq!(commas(50_000_000), "50,000,000");
}

#[test]
fn the_effective_cap_is_the_cap_bounded_by_what_the_pool_leaves_others() {
    let both = Effective {
        cap: Some(100_000),
        pool: Some(1_000_000),
        ..Effective::none_set()
    };
    // Others used 950,000 of the million: the pool leaves 50,000, under the cap.
    assert_eq!(
        effective_cap(&both, Some(10_000), Some(960_000)),
        Some(50_000)
    );
    // Others used little: the cap binds.
    assert_eq!(
        effective_cap(&both, Some(10_000), Some(20_000)),
        Some(100_000)
    );
    let cap_only = Effective {
        cap: Some(100_000),
        ..Effective::none_set()
    };
    assert_eq!(effective_cap(&cap_only, None, None), Some(100_000));
    let pool_only = Effective {
        pool: Some(1_000_000),
        ..Effective::none_set()
    };
    assert_eq!(
        effective_cap(&pool_only, Some(5), Some(400_005)),
        Some(600_000)
    );
    assert_eq!(effective_cap(&Effective::none_set(), None, None), None);
}

#[test]
fn a_limit_is_a_whole_non_negative_number_of_points() {
    assert!(validate_points("cap", None).is_ok());
    assert!(validate_points("cap", Some(0)).is_ok());
    assert!(validate_points("cap", Some(-1)).is_err());
    assert!(validate_points("cap", Some(MAX_POINTS + 1)).is_err());
    let body = json!({ "cap": 100, "dayCap": null });
    assert_eq!(field(&body, "cap").unwrap(), Some(Some(100)));
    assert_eq!(field(&body, "dayCap").unwrap(), Some(None));
    assert_eq!(field(&body, "other").unwrap(), None, "absent is leave it");
    assert!(field(&json!({ "cap": "ten" }), "cap").is_err());
}
