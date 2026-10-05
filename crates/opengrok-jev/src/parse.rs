//! Wire reply → [`Answer`], under the off-menu guard (PUA spec §8).

use core::fmt;

use pua_core::{Answer, Confidence, OptionIndex, Options, Question, Ranked};
use serde_json::{Map, Value};

use crate::convert::{ConvertError, confidence_from_unit, noul_reading};
use crate::wire::{AnsweredQuestion, Reply};

/// Why a reply could not become an [`Answer`]. A reply that fails here is refused, with the
/// reason; the caller then takes its declared fallback ([`crate::escalate`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ParseError {
    /// The body is not a reply (malformed JSON or wrong shape).
    Deserialize(String),
    /// No answer in the reply carries the question's name.
    MissingAnswer(String),
    /// The answer is for a different question.
    NameMismatch {
        /// The asked question's name.
        expected: String,
        /// The answer's name.
        got: String,
    },
    /// The answer kind does not match the question shape.
    KindMismatch {
        /// Asked shape.
        expected: &'static str,
        /// Answered kind.
        got: &'static str,
    },
    /// A label, level or probability key that was not offered (the off-menu guard).
    OffMenu(String),
    /// An offered label has no probability, or its probability is not a number (a `null` is a
    /// float that could not be one). Never read as a probability of zero.
    MissingProbability(String),
    /// A float could not become a [`Confidence`].
    Convert {
        /// Which field.
        field: &'static str,
        /// Why.
        error: ConvertError,
    },
    /// The noul `yes`/`confidence` disagree with the `yes` probability under
    /// [`noul_reading`]: the reading drifted from the route's.
    InconsistentNoul,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Deserialize(e) => write!(f, "not a Jev reply: {e}"),
            Self::MissingAnswer(n) => write!(f, "no answer called \"{n}\""),
            Self::NameMismatch { expected, got } => {
                write!(f, "answer \"{got}\" is not for question \"{expected}\"")
            }
            Self::KindMismatch { expected, got } => {
                write!(f, "asked a {expected} question, got a {got} answer")
            }
            Self::OffMenu(l) => write!(f, "Jev answered off-menu: \"{l}\""),
            Self::MissingProbability(l) => write!(f, "no probability for \"{l}\""),
            Self::Convert { field, error } => write!(f, "{field}: {error}"),
            Self::InconsistentNoul => {
                f.write_str("noul reading disagrees with its yes probability")
            }
        }
    }
}

impl std::error::Error for ParseError {}

/// Parses a reply body and returns the answer to `question`, found by name.
///
/// # Errors
/// [`ParseError::Deserialize`], [`ParseError::MissingAnswer`], or any error of
/// [`parse_answer`].
pub fn parse_reply(body: &str, question: &Question) -> Result<Answer, ParseError> {
    let reply: Reply =
        serde_json::from_str(body).map_err(|e| ParseError::Deserialize(e.to_string()))?;
    let name = question.name().as_str();
    let answered = reply
        .answers
        .iter()
        .find(|a| answer_name(a) == name)
        .ok_or_else(|| ParseError::MissingAnswer(name.to_owned()))?;
    parse_answer(answered, question)
}

