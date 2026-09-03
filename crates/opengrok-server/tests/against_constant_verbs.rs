//! The exemption list may not drift away from the match statement it copies.
//!
//! `ANSWERS_A_CONSTANT` names the seam-A verbs that are NOT authorisation-checked, because they
//! answer a fixed shape the renderer requires and a 404 would divert it rather than protect
//! anything. It is a hand-written second copy of something the dispatch already knows, and a
//! second copy with nothing forcing agreement is exactly how a catalogue goes stale — the same
//! shape as a model list that advertises models nobody can call.
//!
//! This is that mechanism. It reads the dispatch and holds it to two rules. No database.
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeSet;

use opengrok_server::gateway::routes::ANSWERS_A_CONSTANT;

const DISPATCH: &str = include_str!("../src/gateway/routes.rs");

/// Every `"a" | "b" | "c" => ` arm in the dispatch, as its set of verb names. Arms are found by
/// their `=>`, so a single-verb arm is a set of one and is checked the same way.
fn arms() -> Vec<BTreeSet<String>> {
    let mut arms = Vec::new();
    let mut pending: BTreeSet<String> = BTreeSet::new();
    for line in DISPATCH.lines() {
        let line = line.trim();
        // A line of the arm's head: quoted names, optionally continued with a trailing `|`.
        if !line.starts_with('"') && !line.starts_with('|') {
            pending.clear();
            continue;
        }
        for name in line.split('|') {
            let name = name.trim();
            if let Some(rest) = name.strip_prefix('"')
                && let Some(verb) = rest.split('"').next()
                && !verb.is_empty()
            {
                pending.insert(verb.to_string());
            }
        }
        if line.contains("=>") {
            if !pending.is_empty() {
                arms.push(std::mem::take(&mut pending));
            }
            pending.clear();
        }
    }
    arms
}

/// An arm is all-exempt or all-gated. Splitting one makes identical verbs behave differently for
/// the same id: the client passes an agent id on all four of the `setAgentUnread` family
/// (`source/host/host-gateway-api.ts:361-367`), so a gated sibling answers 404 to somebody using
/// a shared coworker where its arm-mate answers a shape. That breakage cannot appear until
/// sharing is live, which is why it needs a test rather than a reading.
#[test]
fn no_match_arm_is_half_exempt() {
    let listed: BTreeSet<&str> = ANSWERS_A_CONSTANT.iter().copied().collect();
    let mut split = Vec::new();
    for arm in arms() {
        if arm.len() < 2 {
            continue;
        }
        let (in_list, out): (Vec<_>, Vec<_>) =
            arm.iter().partition(|verb| listed.contains(verb.as_str()));
        if !in_list.is_empty() && !out.is_empty() {
            split.push(format!(
                "exempt {in_list:?} but gated {out:?} — same arm, same reply"
            ));
        }
    }
    assert!(
        split.is_empty(),
        "these match arms are half-exempt; list all of an arm or none of it:\n  {}",
        split.join("\n  ")
    );
}

/// A name on the list that the dispatch no longer has is a rename nobody finished. Harmless
/// today and misleading forever: it reads as a decision about a verb that does not exist.
#[test]
fn every_exempt_verb_is_still_a_verb() {
    let known: BTreeSet<String> = arms().into_iter().flatten().collect();
    let stale: Vec<&&str> = ANSWERS_A_CONSTANT
        .iter()
        .filter(|verb| !known.contains(**verb))
        .collect();
    assert!(
        stale.is_empty(),
        "these are exempt from the coworker check but are not verbs any more: {stale:?}"
    );
}

/// The list is sorted, so a reader can find a name and a diff shows one line per change.
#[test]
fn the_list_is_sorted() {
    let mut sorted = ANSWERS_A_CONSTANT.to_vec();
    sorted.sort_unstable();
    assert_eq!(ANSWERS_A_CONSTANT, sorted.as_slice());
}
