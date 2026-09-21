//! Asking Jev about what the rules could not decide.
//!
//! `cred-swap` recognises things with a defined shape: keys, cards, connection
//! strings, taxpayer numbers. It is blind to a colleague's name in the middle
//! of a sentence, because no pattern tells `Avery Sinclair` from `Bond Street`
//! from `Redis Cluster` without knowing what the sentence is about.
//!
//! Jev can tell them apart and cannot find them: it answers questions, it does
//! not return offsets. So the halves fit together the other way round from how
//! it first looks. `cred-swap` does the finding, over-generously — every run of
//! capitalised words, every value after a label, every long opaque token — and
//! Jev does the deciding, one calibrated probability per span.
//!
//! # Why a classifier rather than a chat call
//!
//! The same question put through the chat door comes back as prose that has to
//! be parsed into a decision, which is the guessing this is meant to remove. A
//! `Choice` answer arrives typed, with a probability against every label, so
//! "mask it" is a threshold rather than an interpretation. That is also what
//! makes the threshold arguable: it is a number in a config, and moving it
//! moves a measurable rate.
//!
//! # No fallback lives here
//!
//! When Jev cannot answer, this returns the refusal and no verdicts, matching
//! the rule the rest of this module keeps. The caller decides what that means.
//! For scrubbing the safe reading is usually "send only what the rules
//! already masked, and say so", but a classifier that silently degraded to
//! "nothing else looked sensitive" would make an outage and a clean bill of
//! health the same event in the transcript.

use std::collections::BTreeMap;

use cred_swap_core::detect::candidates::{Candidate, Shape, Surveyed};
use cred_swap_core::{EntityKind, Finding};
use typesafe_sdk::{JsonContent, Question};

use super::{Ask, JevDoor, JevError, Judgement};

/// How sure Jev has to be before a span is masked.
///
/// The default leans towards masking: a stand-in that turned out to be
/// unnecessary costs a slightly odd-looking word in the prompt, and a miss
/// costs the thing this exists to prevent. Raise it when a coworker's work is
/// mostly code, where capitalised words are types rather than people.
pub const DEFAULT_THRESHOLD: f64 = 0.55;

/// The labels Jev picks between, and what each one means for a span.
const LABELS: &[(&str, &str)] = &[
    (
        "person",
        "The name of a real individual: a colleague, a customer, an author of a message.",
    ),
    (
        "organisation",
        "The name of a real company, team, customer or institution.",
    ),
    (
        "place",
        "A real physical location: a street, a building, a city where someone can be found.",
    ),
    (
        "secret",
        "A credential, key, token, password or other value that grants access to something.",
    ),
    (
        "account",
        "An identifier for a real account or record: a customer number, a case number, an internal reference.",
    ),
    (
        "technical",
        "A name from software or infrastructure rather than from life: a product, a library, a type, a service, a branch, a build id, a hash.",
    ),
    (
        "ordinary",
        "An ordinary word, a heading, or a value that identifies nobody and grants nothing.",
    ),
];

/// What a verdict means for the span it was about.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    /// The label Jev chose.
    pub label: String,
    /// How sure it was of that label, from 0.0 to 1.0.
    pub confidence: f64,
    /// The kind this becomes if it is masked. `None` when the label is one
    /// that means "leave it alone".
    pub kind: Option<EntityKind>,
}

impl Verdict {
    /// Whether this span should be masked at `threshold`.
    #[must_use]
    pub fn masks(&self, threshold: f64) -> bool {
        self.kind.is_some() && self.confidence >= threshold
    }
}

