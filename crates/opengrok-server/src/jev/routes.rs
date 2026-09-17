//! `POST /jev/ask` — put questions to Jev and get typed answers back.
//!
//! The body is the state Jev judges and the questions to ask about it; the reply is one answer
//! per question, each with the confidence in it and the probability of every option:
//!
//! ```json
//! { "state": {"asked": "search youtube for kabisado", "did": "searched, then played a video"},
//!   "questions": [
//!     {"name": "done", "kind": "noul", "instructions": "Is what was asked now true?"},
//!     {"name": "tone", "kind": "choice", "instructions": "How did it go?",
//!      "choices": ["fine", {"label": "overshot", "means": "it did more than was asked"}]},
//!     {"name": "effort", "kind": "score", "instructions": "How much work is left?",
//!      "levels": ["none", "a little", "most of it"]}] }
//! ```
//!
//! THE FIELD NAMES ARE JEV'S OWN WHERE JEV HAS ONE. `instructions` is what the classifier calls a
//! question's text, so it is what this calls it too — somebody reading TypeSafe's documentation
//! can write this body without a translation table. The three criteria shapes are named for what
//! they are (`choices`, `levels`, `yesMeans`/`noMeans`) rather than sharing the SDK's one word
//! `criteria` for three different structures, which is the one place a shared vocabulary would
//! cost more than it buys.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{Answer, Ask, JevError, JsonContent, Judgement, NoulCriteria, Question};
use crate::agui::AgUiState;
use crate::agui::routes::account_from_bearer;

/// Above this a noul answer reads as yes. Exactly a half is a coin, and calling it yes is
/// arbitrary — which is why the confidence beside it is the number a caller should act on, and
/// why this constant exists rather than a bare `0.5` in the middle of the mapping.
const YES_ABOVE: f64 = 0.5;

pub fn router(state: AgUiState) -> Router {
    Router::new().route("/jev/ask", post(ask)).with_state(state)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AskRequest {
    /// What Jev judges: text, an object, or an array.
    state: Value,
    /// The Jev model for this one call. Absent uses the deployment's configured default.
    #[serde(default)]
    model: Option<String>,
    questions: Vec<AskedQuestion>,
}

/// One question, tagged by the kind of answer it declares.
#[derive(Debug, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum AskedQuestion {
    /// Yes or no, answered as a probability.
    Noul {
        name: String,
        instructions: String,
        /// What a yes would mean, when the question alone does not make it obvious.
        #[serde(default)]
        yes_means: Option<String>,
        #[serde(default)]
        no_means: Option<String>,
    },
    /// One label out of several.
    Choice {
        name: String,
        instructions: String,
        choices: Vec<AskedChoice>,
    },
    /// A rung on a rubric, given in order.
    Score {
        name: String,
        instructions: String,
        levels: Vec<String>,
    },
}

/// A label, or a label with what it means. Both, because a bare list of words is the common case
/// and a description is what makes two similar labels distinguishable to a classifier.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum AskedChoice {
    Bare(String),
    Described { label: String, means: String },
}

impl AskedQuestion {
    fn name(&self) -> &str {
        match self {
            Self::Noul { name, .. } | Self::Choice { name, .. } | Self::Score { name, .. } => name,
        }
    }

    fn instructions(&self) -> &str {
        match self {
            Self::Noul { instructions, .. }
            | Self::Choice { instructions, .. }
            | Self::Score { instructions, .. } => instructions,
        }
    }
}