/// Converts one answered question. Every float is converted exactly once, here.
///
/// - Noul: `yes` and `confidence` come from the wire and must agree with [`noul_reading`] of
///   the `yes` probability.
/// - Choice: the label must be offered; every offered label needs a probability and no other
///   key may appear; the ranked list is built from those probabilities.
/// - Score: the `level` text must be one of the offered levels (the rung number alone is not
///   trusted: whether rungs count from 0 or 1 is not pinned by the route).
///
/// # Errors
/// Any [`ParseError`] except `Deserialize` and `MissingAnswer`.
pub fn parse_answer(
    answered: &AnsweredQuestion,
    question: &Question,
) -> Result<Answer, ParseError> {
    let got = answer_name(answered);
    if got != question.name().as_str() {
        return Err(ParseError::NameMismatch {
            expected: question.name().as_str().to_owned(),
            got: got.to_owned(),
        });
    }
    match (answered, question) {
        (
            AnsweredQuestion::Noul {
                yes,
                confidence,
                probabilities,
                ..
            },
            Question::Noul { .. },
        ) => {
            let conf = convert("confidence", *confidence)?;
            let p = number(probabilities, "yes")?;
            let derived = noul_reading(p).map_err(|error| ParseError::Convert {
                field: "probabilities.yes",
                error,
            })?;
            if derived != (*yes, conf) {
                return Err(ParseError::InconsistentNoul);
            }
            Ok(Answer::Noul {
                yes: *yes,
                confidence: conf,
            })
        }
        (
            AnsweredQuestion::Choice {
                choice,
                confidence,
                probabilities,
                ..
            },
            Question::Choice { options, .. },
        ) => {
            let option = options
                .index_of(choice)
                .ok_or_else(|| ParseError::OffMenu(choice.clone()))?;
            Ok(Answer::Choice {
                option,
                confidence: convert("confidence", *confidence)?,
                ranked: ranked(options, probabilities)?,
            })
        }
        (
            AnsweredQuestion::Score {
                score,
                level,
                confidence,
                ..
            },
            Question::Score { levels, .. },
        ) => {
            if !score.is_finite() {
                return Err(ParseError::Convert {
                    field: "score",
                    error: ConvertError::NonFinite,
                });
            }
            let text = match level {
                Some(Value::String(s)) => s.clone(),
                Some(other) => return Err(ParseError::OffMenu(other.to_string())),
                None => return Err(ParseError::OffMenu(format!("rung {score} (no level)"))),
            };
            let level = levels.index_of(&text).ok_or(ParseError::OffMenu(text))?;
            Ok(Answer::Score {
                level,
                confidence: convert("confidence", *confidence)?,
            })
        }
        (a, q) => Err(ParseError::KindMismatch {
            expected: shape_name(q),
            got: kind_name(a),
        }),
    }
}

fn answer_name(a: &AnsweredQuestion) -> &str {
    match a {
        AnsweredQuestion::Noul { name, .. }
        | AnsweredQuestion::Choice { name, .. }
        | AnsweredQuestion::Score { name, .. } => name,
    }
}

fn kind_name(a: &AnsweredQuestion) -> &'static str {
    match a {
        AnsweredQuestion::Noul { .. } => "noul",
        AnsweredQuestion::Choice { .. } => "choice",
        AnsweredQuestion::Score { .. } => "score",
    }
}

fn shape_name(q: &Question) -> &'static str {
    match q {
        Question::Noul { .. } => "noul",
        Question::Choice { .. } => "choice",
        Question::Score { .. } => "score",
    }
}

fn convert(field: &'static str, v: f64) -> Result<Confidence, ParseError> {
    confidence_from_unit(v).map_err(|error| ParseError::Convert { field, error })
}

fn number(map: &Map<String, Value>, key: &str) -> Result<f64, ParseError> {
    map.get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| ParseError::MissingProbability(key.to_owned()))
}

fn ranked(options: &Options, probs: &Map<String, Value>) -> Result<Ranked, ParseError> {
    if let Some(extra) = probs.keys().find(|k| options.index_of(k).is_none()) {
        return Err(ParseError::OffMenu(extra.clone()));
    }
    let mut entries: Vec<(OptionIndex, Confidence)> = Vec::with_capacity(probs.len());
    for i in options.indices() {
        let label = options.get(i).map_or("", |l| l.as_str());
        let p = number(probs, label)?;
        entries.push((i, convert("probabilities", p)?));
    }
    // Confidence descending, then option index ascending (PUA spec §5 rule 4).
    entries.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    Ranked::try_from(entries).map_err(|e| ParseError::Deserialize(e.to_owned()))
}
