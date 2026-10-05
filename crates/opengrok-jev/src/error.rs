//! The four Jev failure kinds kept apart on purpose (PUA spec §8).

use core::fmt;

/// Why Jev did not answer. Four kinds, never collapsed into one "jev failed" string: a bad
/// question, an unreachable service, a timeout and a refusal are four different people's
/// problems (`opengrok-server src/jev/mod.rs`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum JevError {
    /// The question could never have been asked (empty name, empty rubric, …). Never retried.
    Asked(String),
    /// The service could not be reached.
    Unreachable(String),
    /// The service did not answer within the caller's patience. Duration is whole milliseconds.
    TimedOut {
        /// Elapsed wait, in milliseconds.
        millis: u64,
    },
    /// The service refused (status + message).
    Refused {
        /// HTTP status from the service.
        status: u16,
        /// Refusal sentence.
        message: String,
    },
}

impl fmt::Display for JevError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Asked(s) => write!(f, "that question could not be put to Jev: {s}"),
            Self::Unreachable(s) => write!(f, "Jev is unreachable: {s}"),
            Self::TimedOut { millis } => {
                write!(f, "Jev did not answer within {millis} ms")
            }
            Self::Refused { status, message } => write!(f, "Jev refused: {status} {message}"),
        }
    }
}

impl std::error::Error for JevError {}
