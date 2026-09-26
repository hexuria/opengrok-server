//! How much of the conversation fits in the model's context, and what is left out when it does not.
//!
//! NOTHING COUNTED THE PROMPT (#90). A long thread grew until the provider refused it with a 400
//! the person read as "the gateway could not take this request" — after a wasted round trip, and
//! mid-run after tools had already acted. Nothing ever shrank the prompt, so every later turn on
//! that thread failed the same way.
//!
//! The estimate is deliberately pessimistic: UTF-8 bytes / 3, not the usual characters / 4, which
//! reads Chinese or Japanese text at a quarter of its real cost and JSON a quarter short. A
//! picture is a fixed cost; its base64 counted as text would read one screenshot as 50k tokens.

use crate::model::{ChatMessage, ModelRequest};

/// What one picture costs, whatever its size: the order of a high-detail screenshot.
const IMAGE_TOKENS: u64 = 1_600;
/// The framing every message costs beyond its words.
const MESSAGE_TOKENS: u64 = 4;
/// Room kept for the answer. No request sets `max_tokens`, so the provider's own default applies;
/// a prompt that fills the window exactly leaves the model no room to answer.
const ANSWER_TOKENS: u64 = 8_192;

fn text_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(3)
}

fn message_tokens(message: &ChatMessage) -> u64 {
    MESSAGE_TOKENS
        + text_tokens(&message.content)
        + IMAGE_TOKENS * message.images.len() as u64
        + message
            .tool_calls
            .iter()
            .map(|call| text_tokens(&call.name) + text_tokens(&call.arguments))
            .sum::<u64>()
}

/// A pessimistic count of what `request` costs the model to read.
pub fn estimate_tokens(request: &ModelRequest) -> u64 {
    request.system.as_deref().map_or(0, text_tokens)
        + request.messages.iter().map(message_tokens).sum::<u64>()
        + request
            .tools
            .iter()
            .map(|tool| text_tokens(&tool.to_string()))
            .sum::<u64>()
}

/// The part of a run's conversation that may be left out, fixed when the run starts.
///
/// THE BOUNDARY IS FOUND ONCE, AT ENTRY, and only moves as messages are dropped. Everything from
/// the last `user` message on is this turn — the person's words, and on a resumed run the run's
/// own prompt and its results — and is never dropped: the issue forbids losing a card or this
/// run's tool results, and a model that cannot see its own work repeats it. Searching again
/// later would find the wrap-up's own "[harness]" line. A turn that is only a `tool` message
/// protects back to the previous prompt, which is more than needed and never less.
#[derive(Debug, Clone)]
pub(crate) struct Window {
    /// Messages before this index are earlier turns.
    head: usize,
    left_out: usize,
    system: Option<String>,
}

impl Window {
    pub(crate) fn at_entry(request: &ModelRequest) -> Self {
        // No user message at all (an old run resumed with no stored prompt): protect everything,
        // and let `fit` fail closed if even that is too long.
        let head = request
            .messages
            .iter()
            .rposition(|message| message.role == "user")
            .unwrap_or(0);
        Self {
            head,
            left_out: 0,
            system: request.system.clone(),
        }
    }

    pub(crate) fn left_out(&self) -> usize {
        self.left_out
    }

    /// Leave out the oldest turns until `request` fits, and return its estimate; or say why it
    /// cannot. A request with no known limit is only counted.
    ///
    /// WHOLE TURNS, oldest first: a user message and everything up to the next one, so no
    /// question is left without its answer and no call without its result. Once trimming starts
    /// it goes down to 80% of the room, not to the brim: a window that slides one message a turn
    /// changes the prompt's prefix every turn, and the provider's prompt cache never hits.
    pub(crate) fn fit(&mut self, request: &mut ModelRequest) -> Result<u64, String> {
        let mut estimate = estimate_tokens(request);
        let Some(limit) = request.context_tokens else {
            return Ok(estimate);
        };
        let room = limit.saturating_sub(ANSWER_TOKENS.min(limit / 4));
        if estimate <= room {
            return Ok(estimate);
        }
        let target = room / 10 * 8;
        while estimate > target && self.head > 0 {
            loop {
                let dropped = request.messages.remove(0);
                estimate = estimate.saturating_sub(message_tokens(&dropped));
                self.head -= 1;
                self.left_out += 1;
                let next_is_a_turn = request
                    .messages
                    .first()
                    .is_none_or(|next| next.role == "user");
                if self.head == 0 || next_is_a_turn {
                    break;
                }
            }
        }
        if self.left_out > 0 {
            let note = format!(
                "[harness] The {} oldest messages of this conversation were left out so it fits \
                 the model's context. Ask the person if you need something from them.",
                self.left_out
            );
            request.system = Some(match &self.system {
                Some(system) => format!("{system}\n\n{note}"),
                None => note,
            });
        }
        let estimate = estimate_tokens(request);
        if estimate <= room {
            return Ok(estimate);
        }
        Err(format!(
            "This conversation is too long for {} (about {estimate} of {limit} tokens) even with \
             its earlier messages left out. Start a new conversation, or send less at once.",
            request.model
        ))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "../tests/unit/context_tests.rs"]
mod tests;
