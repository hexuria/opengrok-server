//! Bots messaging each other (#314): the side thread two of one person's Bots share, the refusal a
//! person's own words into it get, and the rows a Bot's main chat shows.
//!
//! Provenance: hexuria/opengrok-server#314, its "Contract of record" comment, agreed with
//! NativeChat on 2 Oct 2026. The thread id's shape, the 403's sentence and code, the CUSTOM name
//! and the entry's fields are that comment's, word for word: NativeChat matches on them, so a
//! tidier spelling here is a broken client there.

use serde_json::{Value, json};

use crate::agui::{Event, EventType};

/// Every pair thread's id starts so: `pair-{lo}-{hi}`, the two coworker ids sorted.
pub const PAIR_PREFIX: &str = "pair-";

/// The thread two Bots share, whichever of them writes: one per pair, used in both directions.
pub fn pair_thread(a: &str, b: &str) -> String {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    format!("{PAIR_PREFIX}{lo}-{hi}")
}

/// Whether a thread is a pair's, which nobody writes in but its two Bots.
pub fn is_pair_thread(thread_id: &str) -> bool {
    thread_id.starts_with(PAIR_PREFIX)
}

/// The two Bots a pair thread is between, lower id first, or `None` for any other thread.
///
/// A COWORKER ID IS `cw_` AND A UUID, which holds no `-cw_`, so the second id starts at the first
/// `-cw_`. The answer must spell the thread it came from, so an id that holds one after all reads
/// as no pair rather than as two Bots it is not between.
pub fn pair_peers(thread_id: &str) -> Option<(String, String)> {
    let rest = thread_id.strip_prefix(PAIR_PREFIX)?;
    let at = rest.find("-cw_")?;
    let (lo, hi) = (&rest[..at], &rest[at + 1..]);
    let spelt = !lo.is_empty() && lo < hi && pair_thread(lo, hi) == thread_id;
    spelt.then(|| (lo.to_string(), hi.to_string()))
}

/// A person's main chat with one Bot: the thread that Bot's timeline rows replay on.
pub fn chat_thread(coworker_id: &str) -> String {
    format!("gateway-{coworker_id}")
}

/// The Bot whose main chat this is, for `chat_thread`'s own spelling.
pub fn chat_of(thread_id: &str) -> Option<&str> {
    thread_id
        .strip_prefix("gateway-")
        .filter(|id| !id.is_empty())
}

/// What a person's own words into a pair thread are answered with: 403 and this body.
pub const READ_ONLY_ERROR: &str =
    "This side thread is between two of your Bots. You can read it, not write in it.";
pub const READ_ONLY_CODE: &str = "read-only-thread";

pub fn read_only_body() -> Value {
    json!({ "error": READ_ONLY_ERROR, "code": READ_ONLY_CODE })
}

/// The CUSTOM a timeline row goes out live in: `{v, op, threadId, entry}`, beside the stored row a
/// replay of `threadId` carries in `timeline`. Clients de-duplicate the two by the entry's `id`.
pub const TIMELINE_NAME: &str = "opengrok.timeline";
pub const TIMELINE_V: u32 = 1;
/// The `op` of a row written now. The contract names no word; `created` is the one the queue's
/// own CUSTOM (`pending-user-message`) uses for the same thing.
pub const TIMELINE_CREATED: &str = "created";

pub fn timeline_frame(op: &str, thread_id: &str, entry: &Value, at_ms: i64) -> Event {
    let value = json!({ "v": TIMELINE_V, "op": op, "threadId": thread_id, "entry": entry });
    Event::new(EventType::Custom, at_ms)
        .with("name", TIMELINE_NAME)
        .with("value", value)
}

/// One Bot a `messaged` row names: who, as the row shows them, and the pair thread its chip opens.
pub struct Messaged<'a> {
    pub coworker_id: &'a str,
    pub name: &'a str,
    pub thread_id: &'a str,
}

/// The `messaged` row a sender's main chat gets for one `message_bot` call, a broadcast's one row
/// naming every Bot it reached. `text` is the row's own words, so a client that does not know a
/// kind still has a line to show for it.
pub fn messaged_entry(
    id: &str,
    at_ms: i64,
    coworker_id: &str,
    to: &[Messaged<'_>],
    run_id: &str,
) -> Value {
    let names: Vec<&str> = to.iter().map(|bot| bot.name).collect();
    let text = match names.as_slice() {
        [] => "Messaged nobody".to_string(),
        [one] => format!("Messaged {one}"),
        [rest @ .., last] => format!("Messaged {} and {last}", rest.join(", ")),
    };
    let to: Vec<Value> = to
        .iter()
        .map(|bot| json!({ "coworkerId": bot.coworker_id, "name": bot.name, "threadId": bot.thread_id }))
        .collect();
    json!({ "id": id, "atMs": at_ms, "kind": "messaged", "coworkerId": coworker_id, "text": text,
            "to": to, "runId": run_id })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "../tests/unit/pair.rs"]
mod tests;
