//! `POST /triage`: a fault the person is about to report, weighed by a model before anything is
//! filed. The desktop app sends a report it has already redacted (its `src/report` module) and
//! what else failed around it; one plain completion, with no tools, says whether it looks like
//! the project's bug, the person's own to fix, or noise, and for a bug writes the issue's title
//! and summary from the evidence it was given.
//!
//! The app's first gate has already sent away the plain cases (offline, signed out, refusals):
//! this is the second, for what is left. Anything short of a confident, well-formed answer is
//! said so, and the app falls back to the person filling the report by hand. A verdict is advice;
//! nothing here files anything.

use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::agui::AgUiState;
use crate::agui::routes::account_from_bearer;

/// The model a triage runs on when the app names none: the gateway's cheapest route. A triage
/// is a short judgement, not a turn, and the person pays for it on every Report they press.
pub const DEFAULT_MODEL: &str = "oag/cheap";

/// The longest a triage may take before the app is told to fall back to the manual report.
const TRIAGE_TIMEOUT: Duration = Duration::from_secs(15);

/// The most tokens a verdict may use: room for a title, a summary and a few lines of evidence.
const VERDICT_TOKENS: u32 = 900;

/// The largest context pack accepted, in bytes of its JSON. A redacted report is about one
/// kilobyte; anything near this is not a report.
const MOST_PACK_BYTES: usize = 16 * 1024;

/// What the app sends. Every string in it was redacted on the Mac before it was sent.
///
/// Wire shape agreed with the desktop app (hexuria/opengrok `src/report/mod.rs`, `Report`,
/// schema 1), which transcribes this file for the request and the [`Verdict`].
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextPack {
    /// The report as the app's preview shows it (`Report`, schema 1).
    pub report: serde_json::Value,
    /// What else failed around it on the Mac: `{place, status, secondsApart}`.
    #[serde(default)]
    pub neighbours: Vec<serde_json::Value>,
    /// The model to triage on; [`DEFAULT_MODEL`] when absent.
    #[serde(default)]
    pub model: Option<String>,
}

/// What a triage concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Call {
    /// Looks like the project's bug: worth an issue.
    Bug,
    /// The person's own to fix: `advice` says what to do.
    YourSide,
    /// Neither: a blip, an expected answer.
    Noise,
    /// The model could not tell. The app treats this as "fill it in yourself".
    Unsure,
}

/// The answer `POST /triage` gives, every field bounded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Verdict {
    pub verdict: Call,
    /// 0 to 1: how sure the model says it is. The app writes the issue from the verdict only at
    /// 0.8 and above.
    pub confidence: f64,
    /// The issue's title, for a bug.
    #[serde(default)]
    pub title: String,
    /// What happened, in plain words, for a bug.
    #[serde(default)]
    pub summary: String,
    /// The facts the verdict rests on, from the pack.
    #[serde(default)]
    pub evidence: Vec<String>,
    /// Where in the code it most likely is, from the pack's `raisedAt` and request.
    #[serde(default)]
    pub suspect: String,
    /// How to make it happen again, as steps.
    #[serde(default)]
    pub repro: Vec<String>,
    /// What the person can do, for their side.
    #[serde(default)]
    pub advice: String,
}

const TITLE_CHARS: usize = 120;
const SUMMARY_CHARS: usize = 1200;
const LINE_CHARS: usize = 200;
const ADVICE_CHARS: usize = 300;
const MOST_LINES: usize = 6;

fn cut(text: &str, most: usize) -> String {
    text.trim().chars().take(most).collect()
}

impl Verdict {
    /// The verdict with every field held to its bounds, and a confidence outside 0 to 1 read as
    /// no confidence: a model that says 7 is not seven times sure.
    fn clamped(self) -> Self {
        let lines = |lines: Vec<String>| -> Vec<String> {
            lines
                .iter()
                .map(|line| cut(line, LINE_CHARS))
                .filter(|line| !line.is_empty())
                .take(MOST_LINES)
                .collect()
        };
        Self {
            verdict: self.verdict,
            confidence: if (0.0..=1.0).contains(&self.confidence) {
                self.confidence
            } else {
                0.0
            },
            title: cut(&self.title, TITLE_CHARS),
            summary: cut(&self.summary, SUMMARY_CHARS),
            evidence: lines(self.evidence),
            suspect: cut(&self.suspect, LINE_CHARS),
            repro: lines(self.repro),
            advice: cut(&self.advice, ADVICE_CHARS),
        }
    }
}