/// What one review produced.
#[derive(Debug, Clone)]
pub struct Review {
    /// Findings to fold in beside the rules', already filtered by threshold.
    pub findings: Vec<Finding>,
    /// Every verdict, including the ones that did not reach the threshold.
    ///
    /// Kept so a caller can log what was nearly masked. A span that sat just
    /// under the line is the most useful thing to look at when tuning it.
    pub verdicts: Vec<(Candidate, Verdict)>,
    /// What the call cost and which model answered.
    pub judgement_model: String,
    /// The request id, when Jev sent one.
    pub request_id: Option<String>,
    /// Distinct values the survey's budget could not afford to ask about.
    ///
    /// Carried through rather than swallowed. A caller that reports "nothing
    /// else looked sensitive" while this is non-zero is reporting that it
    /// stopped looking, which is the failure this module's no-fallback rule
    /// exists to prevent.
    pub unexamined: usize,
}

/// Ask Jev to judge a set of candidates.
///
/// Returns findings for the spans that reached `threshold`, ready for
/// `cred_swap_core::detect::merge` and then `Cloak::scrub_findings`.
///
/// One call, one question per candidate, so the cost is bounded by whatever
/// limit the survey was given rather than by the length of the text.
///
/// # Errors
///
/// Returns the refusal Jev gave. Nothing is masked on the strength of a failed
/// call, and nothing is quietly let through either: the caller is told.
pub async fn review(
    jev: &dyn JevDoor,
    text: &str,
    surveyed: Surveyed,
    threshold: f64,
) -> Result<Review, JevError> {
    let unexamined = surveyed.dropped;
    let candidates = surveyed.candidates;

    if candidates.is_empty() {
        return Ok(Review {
            findings: Vec::new(),
            verdicts: Vec::new(),
            judgement_model: String::new(),
            request_id: None,
            unexamined,
        });
    }

    // One question per distinct value, not per occurrence. The same name five
    // times is one judgement, and the verdict applies to all five; asking five
    // times would cost five times as much to learn the same thing.
    let distinct = distinct_values(&candidates);

    let ask = Ask {
        state: state_for(text, &distinct),
        questions: distinct
            .iter()
            .enumerate()
            .map(|(index, candidate)| (question_name(index), question_for(candidate)))
            .collect(),
        model: None,
    };

    let judgement = jev.ask(ask).await?;
    Ok(collect(
        candidates, &distinct, &judgement, threshold, unexamined,
    ))
}

/// One representative candidate per distinct span text, in document order.
fn distinct_values(candidates: &[Candidate]) -> Vec<Candidate> {
    let mut seen: BTreeMap<&str, ()> = BTreeMap::new();
    let mut out = Vec::new();
    for candidate in candidates {
        if seen.insert(candidate.text.as_str(), ()).is_none() {
            out.push(candidate.clone());
        }
    }
    out
}

/// The state Jev judges: the message, and the spans in question.
///
/// The whole message goes in rather than each span alone, because the answer
/// depends on it. `Sinclair` in a sentence about a person and `Sinclair` in a
/// sentence about a repository are different answers to the same question.
fn state_for(text: &str, candidates: &[Candidate]) -> JsonContent {
    let spans: Vec<serde_json::Value> = candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| {
            serde_json::json!({
                "id": question_name(index),
                "span": candidate.text,
                "around": candidate.context,
                "shape": candidate.shape.as_str(),
            })
        })
        .collect();

    // Built as an Object directly rather than through `from_value`, which
    // returns a Result for the cases a literal object cannot be anyway.
    let mut state = serde_json::Map::new();
    state.insert(
        "message".to_string(),
        serde_json::Value::String(truncate(text, 8_000)),
    );
    state.insert("spans".to_string(), serde_json::Value::Array(spans));
    JsonContent::Object(state)
}

fn question_name(index: usize) -> String {
    format!("span_{index}")
}