/// One question for the SDK, or the sentence refusing it.
fn question_from(asked: &AskedQuestion) -> Result<(String, Question), String> {
    let name = asked.name().trim();
    if name.is_empty() {
        return Err("every question needs a name: its answer comes back under it".to_string());
    }
    let instructions = asked.instructions().trim();
    if instructions.is_empty() {
        return Err(format!("the question called \"{name}\" has no words to it"));
    }
    let question = match asked {
        AskedQuestion::Noul {
            yes_means,
            no_means,
            ..
        } => {
            let question = Question::noul(instructions);
            match (yes_means, no_means) {
                (None, None) => question,
                (yes, no) => {
                    let mut criteria = NoulCriteria::new();
                    if let Some(yes) = yes {
                        criteria = criteria.yes(yes.as_str());
                    }
                    if let Some(no) = no {
                        criteria = criteria.no(no.as_str());
                    }
                    question.with_noul_criteria(criteria)
                }
            }
        }
        AskedQuestion::Choice { choices, .. } => {
            // ONE LABEL IS NOT A CHOICE. Jev would dutifully answer it with that label at a
            // probability of one, having decided nothing — a question whose answer is known
            // before it is asked is a call that costs money and tells the caller what it already
            // knew, so it is refused here rather than billed.
            if choices.len() < 2 {
                return Err(format!(
                    "the question called \"{name}\" needs at least two choices to choose between"
                ));
            }
            let labels: Vec<(String, Option<JsonContent>)> = choices
                .iter()
                .map(|choice| match choice {
                    AskedChoice::Bare(label) => (label.clone(), None),
                    AskedChoice::Described { label, means } => {
                        (label.clone(), Some(JsonContent::String(means.clone())))
                    }
                })
                .collect();
            if labels.iter().any(|(label, _)| label.trim().is_empty()) {
                return Err(format!(
                    "the question called \"{name}\" has a choice with no label"
                ));
            }
            Question::choice(instructions, labels)
        }
        AskedQuestion::Score { levels, .. } => {
            // The SDK refuses an empty rubric at send time with a sentence of its own; saying it
            // here means the caller is told before a request is prepared, and told which question.
            if levels.is_empty() {
                return Err(format!(
                    "the question called \"{name}\" has no levels to score against"
                ));
            }
            Question::score(instructions, levels.iter().map(String::as_str))
        }
    };
    Ok((name.to_string(), question))
}

