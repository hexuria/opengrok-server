//! `opengrok-jev`: Jev wire shapes (noul / choice / score) to and from PUA core types.
//!
//! Moved from `hexuria/pua` `crates/pua-jev` in PUA's Phase 2 engine/consumer split, so the PUA
//! engine has no floats at all. The Jev door is opengrok's; this crate is the one place a Jev
//! float becomes a PUA `Confidence`.
//!
//! This crate is the **only** place a float may enter a PUA decision. It converts Jev's
//! `f64` probabilities and confidences into [`Confidence`] once, then every later stage works in
//! millis. There is no HTTP client: consumers keep their own `JevDoor` and feed us the JSON.
//!
//! Wire shapes mirror `crates/opengrok-server/src/jev/routes.rs` (`AskedQuestion` /
//! `AnsweredQuestion`), checked against it at the time of the move: same `kind` tag and camelCase
//! fields. Keep the two in step.
//!
//! - [`render_question`] / [`render_ask`] turn a [`Question`] into the body Jev expects.
//! - [`parse_reply`] / [`parse_answer`] turn a reply into an [`Answer`] under the off-menu guard,
//!   converting each float once ([`confidence_from_unit`]).
//! - [`noul_reading`] is for consumers that hold a raw noul probability (SDK users): a noul
//!   probability is not a confidence.
//! - [`escalate`] / [`Escalation`] label every outcome: either Jev answered (and no pack veto
//!   fired), or the caller took its declared fallback, with the reason ([`FallbackWhy`]). A
//!   fallback is never recorded as an answer PUA or Jev gave.
//!
//! ```
//! use pua_core::{Answer, OptionIndex, Question};
//! use opengrok_jev::{ParseError, parse_reply, render_question};
//!
//! let q = Question::choice("delivery", &["queue", "steer", "interrupt"])?;
//! let wire = render_question(&q, "How should this message be delivered?")?;
//! assert_eq!(serde_json::to_value(&wire)?["choices"][2], "interrupt");
//!
//! let body = r#"{"model": "jev", "usage": {}, "answers": [{"kind": "choice",
//!   "name": "delivery", "choice": "steer", "confidence": 0.71,
//!   "probabilities": {"queue": 0.2, "steer": 0.71, "interrupt": 0.09}}]}"#;
//! let answer = parse_reply(body, &q)?;
//! assert_eq!(answer.chosen(), Some(OptionIndex::new(1)));
//!
//! let off = body.replace(r#""choice": "steer""#, r#""choice": "halt""#);
//! assert_eq!(parse_reply(&off, &q), Err(ParseError::OffMenu("halt".into())));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! [`Question`]: pua_core::Question
//! [`Answer`]: pua_core::Answer
//! [`Confidence`]: pua_core::Confidence
#![forbid(unsafe_code)]

mod convert;
mod error;
mod escalation;
mod parse;
mod render;
mod wire;

#[cfg(test)]
mod tests;

pub use convert::{ConvertError, YES_ABOVE, confidence_from_unit, noul_reading};
pub use error::JevError;
pub use escalation::{Escalation, FallbackWhy, escalate};
pub use parse::{ParseError, parse_answer, parse_reply};
pub use render::{RenderError, render_ask, render_question};
pub use wire::{AnsweredQuestion, AskRequest, AskedChoice, AskedQuestion, Reply, SpentTokens};

/// Algorithm tag folded into pack `DataVersion`s that escalate through this adapter.
pub const ALGORITHM_TAG: &str = "pua-jev/1";
