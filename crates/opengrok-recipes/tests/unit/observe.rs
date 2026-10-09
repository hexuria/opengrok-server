//! `observe.rs`: what the box saw while a recipe played, read back and put into words.

use super::*;
use serde_json::json;

/// A receipt shaped the way `box-cua` writes one, with the fields this module reads.
fn receipt(observe: Option<&str>, steps: Value) -> Value {
    let mut receipt = json!({ "ok": true, "ran": 3, "stopped_at": null, "steps": steps });
    if let (Some(observe), Some(object)) = (observe, receipt.as_object_mut()) {
        object.insert("observe".to_string(), json!(observe));
    }
    receipt
}

/// The box's default is off and its receipt is then byte-identical to the one it wrote before
/// the field existed. Sending `"off"` instead of sending nothing would be a body no shipped
/// box was tested against, for no gain.
#[test]
fn asking_for_nothing_leaves_the_body_alone() {
    let mut request = json!({ "name": "Search", "steps": [] });
    let before = request.clone();
    ask(&mut request, Observe::Off);
    assert_eq!(request, before);

    ask(&mut request, Observe::Input);
    assert_eq!(request.get("observe"), Some(&json!("input")));
    ask(&mut request, Observe::Page);
    assert_eq!(request.get("observe"), Some(&json!("page")));
}

/// The three words are the box's, and a fourth is not silently accepted as one of them.
#[test]
fn only_the_boxs_own_words_parse() {
    assert_eq!(Observe::read("off"), Some(Observe::Off));
    assert_eq!(Observe::read(" input "), Some(Observe::Input));
    assert_eq!(Observe::read("page"), Some(Observe::Page));
    assert_eq!(Observe::read("everything"), None);
    assert_eq!(Observe::read("Input"), None);
}

/// A box that never answered the question, or was never asked, must leave the words a model
/// reads exactly as they were. Silence about observation is not the same as an observation.
#[test]
fn a_receipt_that_never_looked_reads_as_nothing_to_say() {
    assert!(
        Seen::read(&receipt(
            None,
            json!([{ "index": 0, "op": "click", "ok": true }])
        ))
        .is_none()
    );
    assert!(
        Seen::read(&receipt(
            Some("off"),
            json!([{ "index": 0, "op": "click", "ok": true }])
        ))
        .is_none()
    );
}

/// Asked and told nothing is its own answer, and it is not the same as never asking: the box
/// echoes the level for exactly this reason, so the sentence says so rather than vanishing.
#[test]
fn asked_and_nothing_to_look_at_still_says_so() {
    let seen = Seen::read(&receipt(
        Some("input"),
        json!([{ "index": 0, "op": "wait", "ok": true }]),
    ))
    .unwrap();
    assert_eq!(seen.looked_at, 0);
    let said = seen.sentence();
    assert!(
        said.contains("no step had anything for it to look at"),
        "{said}"
    );
}

/// The reading a model needs: which window each click landed on, named, with the steps that
/// landed there — so "the third click went somewhere else" is visible without being asserted.
#[test]
fn the_windows_under_the_pointer_are_named_with_their_steps() {
    let seen = Seen::read(&receipt(
        Some("input"),
        json!([
            { "index": 0, "op": "click", "ok": true, "observed": {
                "target": { "id": "0x3a00007", "class": "chromium.Chromium", "title": "Inbox — Gmail" },
                "observe_ms": 11 } },
            { "index": 1, "op": "click", "ok": true, "observed": {
                "target": { "id": "0x3a00007", "class": "chromium.Chromium", "title": "Inbox — Gmail" },
                "observe_ms": 9 } },
            { "index": 2, "op": "click", "ok": true, "observed": {
                "target": { "id": "0x1400003", "class": "xterm.XTerm", "title": "Terminal" },
                "observe_ms": 10 } },
        ]),
    ))
    .unwrap();
    let said = seen.sentence();
    assert!(said.contains("3 of 3 steps observed"), "{said}");
    assert!(
        said.contains(
            "pointer steps landed on chromium.Chromium \"Inbox — Gmail\" (at steps 0, 1)"
        ),
        "{said}"
    );
    assert!(
        said.contains("pointer steps landed on xterm.XTerm \"Terminal\" (at step 2)"),
        "{said}"
    );
    assert!(said.contains("the looking cost 30ms"), "{said}");
    assert_eq!(
        seen.target_fact(),
        "chromium.Chromium \"Inbox — Gmail\"\nxterm.XTerm \"Terminal\""
    );
}

/// A `type` that reached nothing at all is the box's clearest reading, and the one a tape
/// whose window went away produces. It is reported as what the box read, not as a failure.
#[test]
fn keys_that_reached_nothing_are_said_in_the_boxs_own_terms() {
    let seen = Seen::read(&receipt(
        Some("input"),
        json!([
            { "index": 0, "op": "type", "ok": true, "observed": {
                "focus": { "state": "none" }, "observe_ms": 4 } },
            { "index": 1, "op": "key", "ok": true, "observed": {
                "focus": { "state": "window", "window": { "id": "0x1", "class": "chromium.Chromium" } },
                "observe_ms": 4 } },
        ]),
    ))
    .unwrap();
    let said = seen.sentence();
    assert!(
        said.contains("keys had nowhere to go: no window held the focus (at step 0)"),
        "{said}"
    );
    assert!(
        said.contains("keys went to chromium.Chromium (at step 1)"),
        "{said}"
    );
    assert_eq!(seen.focus_fact(), "none\nwindow");
}

