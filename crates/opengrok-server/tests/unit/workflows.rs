#[test]
fn an_ask_with_kind_noul_refuses_when_jev_is_not_compiled() {
    let why = super::ask_refused_without_jev();
    assert!(
        why.contains("cargo feature `jev`"),
        "the refusal must name the missing feature, got {why}"
    );
    assert!(
        matches!(super::jev_wanted_on_this_build(None), Err(sentence) if sentence == why),
        "an ask that needs Jev must refuse"
    );
    assert!(
        matches!(
            super::jev_wanted_on_this_build(Some(true)),
            Err(sentence) if sentence == why
        ),
        "jev true still needs the feature"
    );
    assert!(
        super::jev_wanted_on_this_build(Some(false)).is_ok(),
        "an explicit off stays a walk without a judge"
    );
}