/// The instructions a triage runs under.
pub const SYSTEM: &str = "You triage a fault from the OpenGrok desktop app before it is filed as \
a GitHub issue. You get a JSON report the app already redacted ({host}, {id}, {name}, <secret> \
are placeholders) and what else failed near it. Decide one of: \"bug\" (the app or the server \
misbehaved: a 5xx, an answer that could not be read, a crash, a request that should have worked), \
\"your_side\" (the person can fix it: their network, their own server down, a setting), \"noise\" \
(expected or harmless), \"unsure\". Use only the evidence given; never invent file names, \
functions or causes. Answer with ONE JSON object and nothing else: {\"verdict\":..., \
\"confidence\": 0..1, \"title\": short issue title for a bug, \"summary\": what happened in plain \
words, \"evidence\": [facts from the input], \"suspect\": the code location given in raisedAt if \
it is relevant, \"repro\": [steps, if the input shows them], \"advice\": what the person can do, \
for your_side}.";

/// The verdict in a model's answer: the first JSON object in it, read strictly. A model that
/// wraps it in prose or a code fence is fine; one that answers anything else is unread.
pub fn read_verdict(answer: &str) -> Option<Verdict> {
    let start = answer.find('{')?;
    let end = answer.rfind('}')?;
    let object = answer.get(start..=end)?;
    serde_json::from_str::<Verdict>(object)
        .ok()
        .map(Verdict::clamped)
}

/// `POST /triage`.
pub async fn triage_fault(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Json(pack): Json<ContextPack>,
) -> Response {
    let Some(account) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let Some(catalogue) = state.auth.model_catalogue.clone() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "this deployment has no gateway configured, so a fault cannot be triaged here",
        )
            .into_response();
    };
    let user = serde_json::json!({ "report": pack.report, "neighbours": pack.neighbours });
    let user = user.to_string();
    if user.len() > MOST_PACK_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            "a report is about a kilobyte; this is not one",
        )
            .into_response();
    }
    if !catalogue.may_triage(account.as_str()) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "wait a moment before triaging another fault",
        )
            .into_response();
    }
    let model = pack
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or(DEFAULT_MODEL);
    let answer = match catalogue
        .complete(model, SYSTEM, &user, VERDICT_TOKENS, TRIAGE_TIMEOUT)
        .await
    {
        Ok(answer) => answer,
        // The gateway's own words, already scrubbed: the app shows none of it to the person, it
        // only falls back to the manual report.
        Err(detail) => return (StatusCode::BAD_GATEWAY, detail).into_response(),
    };
    match read_verdict(&answer) {
        Some(verdict) => Json(verdict).into_response(),
        None => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "the model did not answer in the shape asked for",
        )
            .into_response(),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    /// A verdict is read from the first JSON object in the answer, fenced or not, and every field
    /// is held to its bounds; anything else is unread.
    #[test]
    fn a_verdict_is_read_strictly_and_bounded() {
        let fenced = "Here you go:\n```json\n{\"verdict\":\"bug\",\"confidence\":0.9,\"title\":\"Usage fails on 502\",\"evidence\":[\"status 502\"]}\n```";
        let verdict = read_verdict(fenced).expect("a fenced verdict reads");
        assert_eq!(verdict.verdict, Call::Bug);
        assert_eq!(verdict.title, "Usage fails on 502");
        assert_eq!(verdict.evidence, vec!["status 502".to_string()]);

        let long = format!(
            "{{\"verdict\":\"your_side\",\"confidence\":7,\"advice\":\"{}\",\"repro\":[{}]}}",
            "x".repeat(1000),
            vec!["\"step\""; 20].join(",")
        );
        let verdict = read_verdict(&long).expect("a long verdict reads");
        assert_eq!(verdict.confidence, 0.0, "a confidence outside 0..1 is none");
        assert_eq!(verdict.advice.chars().count(), ADVICE_CHARS);
        assert_eq!(verdict.repro.len(), MOST_LINES);

        for unread in ["I think it's a bug.", "{\"verdict\":\"maybe\"}", "{}", ""] {
            assert_eq!(read_verdict(unread), None, "{unread:?}");
        }
    }
}
