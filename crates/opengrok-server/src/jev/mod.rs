#![cfg(feature = "jev")]
//! Jev: the classifier this server asks, as opposed to the model it talks to.
//!
//! Every other model call here is a conversation — a prompt goes out, words come back, and what
//! they mean is somebody else's problem. Jev is the other shape: each question declares the form
//! of its own answer, and what comes back is typed, with a calibrated probability for every
//! option. That difference is the whole of this module. A classifier reached through the chat
//! door would arrive as prose that has to be parsed back into a decision, which is precisely the
//! guessing it exists to remove.
//!
//! NO FALLBACKS LIVE HERE, DELIBERATELY. When Jev cannot answer, this says so — a refusal with
//! the reason in it, and no answer. Deciding what to do instead (carry on as before, ask a
//! cheaper judge, stop) belongs to the layer that knows what the answer was for; the workflow
//! engine is that layer. A fallback quietly applied here would make the failure invisible to the
//! only code that can act on it, and "the classifier said carry on" and "the classifier was down"
//! would become the same event in the transcript.
//!
//! THE KEY IS THE ONE PROVIDER CREDENTIAL THIS SERVER HOLDS, and it is here because the gateway
//! does not host Jev yet (open-ai-gateway#83). Every model call leaves through open-ai-gateway on
//! a route rather than a key (CLAUDE.md #4), and a classifier should reach it the same way; until
//! it can, `OG_JEV_API_KEY` is read from the process environment and goes nowhere else — not a
//! coworker's row, not a client payload, not a log line, which is what that rule actually
//! forbids. When the gateway hosts Jev, `OG_JEV_BASE_URL` points at it and this key can go.
//!
//! SPEND IS NOT METERED HERE, AND CANNOT HONESTLY BE. Model spend in this server is the gateway's
//! ledger: every model call exits through open-ai-gateway on a coworker's own key, the gateway
//! counts the tokens, and `spend.rs` reads that meter before the next call. Nothing in this repo
//! writes a token count anywhere — `gateway_admin` has no endpoint that would take one, and
//! `ModelDelta` carries no usage at all. A Jev call made straight to TypeSafe's API never touches
//! that ledger, so the counts it returns are reported rather than banked: `usage` and the request
//! id come back on every answer and are logged beside it, and no second ledger is invented to put
//! them in. The way to make Jev spend count like model spend is `OG_JEV_BASE_URL` — point it at
//! open-ai-gateway once the gateway hosts Jev (open-ai-gateway#83) and the gateway meters these
//! calls exactly as it meters the rest, with nothing here to change.

pub mod client;
pub mod mock;
pub mod routes;

use std::sync::Arc;

pub use client::{JevConfig, TypeSafeJev, from_env};
pub use mock::MockJev;
/// The SDK's own vocabulary, re-exported so a caller in this workspace names one crate rather
/// than two. The three answer kinds and their decoding are the reason to depend on the SDK at
/// all; re-declaring them here would be a second definition of the same wire, free to drift.
pub use typesafe_sdk::{
    Answer, ChoiceAnswer, JsonContent, NoulAnswer, NoulCriteria, Question, ScoreAnswer, Usage,
};

/// One request to Jev: the state it judges, and the named questions about it.
///
/// NAMED, not a list. The SDK keys its answers by question name and this server looks them up by
/// name, so two questions called the same thing would silently become one — `routes` refuses that
/// in words rather than letting an `IndexMap` drop one on the floor.
#[derive(Debug, Clone)]
pub struct Ask {
    /// What Jev is judging: text, an object or an array. Never a bare number or null — the SDK
    /// rejects those, and `routes` says so before the call is made.
    pub state: JsonContent,
    pub questions: Vec<(String, Question)>,
    /// The Jev model for this one call. `None` uses the deployment's configured default.
    pub model: Option<String>,
}

/// What came back, plus the two things an accounting trail needs.
#[derive(Debug, Clone)]
pub struct Judgement {
    /// The model that actually answered, as Jev names it — not what was asked for.
    pub model: String,
    /// `x-typesafe-request-id`, when the service sent one. `None` rather than an invented id: a
    /// made-up correlation id is worse than an absent one, because it looks like it would match
    /// something on the other side.
    pub request_id: Option<String>,
    pub usage: Usage,
    /// In the order the questions were asked, so a caller can walk answers beside questions.
    pub answers: Vec<(String, Answer)>,
}

/// Why Jev did not answer.
///
/// FOUR KINDS, KEPT APART ON PURPOSE. They are four different people's problems: a malformed
/// question is ours, an unreachable service is the network's, a timeout is a slow answer that may
/// well succeed next time, and a refusal is TypeSafe telling us something specific. Flattening
/// them into one "jev failed" string is the bug that makes an expired key look like a blip and a
/// blip look like a bad question — the vocabulary mirrors `ModelError`'s for the same reason.
#[derive(Debug, Clone, thiserror::Error)]
pub enum JevError {
    /// The question could never have been asked: a name missing, a rubric with no levels, a state
    /// that is not text or an object. Never retried, because a second identical attempt fails
    /// identically.
    #[error("that question could not be put to Jev: {0}")]
    Asked(String),
    #[error("Jev is unreachable: {0}")]
    Unreachable(String),
    #[error("Jev did not answer within {:.1}s", .0.as_secs_f64())]
    TimedOut(std::time::Duration),
    #[error("Jev refused: {status} {message}")]
    Refused { status: u16, message: String },
}

/// A way to reach Jev. A trait for the same reason `ModelDoor` is one: a test hands the server a
/// door that answers from a script, and the endpoint, the routes and everything downstream cannot
/// tell the difference — which is what lets the suite run with no key, no spend and no network.
#[async_trait::async_trait]
pub trait JevDoor: Send + Sync {
    async fn ask(&self, ask: Ask) -> Result<Judgement, JevError>;
}

/// A door shared by the whole process, the way the model door is.
pub type SharedJev = Arc<dyn JevDoor>;