/// The whole body as one `Ask`, or the sentence refusing it.
fn ask_from(request: AskRequest) -> Result<Ask, String> {
    if request.questions.is_empty() {
        return Err("ask Jev at least one question".to_string());
    }
    let state = JsonContent::from_value(request.state).map_err(|error| error.to_string())?;
    let mut questions: Vec<(String, Question)> = Vec::with_capacity(request.questions.len());
    for asked in &request.questions {
        let (name, question) = question_from(asked)?;
        // A DUPLICATE NAME LOSES AN ANSWER SILENTLY. Answers come back keyed by name, so the
        // second question of a pair would overwrite the first and the caller would get fewer
        // answers than questions with nothing saying why.
        if questions.iter().any(|(existing, _)| existing == &name) {
            return Err(format!(
                "two questions are both called \"{name}\"; an answer is found by its name, so the \
                 names have to differ"
            ));
        }
        questions.push((name, question));
    }
    Ok(Ask {
        state,
        questions,
        model: request.model.filter(|model| !model.trim().is_empty()),
    })
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Answered {
    /// The model that answered, as Jev names it.
    model: String,
    /// `null` when the service sent no request id, never an invented one.
    request_id: Option<String>,
    usage: SpentTokens,
    /// In the order the questions were asked.
    answers: Vec<AnsweredQuestion>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SpentTokens {
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum AnsweredQuestion {
    Noul {
        name: String,
        yes: bool,
        confidence: f64,
        probabilities: Map<String, Value>,
    },
    Choice {
        name: String,
        choice: String,
        confidence: f64,
        probabilities: Map<String, Value>,
    },
    Score {
        name: String,
        score: f64,
        /// The rubric text for the score Jev picked, out of the legend it sent back. `null` when
        /// the legend has no entry at that rung — reported as missing rather than guessed at.
        level: Option<Value>,
        confidence: f64,
        legend: Map<String, Value>,
        probabilities: Map<String, Value>,
    },
}

/// A JSON number, or `null` where a float cannot be one (NaN, infinity). Never a silent zero:
/// a probability that did not arrive must not read as a probability of none.
fn number(value: f64) -> Value {
    serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
}

fn answered_question(name: String, answer: Answer) -> AnsweredQuestion {
    match answer {
        Answer::Noul(noul) => {
            // A NOUL ANSWER IS ONE NUMBER, AND IT IS NOT A CONFIDENCE. It is the probability that
            // the answer is yes, which the SDK's own notes are explicit about; reporting it as a
            // confidence would read a confident no — 0.02 — as almost no confidence at all, and a
            // caller thresholding on it would act on exactly the wrong half of the answers. So
            // the reading and the confidence in it are derived here, once, and every answer this
            // route returns carries the same two things.
            let yes = noul.noul >= YES_ABOVE;
            let confidence = if yes { noul.noul } else { 1.0 - noul.noul };
            let mut probabilities = Map::new();
            probabilities.insert("yes".to_string(), number(noul.noul));
            probabilities.insert("no".to_string(), number(1.0 - noul.noul));
            AnsweredQuestion::Noul {
                name,
                yes,
                confidence,
                probabilities,
            }
        }
        Answer::Choice(choice) => {
            let probabilities = choice
                .probabilities
                .into_iter()
                .map(|(label, probability)| (label, number(probability)))
                .collect();
            AnsweredQuestion::Choice {
                name,
                choice: choice.choice,
                confidence: choice.confidence,
                probabilities,
            }
        }
        Answer::Score(score) => {
            let legend: Map<String, Value> = score
                .legend
                .into_iter()
                .map(|(rung, text)| (rung.to_string(), Value::from(text)))
                .collect();
            let probabilities = score
                .probabilities
                .into_iter()
                .map(|(rung, probability)| (rung.to_string(), number(probability)))
                .collect();
            // The score is a rung of the rubric, sent as a float. `level` is the words for it,
            // because the number alone means nothing without the legend beside it.
            let level =
                rung_of(score.score).and_then(|rung| legend.get(&rung.to_string()).cloned());
            AnsweredQuestion::Score {
                name,
                score: score.score,
                level,
                confidence: score.confidence,
                legend,
                probabilities,
            }
        }
    }
}

/// Which rung of the rubric a score names, or `None` for a number that cannot be one.
fn rung_of(score: f64) -> Option<u32> {
    if !score.is_finite() || score < 0.0 || score > f64::from(u32::MAX) {
        return None;
    }
    Some(score.round() as u32)
}

fn answered_from(judgement: Judgement) -> Answered {
    Answered {
        model: judgement.model,
        request_id: judgement.request_id,
        usage: SpentTokens {
            input_tokens: judgement.usage.input_tokens,
            output_tokens: judgement.usage.output_tokens,
        },
        answers: judgement
            .answers
            .into_iter()
            .map(|(name, answer)| answered_question(name, answer))
            .collect(),
    }
}

/// What a failure to answer is, as a status.
///
/// FOUR FAILURES, FOUR ANSWERS. A caller has to be able to tell a bad question (fix it) from an
/// unreachable service (try later) from a slow one (try later, with more patience) from a refusal
/// by TypeSafe (read the sentence). Collapsing them into one 500 is what makes an expired key
/// look like a network blip for a week.
fn refusal(error: &JevError) -> (StatusCode, String) {
    let status = match error {
        JevError::Asked(_) => StatusCode::BAD_REQUEST,
        JevError::Unreachable(_) => StatusCode::BAD_GATEWAY,
        JevError::TimedOut(_) => StatusCode::GATEWAY_TIMEOUT,
        JevError::Refused { status: 429, .. } => StatusCode::TOO_MANY_REQUESTS,
        // Everything else TypeSafe said, including a refused credential: a 502, because the thing
        // that failed is upstream of this server and the caller did nothing wrong.
        JevError::Refused { .. } => StatusCode::BAD_GATEWAY,
    };
    (status, error.to_string())
}

/// `POST /jev/ask` — the questions, and what Jev said.
///
/// Signed in is enough. A question carries no coworker and touches no coworker's data: the state
/// is whatever the caller sent, and the answer goes back to the caller and nowhere else.
///
/// AND NOTHING IS DECIDED HERE WHEN JEV CANNOT ANSWER. There is no default answer, no cached one
/// and no cheaper judge standing by — the refusal goes back with the reason in it, and the caller
/// decides what to do without one. That boundary is the point of this route: the layer that asked
/// the question is the only layer that knows what an unanswered one costs.
async fn ask(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Json(request): Json<AskRequest>,
) -> Response {
    let Some(_account) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let Some(jev) = state.auth.jev.clone() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "this deployment has no Jev key configured (OG_JEV_API_KEY), so there is nothing here \
             to ask",
        )
            .into_response();
    };
    let ask = match ask_from(request) {
        Ok(ask) => ask,
        Err(why) => return (StatusCode::BAD_REQUEST, why).into_response(),
    };
    match jev.ask(ask).await {
        Ok(judgement) => Json(answered_from(judgement)).into_response(),
        Err(error) => {
            let (status, sentence) = refusal(&error);
            (status, sentence).into_response()
        }
    }
}
