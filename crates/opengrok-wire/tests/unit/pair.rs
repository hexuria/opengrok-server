//! `pair.rs`: the thread id two Bots share, read back, and the row a broadcast leaves.

use super::*;

const LO: &str = "cw_0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";
const HI: &str = "cw_0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5c";

/// One thread per pair, whichever Bot writes: the ids sorted.
#[test]
fn a_pair_has_one_thread_whichever_bot_writes() {
    let thread = pair_thread(HI, LO);
    assert_eq!(thread, pair_thread(LO, HI));
    assert_eq!(thread, format!("pair-{LO}-{HI}"));
    assert!(is_pair_thread(&thread));
    assert_eq!(pair_peers(&thread), Some((LO.to_string(), HI.to_string())));
}

/// A thread that only looks like a pair's names nobody: not sorted, one id, not a pair at all.
#[test]
fn a_thread_that_is_not_spelt_as_a_pair_names_no_bots() {
    assert_eq!(pair_peers(&format!("pair-{HI}-{LO}")), None);
    assert_eq!(pair_peers(&format!("pair-{LO}")), None);
    assert_eq!(pair_peers(&format!("gateway-{LO}")), None);
    assert!(!is_pair_thread(&chat_thread(LO)));
    assert_eq!(chat_of(&chat_thread(LO)), Some(LO));
    assert_eq!(chat_of("gateway-"), None);
}

/// The refusal is the contract's sentence and code, exactly.
#[test]
fn the_read_only_refusal_is_the_contracts() {
    assert_eq!(
        read_only_body(),
        json!({ "error": "This side thread is between two of your Bots. You can read it, not write in it.",
                "code": "read-only-thread" })
    );
}

/// A broadcast is one row naming every Bot, each with the thread its chip opens.
#[test]
fn a_broadcast_is_one_row_naming_every_bot() {
    let to = [
        Messaged {
            coworker_id: "cw_a",
            name: "Ada",
            thread_id: "pair-cw_a-cw_z",
        },
        Messaged {
            coworker_id: "cw_b",
            name: "Bob",
            thread_id: "pair-cw_b-cw_z",
        },
        Messaged {
            coworker_id: "cw_c",
            name: "Cy",
            thread_id: "pair-cw_c-cw_z",
        },
    ];
    let entry = messaged_entry("tl_1", 7, "cw_z", &to, "run-1");
    assert_eq!(entry["kind"], "messaged");
    assert_eq!(entry["text"], "Messaged Ada, Bob and Cy");
    assert_eq!(
        entry["to"][1],
        json!({ "coworkerId": "cw_b", "name": "Bob", "threadId": "pair-cw_b-cw_z" })
    );
    assert_eq!(entry["runId"], "run-1");
    assert_eq!(
        messaged_entry("tl_2", 7, "cw_z", &to[..1], "r")["text"],
        "Messaged Ada"
    );
    let frame =
        serde_json::to_value(timeline_frame(TIMELINE_CREATED, "gateway-cw_z", &entry, 7)).unwrap();
    assert_eq!(frame["type"], "CUSTOM");
    assert_eq!(frame["name"], "opengrok.timeline");
    assert_eq!(frame["value"]["v"], 1);
    assert_eq!(frame["value"]["threadId"], "gateway-cw_z");
    assert_eq!(frame["value"]["entry"], entry);
}
