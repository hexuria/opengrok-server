//! [`Question`] → wire request shapes (`POST /jev/ask` body).

use core::fmt;

use pua_core::Question;
use serde_json::Value;

use crate::wire::{AskRequest, AskedChoice, AskedQuestion};

/// Why a request could not be rendered. These mirror the route's own refusals, so a consumer
/// learns before a request is prepared (and is never billed for a question Jev must refuse).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RenderError {
    /// Empty (or whitespace-only) instructions for the named question.
    EmptyInstructions(String),
    /// No questions at all.
    NoQuestions,
    /// Two questions share a name; answers are keyed by name, so one would be lost.
    DuplicateName(String),
    /// The state is not text, an object or an array.
    BadState,
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyInstructions(n) => write!(f, "the question called \"{n}\" has no words"),
            Self::NoQuestions => f.write_str("ask Jev at least one question"),
            Self::DuplicateName(n) => write!(f, "two questions are both called \"{n}\""),
            Self::BadState => f.write_str("state must be text, an object or an array"),
        }
    }
}

impl std::error::Error for RenderError {}

/// Renders a PUA [`Question`] as a Jev wire question. `instructions` is the classifier text
/// (Jev's field name); the PUA question name is the answer key. Options and levels keep their
/// order, so option 0 (the safe default) stays first.
///
/// # Errors
/// [`RenderError::EmptyInstructions`].
pub fn render_question(q: &Question, instructions: &str) -> Result<AskedQuestion, RenderError> {
    let name = q.name().as_str().to_owned();
    let instructions = instructions.trim();
    if instructions.is_empty() {
        return Err(RenderError::EmptyInstructions(name));
    }
    let instructions = instructions.to_owned();
    let labels = |o: &pua_core::Options| -> Vec<String> {
        o.labels().iter().map(|l| l.as_str().to_owned()).collect()
    };
    Ok(match q {
        Question::Noul { .. } => AskedQuestion::Noul {
            name,
            instructions,
            yes_means: None,
            no_means: None,
        },
        Question::Choice { options, .. } => AskedQuestion::Choice {
            name,
            instructions,
            choices: labels(options).into_iter().map(AskedChoice::Bare).collect(),
        },
        Question::Score { levels, .. } => AskedQuestion::Score {
            name,
            instructions,
            levels: labels(levels),
        },
    })
}

/// Builds the whole request body for `questions` about `state`.
///
/// # Errors
/// [`RenderError::NoQuestions`], [`RenderError::DuplicateName`], [`RenderError::BadState`], or
/// [`RenderError::EmptyInstructions`] from any question.
pub fn render_ask(
    state: Value,
    questions: &[(&Question, &str)],
    model: Option<&str>,
) -> Result<AskRequest, RenderError> {
    if !matches!(state, Value::String(_) | Value::Object(_) | Value::Array(_)) {
        return Err(RenderError::BadState);
    }
    if questions.is_empty() {
        return Err(RenderError::NoQuestions);
    }
    let mut out: Vec<AskedQuestion> = Vec::with_capacity(questions.len());
    for (i, (q, instructions)) in questions.iter().enumerate() {
        let name = q.name().as_str();
        if questions[..i]
            .iter()
            .any(|(p, _)| p.name().as_str() == name)
        {
            return Err(RenderError::DuplicateName(name.to_owned()));
        }
        out.push(render_question(q, instructions)?);
    }
    Ok(AskRequest {
        state,
        model: model
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_owned),
        questions: out,
    })
}
