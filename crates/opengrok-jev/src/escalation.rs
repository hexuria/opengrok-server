//! Escalation outcomes: answered, or a labelled fallback (PUA spec §8).

use core::fmt;

use pua_core::{Answer, Question, StageKind, TrailRecord};

use crate::{JevError, ParseError, parse_reply};

/// Why the caller's fallback was used.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FallbackWhy {
    /// Jev did not answer (one of the four kinds, kept distinct).
    Jev(JevError),
    /// Jev answered, but the reply was refused (off-menu, malformed, inconsistent).
    Refused(ParseError),
    /// Jev answered on-menu, but a pack veto rejected it (e.g. a negated "don't stop" can't
    /// become an interrupt).
    Vetoed(String),
}

impl fmt::Display for FallbackWhy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Jev(e) => write!(f, "{e}"),
            Self::Refused(e) => write!(f, "reply refused: {e}"),
            Self::Vetoed(why) => write!(f, "vetoed: {why}"),
        }
    }
}

/// The outcome of one escalation. A fallback is always labelled as one, never recorded as an
/// answer PUA or Jev gave.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Escalation {
    /// Jev answered on-menu and no veto fired.
    Answered(Answer),
    /// The caller's declared fallback, with the reason.
    Fallback {
        /// Why.
        why: FallbackWhy,
        /// The caller's declared answer (often an abstain or the safe default).
        answer: Answer,
    },
}

impl Escalation {
    /// Whether this is a labelled fallback.
    pub const fn is_fallback(&self) -> bool {
        matches!(self, Self::Fallback { .. })
    }

    /// The answer to act on (Jev's, or the caller's fallback).
    pub const fn answer(&self) -> &Answer {
        match self {
            Self::Answered(a) | Self::Fallback { answer: a, .. } => a,
        }
    }

    /// The trail record for this outcome: stage `Escalation`, text `jev answered` or
    /// `fallback: <why>`.
    pub fn trail_record(&self) -> TrailRecord {
        match self {
            Self::Answered(_) => TrailRecord::new(StageKind::Escalation, "jev answered"),
            Self::Fallback { why, .. } => {
                TrailRecord::new(StageKind::Escalation, format!("fallback: {why}"))
            }
        }
    }
}

/// Runs the guard over a consumer's Jev call result.
///
/// `reply` is the raw reply body (or the Jev error) from the consumer's own door. `veto` is the
/// pack's veto check (return `Some(reason)` to reject). `fallback` is the caller's declared
/// answer for any failure; it is called at most once and labelled.
pub fn escalate(
    question: &Question,
    reply: Result<&str, JevError>,
    veto: impl FnOnce(&Answer) -> Option<String>,
    fallback: impl FnOnce(&FallbackWhy) -> Answer,
) -> Escalation {
    let why = match reply {
        Err(e) => FallbackWhy::Jev(e),
        Ok(body) => match parse_reply(body, question) {
            Err(e) => FallbackWhy::Refused(e),
            Ok(answer) => match veto(&answer) {
                None => return Escalation::Answered(answer),
                Some(reason) => FallbackWhy::Vetoed(reason),
            },
        },
    };
    let answer = fallback(&why);
    Escalation::Fallback { why, answer }
}