/// A page that did not move is the reading the twenty-five-run incident would have produced.
/// It is a fact about a URL. The sentence must not contain a word about whether that is bad —
/// the box does not know what the recipe was for, and neither does this server.
#[test]
fn a_page_that_never_moved_is_a_reading_and_not_a_verdict() {
    let seen = Seen::read(&receipt(
        Some("page"),
        json!([
            { "index": 0, "op": "type", "ok": true, "observed": {
                "focus": { "state": "window", "window": { "id": "0x1", "class": "chromium.Chromium" } },
                "url_before": "https://www.youtube.com/", "url_after": "https://www.youtube.com/",
                "observe_ms": 40 } },
            { "index": 1, "op": "key", "ok": true, "observed": {
                "focus": { "state": "window", "window": { "id": "0x1", "class": "chromium.Chromium" } },
                "url_before": "https://www.youtube.com/", "url_after": "https://www.youtube.com/",
                "observe_ms": 38 } },
        ]),
    ))
    .unwrap();
    let said = seen.sentence();
    assert!(
        said.contains("the page stayed on https://www.youtube.com/ throughout"),
        "{said}"
    );
    assert_eq!(seen.page_fact(), "https://www.youtube.com/");
    for verdict in ["failed", "did not work", "wrong", "should", "error"] {
        assert!(
            !said.contains(verdict),
            "the server does not get to judge the run: {said}"
        );
    }
}

/// A page that came back to where it started is three readings, not two: collapsing repeats
/// only where they are adjacent is what keeps the going-back visible.
#[test]
fn a_page_that_came_back_still_shows_the_journey() {
    let seen = Seen::read(&receipt(
        Some("page"),
        json!([
            { "index": 0, "op": "key", "ok": true, "observed": {
                "url_before": "https://a.example/", "url_after": "https://b.example/", "observe_ms": 30 } },
            { "index": 1, "op": "key", "ok": true, "observed": {
                "url_before": "https://b.example/", "url_after": "https://a.example/", "observe_ms": 30 } },
        ]),
    ))
    .unwrap();
    assert_eq!(
        seen.page_fact(),
        "https://a.example/\nhttps://b.example/\nhttps://a.example/"
    );
    assert!(
        seen.sentence()
            .contains("the page went https://a.example/ → https://b.example/ → https://a.example/"),
        "{}",
        seen.sentence()
    );
}

/// A pointer step the box looked at and could name nothing under is reported, and is kept
/// apart in the facts from a focus state the box calls `none`.
#[test]
fn nothing_under_the_pointer_is_reported_as_nothing_named() {
    let seen = Seen::read(&receipt(
        Some("input"),
        json!([{ "index": 4, "op": "click", "ok": true, "observed": { "observe_ms": 12 } }]),
    ))
    .unwrap();
    assert!(
        seen.sentence()
            .contains("no window the box could name was under the pointer (at step 4)"),
        "{}",
        seen.sentence()
    );
    assert_eq!(seen.target_fact(), NOTHING_NAMED);
    assert_ne!(NOTHING_NAMED, NOT_READ);
}

/// The history keeps that a run was observed and what the looking cost, and keeps none of what
/// was on the screen: a recipe's runs are read by everyone the recipe was shared to.
#[test]
fn what_was_seen_is_not_written_down_but_what_it_cost_is() {
    let mut kept = receipt(
        Some("input"),
        json!([
            { "index": 0, "op": "click", "ok": true, "observed": {
                "target": { "id": "0x1", "class": "chromium.Chromium", "title": "quarterly-plan — Docs" },
                "observe_ms": 11 } },
            { "index": 1, "op": "wait", "ok": true },
        ]),
    );
    strip_observations(&mut kept);
    let written = kept.to_string();
    assert!(!written.contains("quarterly-plan"), "{written}");
    assert!(!written.contains("observed"), "{written}");
    assert_eq!(kept.get("observe"), Some(&json!("input")));
    assert_eq!(kept.get("observe_ms"), Some(&json!(11)));
    // A receipt that was never observed gains nothing at all, so an old run and a new
    // unobserved one are the same document.
    let mut untouched = receipt(None, json!([{ "index": 0, "op": "click", "ok": true }]));
    let before = untouched.clone();
    strip_observations(&mut untouched);
    assert_eq!(untouched, before);
}

/// A recipe may play 256 steps. Neither a title nor a list of step numbers may grow with it.
#[test]
fn a_long_run_does_not_write_a_long_sentence() {
    let steps: Vec<Value> = (0..200)
        .map(|index| {
            json!({ "index": index, "op": "click", "ok": true, "observed": {
                "target": { "id": "0x1", "class": "chromium.Chromium", "title": "x".repeat(400) },
                "observe_ms": 1 } })
        })
        .collect();
    let seen = Seen::read(&receipt(Some("input"), json!(steps))).unwrap();
    let said = seen.sentence();
    assert!(
        said.contains("steps 0, 1, 2, 3, 4, 5 and 194 more"),
        "{said}"
    );
    assert!(said.contains("clipped"), "{said}");
    assert!(
        said.chars().count() < 600,
        "{} chars: {said}",
        said.chars().count()
    );
}
