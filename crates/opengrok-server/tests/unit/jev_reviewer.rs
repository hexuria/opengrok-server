#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::jev::MockJev;
use cred_swap_core::detect::candidates::{Survey, candidates};
use cred_swap_core::detect::merge;
use cred_swap_core::{Cloak, Decision, Policy, Style, Surrogates};
use typesafe_sdk::{Answer, ChoiceAnswer};

/// How sure Jev has to be before a span is masked. Kept beside the only callers there are: no
/// route reviews with `review` yet, and its caller will choose its own.
///
/// The default leans towards masking: a stand-in that turned out to be
/// unnecessary costs a slightly odd-looking word in the prompt, and a miss
/// costs the thing this exists to prevent. Raise it when a coworker's work is
/// mostly code, where capitalised words are types rather than people.
const DEFAULT_THRESHOLD: f64 = 0.55;

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