fn question_for(candidate: &Candidate) -> Question {
    let hint = match candidate.shape {
        Shape::ProperNoun => {
            "It is a run of capitalised words, which is as often a product or a type as a person."
        }
        Shape::LabelledValue => {
            "It is the value after a label. The label is the strongest clue to what it is."
        }
        Shape::OpaqueToken => {
            "It is a long opaque run. A key and a build hash look identical; the surrounding words decide."
        }
        Shape::Numeric => "It is a long run of digits that no checksum claimed.",
    };

    Question::Choice {
        instructions: Some(JsonContent::String(format!(
            "The span `{}` appears in the message, in the context given under `around`. \
             What is it? {hint} Judge what the span refers to in this message, not what \
             the words could mean elsewhere.",
            candidate.text
        ))),
        criteria: LABELS
            .iter()
            .map(|(label, meaning)| {
                (
                    (*label).to_string(),
                    Some(JsonContent::String((*meaning).to_string())),
                )
            })
            .collect(),
    }
}

/// Turn the answers back into findings, one per occurrence.
///
/// Jev was asked about distinct values; the verdict is then applied to every
/// place that value appears, so masking one mention of a name masks all of
/// them. Leaving the others would defeat the point: the reader learns the name
/// from the occurrence that was missed.
fn collect(
    candidates: Vec<Candidate>,
    distinct: &[Candidate],
    judgement: &Judgement,
    threshold: f64,
    unexamined: usize,
) -> Review {
    let answers: BTreeMap<&str, &typesafe_sdk::Answer> = judgement
        .answers
        .iter()
        .map(|(name, answer)| (name.as_str(), answer))
        .collect();

    let mut by_value: BTreeMap<&str, Verdict> = BTreeMap::new();
    for (index, candidate) in distinct.iter().enumerate() {
        let Some(choice) = answers
            .get(question_name(index).as_str())
            .and_then(|answer| answer.as_choice())
        else {
            continue;
        };
        by_value.insert(
            candidate.text.as_str(),
            Verdict {
                label: choice.choice.clone(),
                confidence: choice.confidence,
                kind: kind_for(&choice.choice),
            },
        );
    }

    let mut findings = Vec::new();
    let mut verdicts = Vec::new();

    for candidate in candidates {
        let Some(verdict) = by_value.get(candidate.text.as_str()).cloned() else {
            continue;
        };
        if verdict.masks(threshold)
            && let Some(kind) = verdict.kind.clone()
        {
            findings.push(Finding {
                kind,
                start: candidate.start,
                end: candidate.end,
                text: candidate.text.clone(),
            });
        }
        verdicts.push((candidate, verdict));
    }

    Review {
        findings,
        verdicts,
        judgement_model: judgement.model.clone(),
        request_id: judgement.request_id.clone(),
        unexamined,
    }
}

/// What a label becomes when it is masked.
///
/// `technical` and `ordinary` map to nothing on purpose. Masking a library
/// name or a branch would make the answer worse without protecting anybody,
/// and an agent that cannot say `tokio` is not much use.
fn kind_for(label: &str) -> Option<EntityKind> {
    match label {
        "person" => Some(EntityKind::PersonName),
        "place" => Some(EntityKind::StreetAddress),
        "secret" => Some(EntityKind::GenericSecret),
        "organisation" => Some(EntityKind::Custom("organisation".to_string())),
        "account" => Some(EntityKind::Custom("account-reference".to_string())),
        _ => None,
    }
}

/// Cut a long message at a character boundary.
fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::jev::MockJev;
    use cred_swap_core::detect::candidates::{Survey, candidates};
    use cred_swap_core::detect::merge;
    use cred_swap_core::{Cloak, Decision, Policy, Style, Surrogates};
    use typesafe_sdk::{Answer, ChoiceAnswer};

    fn chose(label: &str, confidence: f64) -> Answer {
        // Collected into whatever map the field is, so the test does not have
        // to name a type the SDK does not re-export.
        Answer::Choice(ChoiceAnswer {
            choice: label.to_string(),
            confidence,
            probabilities: LABELS
                .iter()
                .map(|(name, _)| {
                    (
                        (*name).to_string(),
                        if *name == label { confidence } else { 0.0 },
                    )
                })
                .collect(),
        })
    }

    fn cloak() -> Cloak {
        Cloak::new(
            Policy::default(),
            Surrogates::from_secret(b"reviewer tests", Style::Realistic),
        )
        .unwrap()
    }

    /// The spans a message offers for judgement, in order.
    fn spans(text: &str) -> Surveyed {
        candidates(text, &cloak().inspect(text), &Survey::default())
    }

    #[tokio::test]
    async fn a_name_the_rules_miss_becomes_a_finding() {
        let text = "Avery Sinclair signed off on the migration.";
        let asking = spans(text);
        let index = asking
            .candidates
            .iter()
            .position(|c| c.text == "Avery Sinclair")
            .expect("the name is a candidate");

        let jev = MockJev::answering(
            asking
                .candidates
                .iter()
                .enumerate()
                .map(|(position, _)| {
                    (
                        question_name(position),
                        if position == index {
                            chose("person", 0.94)
                        } else {
                            chose("ordinary", 0.9)
                        },
                    )
                })
                .collect::<Vec<_>>(),
        );

        let review = review(&jev, text, asking, DEFAULT_THRESHOLD).await.unwrap();
        assert_eq!(review.findings.len(), 1);
        assert_eq!(review.findings[0].text, "Avery Sinclair");
        assert_eq!(review.findings[0].kind, EntityKind::PersonName);
    }

    #[tokio::test]
    async fn a_low_confidence_verdict_is_reported_but_not_acted_on() {
        let text = "Avery Sinclair signed off on the migration.";
        let asking = spans(text);
        let jev = MockJev::answering(
            asking
                .candidates
                .iter()
                .enumerate()
                .map(|(position, _)| (question_name(position), chose("person", 0.31)))
                .collect::<Vec<_>>(),
        );

        let review = review(&jev, text, asking, DEFAULT_THRESHOLD).await.unwrap();
        assert!(review.findings.is_empty(), "masked below the threshold");
        assert!(
            !review.verdicts.is_empty(),
            "a near miss should still be visible to whoever tunes the threshold"
        );
        assert_eq!(review.verdicts[0].1.label, "person");
    }

    #[tokio::test]
    async fn a_library_name_is_left_alone_however_sure_jev_is() {
        let text = "We swapped serde for Miniserde in the hot path.";
        let asking = spans(text);
        let jev = MockJev::answering(
            asking
                .candidates
                .iter()
                .enumerate()
                .map(|(position, _)| (question_name(position), chose("technical", 0.99)))
                .collect::<Vec<_>>(),
        );

        let review = review(&jev, text, asking, DEFAULT_THRESHOLD).await.unwrap();
        assert!(
            review.findings.is_empty(),
            "an agent that cannot say a library's name is not much use"
        );
    }

    #[tokio::test]
    async fn the_whole_loop_scrubs_and_restores() {
        let mut cloak = cloak();
        let text = "Avery Sinclair approved it; mail dana@corp.com to confirm.";

        let rules = cloak.inspect(text);
        assert_eq!(rules.len(), 1, "the rules find only the address");

        let asking = candidates(text, &rules, &Survey::default());
        let index = asking
            .candidates
            .iter()
            .position(|c| c.text == "Avery Sinclair")
            .expect("the name is a candidate");
        let jev = MockJev::answering(
            asking
                .candidates
                .iter()
                .enumerate()
                .map(|(position, _)| {
                    (
                        question_name(position),
                        if position == index {
                            chose("person", 0.92)
                        } else {
                            chose("ordinary", 0.88)
                        },
                    )
                })
                .collect::<Vec<_>>(),
        );

        let review = review(&jev, text, asking, DEFAULT_THRESHOLD).await.unwrap();
        let merged = merge(rules, review.findings);
        assert_eq!(
            merged.displaced, 0,
            "a verdict was displaced without the test noticing"
        );
        let scrubbed = cloak.scrub_findings(text, merged.findings, |_| Decision::Replace);

        assert!(
            !scrubbed.text.contains("Avery Sinclair"),
            "{}",
            scrubbed.text
        );
        assert!(
            !scrubbed.text.contains("dana@corp.com"),
            "{}",
            scrubbed.text
        );
        // The judged span restores exactly like a rule-found one, which is the
        // whole reason this returns findings rather than a redaction.
        assert_eq!(cloak.restore(&scrubbed.text), text);
    }

    #[tokio::test]
    async fn a_refusal_masks_nothing_and_says_so() {
        let text = "Avery Sinclair signed off.";
        let jev = MockJev::failing_with(JevError::Unreachable("no route".to_string()));

        let outcome = review(&jev, text, spans(text), DEFAULT_THRESHOLD).await;
        assert!(matches!(outcome, Err(JevError::Unreachable(_))));
    }

    #[tokio::test]
    async fn nothing_to_judge_costs_nothing() {
        let jev = MockJev::answering(Vec::<(String, Answer)>::new());
        let empty = Surveyed {
            candidates: Vec::new(),
            dropped: 0,
        };
        let review = review(&jev, "nothing here", empty, DEFAULT_THRESHOLD)
            .await
            .unwrap();
        assert!(review.findings.is_empty());
        assert!(
            jev.asked().is_empty(),
            "an empty survey should not call Jev"
        );
    }

    #[tokio::test]
    async fn one_question_per_value_and_a_verdict_for_every_occurrence() {
        let text = "Avery Sinclair wrote it. Avery Sinclair shipped it. Avery Sinclair broke it.";
        let asking = spans(text);
        let occurrences = asking
            .candidates
            .iter()
            .filter(|c| c.text == "Avery Sinclair")
            .count();
        assert_eq!(occurrences, 3, "every occurrence must reach the reviewer");

        let jev = MockJev::answering(vec![(question_name(0), chose("person", 0.95))]);
        let review = review(&jev, text, asking, DEFAULT_THRESHOLD).await.unwrap();

        assert_eq!(
            jev.asked()[0].questions.len(),
            1,
            "the same name was asked about more than once"
        );
        assert_eq!(
            review.findings.len(),
            3,
            "masking one mention and not the others teaches the reader the name"
        );
    }

    #[tokio::test]
    async fn an_unaffordable_survey_is_reported_not_swallowed() {
        let mut text = String::new();
        for name in ["Avery Ashford", "Rowan Barlow", "Quinn Cartwright"] {
            text.push_str(name);
            text.push_str(" approved it. ");
        }

        let asking = candidates(&text, &[], &Survey::default().limit(1));
        assert!(asking.truncated(), "the fixture did not truncate");

        let jev = MockJev::answering(vec![(question_name(0), chose("ordinary", 0.9))]);
        let review = review(&jev, &text, asking, DEFAULT_THRESHOLD)
            .await
            .unwrap();
        assert!(
            review.unexamined > 0,
            "a caller could not tell the survey stopped looking"
        );
    }

    #[tokio::test]
    async fn every_candidate_is_asked_about_with_its_context() {
        let text = "The change was approved by Avery Sinclair on Tuesday.";
        let asking = spans(text);
        let jev = MockJev::answering(
            asking
                .candidates
                .iter()
                .enumerate()
                .map(|(position, _)| (question_name(position), chose("ordinary", 0.9)))
                .collect::<Vec<_>>(),
        );

        let wanted: Vec<Candidate> = asking.candidates.clone();
        review(&jev, text, asking, DEFAULT_THRESHOLD).await.unwrap();

        let asked = jev.asked();
        assert_eq!(asked.len(), 1, "one call, however many spans");
        assert_eq!(asked[0].questions.len(), wanted.len());

        let rendered = serde_json::to_string(&asked[0].state).unwrap();
        for candidate in &wanted {
            assert!(
                rendered.contains(&candidate.text),
                "{} was not in the state",
                candidate.text
            );
        }
        assert!(rendered.contains("approved by"), "context was not sent");
    }
}
