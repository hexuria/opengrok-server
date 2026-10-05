use pua_core::{Answer, Confidence, OptionIndex, Question, StageKind};
use serde_json::{Value, json};

use crate::*;

fn delivery() -> Question {
    Question::choice("delivery", &["queue", "steer", "interrupt"]).unwrap()
}

#[allow(clippy::needless_pass_by_value)] // call sites build the value inline
fn reply(answers: Value) -> String {
    json!({"model": "jev-1", "requestId": null, "usage": {"inputTokens": 10, "outputTokens": 2},
           "answers": answers})
    .to_string()
}

fn c(v: i16) -> Confidence {
    Confidence::new(v).unwrap()
}

#[test]
fn render_matches_the_route_shapes() {
    let ask = render_ask(
        json!({"asked": "stop the build"}),
        &[
            (
                &Question::noul("done").unwrap(),
                "Is what was asked now true?",
            ),
            (&delivery(), "  How should this message be delivered?  "),
            (
                &Question::score("effort", &["none", "a little", "most of it"]).unwrap(),
                "How much work is left?",
            ),
        ],
        Some("  "),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(&ask).unwrap(),
        json!({
            "state": {"asked": "stop the build"},
            "questions": [
                {"kind": "noul", "name": "done", "instructions": "Is what was asked now true?"},
                {"kind": "choice", "name": "delivery",
                 "instructions": "How should this message be delivered?",
                 "choices": ["queue", "steer", "interrupt"]},
                {"kind": "score", "name": "effort", "instructions": "How much work is left?",
                 "levels": ["none", "a little", "most of it"]}
            ]
        })
    );
    let with_model = render_ask(json!("text"), &[(&delivery(), "x")], Some(" jev-2 ")).unwrap();
    assert_eq!(with_model.model.as_deref(), Some("jev-2"));
}

#[test]
fn render_errors_are_exact() {
    let q = delivery();
    assert_eq!(
        render_question(&q, " \n "),
        Err(RenderError::EmptyInstructions("delivery".into()))
    );
    assert_eq!(
        render_ask(json!("s"), &[], None),
        Err(RenderError::NoQuestions)
    );
    assert_eq!(
        render_ask(json!("s"), &[(&q, "a"), (&q, "b")], None),
        Err(RenderError::DuplicateName("delivery".into()))
    );
    for bad in [json!(null), json!(1), json!(true)] {
        assert_eq!(
            render_ask(bad, &[(&q, "a")], None),
            Err(RenderError::BadState)
        );
    }
    assert_eq!(
        RenderError::DuplicateName("x".into()).to_string(),
        "two questions are both called \"x\""
    );
}

#[test]
fn parse_choice_builds_the_ranked_list() {
    let body = reply(
        json!([{"kind": "choice", "name": "delivery", "choice": "steer",
        "confidence": 0.71, "probabilities": {"queue": 0.2, "steer": 0.71, "interrupt": 0.09}}]),
    );
    let a = parse_reply(&body, &delivery()).unwrap();
    let Answer::Choice {
        option,
        confidence,
        ranked,
    } = a
    else {
        panic!("{a:?}")
    };
    assert_eq!((option, confidence), (OptionIndex::new(1), c(710)));
    assert_eq!(
        ranked.entries(),
        [
            (OptionIndex::new(1), c(710)),
            (OptionIndex::new(0), c(200)),
            (OptionIndex::new(2), c(90))
        ]
    );
}

#[test]
fn ties_in_probabilities_go_to_the_lower_option() {
    let body = reply(
        json!([{"kind": "choice", "name": "delivery", "choice": "queue",
        "confidence": 0.4, "probabilities": {"queue": 0.4, "steer": 0.4, "interrupt": 0.2}}]),
    );
    let Answer::Choice { ranked, .. } = parse_reply(&body, &delivery()).unwrap() else {
        panic!()
    };
    assert_eq!(ranked.entries()[0].0, OptionIndex::new(0));
    assert_eq!(ranked.entries()[1].0, OptionIndex::new(1));
}

#[test]
fn off_menu_guard() {
    let q = delivery();
    let choice = |label: &str, probs: Value| {
        reply(
            json!([{"kind": "choice", "name": "delivery", "choice": label,
            "confidence": 0.9, "probabilities": probs}]),
        )
    };
    let full = json!({"queue": 0.05, "steer": 0.05, "interrupt": 0.9});
    assert_eq!(
        parse_reply(&choice("halt", full.clone()), &q),
        Err(ParseError::OffMenu("halt".into()))
    );
    // Case and spacing are not folded: Jev must return the offered label exactly.
    assert_eq!(
        parse_reply(&choice("Interrupt", full), &q),
        Err(ParseError::OffMenu("Interrupt".into()))
    );
    assert_eq!(
        parse_reply(
            &choice(
                "interrupt",
                json!({"queue": 0.0, "steer": 0.0, "interrupt": 0.9, "halt": 0.1})
            ),
            &q
        ),
        Err(ParseError::OffMenu("halt".into()))
    );
    let score = Question::score("effort", &["none", "a little", "most of it"]).unwrap();
    let level = |level: Value| {
        reply(
            json!([{"kind": "score", "name": "effort", "score": 2.0, "level": level,
            "confidence": 0.6, "legend": {}, "probabilities": {}}]),
        )
    };
    assert_eq!(
        parse_reply(&level(json!("most of it")), &score),
        Ok(Answer::Score {
            level: OptionIndex::new(2),
            confidence: c(600)
        })
    );
    assert_eq!(
        parse_reply(&level(json!("all of it")), &score),
        Err(ParseError::OffMenu("all of it".into()))
    );
    assert_eq!(
        parse_reply(&level(json!(null)), &score),
        Err(ParseError::OffMenu("rung 2 (no level)".into()))
    );
    assert_eq!(
        parse_reply(&level(json!(3)), &score),
        Err(ParseError::OffMenu("3".into()))
    );
}

#[test]
fn a_missing_probability_is_never_zero() {
    let q = delivery();
    let probs = |p: Value| {
        reply(
            json!([{"kind": "choice", "name": "delivery", "choice": "queue",
            "confidence": 0.9, "probabilities": p}]),
        )
    };
    assert_eq!(
        parse_reply(&probs(json!({"queue": 0.9, "steer": 0.1})), &q),
        Err(ParseError::MissingProbability("interrupt".into()))
    );
    // `null` is how the route sends a NaN or infinite probability.
    assert_eq!(
        parse_reply(
            &probs(json!({"queue": 0.9, "steer": 0.1, "interrupt": null})),
            &q
        ),
        Err(ParseError::MissingProbability("interrupt".into()))
    );
}

#[test]
fn floats_out_of_range_are_errors() {
    let q = delivery();
    let body = reply(
        json!([{"kind": "choice", "name": "delivery", "choice": "queue",
        "confidence": 1.5, "probabilities": {"queue": 1.0, "steer": 0.0, "interrupt": 0.0}}]),
    );
    assert_eq!(
        parse_reply(&body, &q),
        Err(ParseError::Convert {
            field: "confidence",
            error: ConvertError::OutOfRange
        })
    );
    let body = reply(
        json!([{"kind": "choice", "name": "delivery", "choice": "queue",
        "confidence": 0.9, "probabilities": {"queue": -0.1, "steer": 0.0, "interrupt": 0.0}}]),
    );
    assert_eq!(
        parse_reply(&body, &q),
        Err(ParseError::Convert {
            field: "probabilities",
            error: ConvertError::OutOfRange
        })
    );
    let score = Question::score("effort", &["none", "some"]).unwrap();
    let answered = AnsweredQuestion::Score {
        name: "effort".into(),
        score: f64::NAN,
        level: Some(json!("some")),
        confidence: 0.5,
        legend: serde_json::Map::new(),
        probabilities: serde_json::Map::new(),
    };
    assert_eq!(
        parse_answer(&answered, &score),
        Err(ParseError::Convert {
            field: "score",
            error: ConvertError::NonFinite
        })
    );
}

#[test]
fn noul_reading_and_drift_check() {
    let q = Question::noul("done").unwrap();
    let noul = |yes: bool, conf: f64, p_yes: Value| {
        reply(
            json!([{"kind": "noul", "name": "done", "yes": yes, "confidence": conf,
            "probabilities": {"yes": p_yes, "no": 0.5}}]),
        )
    };
    assert_eq!(
        parse_reply(&noul(false, 0.98, json!(0.02)), &q),
        Ok(Answer::Noul {
            yes: false,
            confidence: c(980)
        })
    );
    // Exactly a half reads as yes (YES_ABOVE is inclusive, as in the route).
    assert_eq!(
        parse_reply(&noul(true, 0.5, json!(0.5)), &q),
        Ok(Answer::Noul {
            yes: true,
            confidence: c(500)
        })
    );
    // A confident no read as "low confidence yes" is the bug the route warns about.
    assert_eq!(
        parse_reply(&noul(true, 0.02, json!(0.02)), &q),
        Err(ParseError::InconsistentNoul)
    );
    assert_eq!(
        parse_reply(&noul(false, 0.8, json!(0.2)), &q),
        Ok(Answer::Noul {
            yes: false,
            confidence: c(800)
        })
    );
    assert_eq!(
        parse_reply(&noul(false, 0.7, json!(0.2)), &q),
        Err(ParseError::InconsistentNoul)
    );
    assert_eq!(
        parse_reply(&noul(true, 0.9, json!(null)), &q),
        Err(ParseError::MissingProbability("yes".into()))
    );
    assert_eq!(
        parse_reply(&noul(true, 0.9, json!(1.2)), &q),
        Err(ParseError::Convert {
            field: "probabilities.yes",
            error: ConvertError::OutOfRange
        })
    );
}

#[test]
fn shape_errors_are_exact() {
    let q = delivery();
    assert!(matches!(
        parse_reply("{", &q),
        Err(ParseError::Deserialize(_))
    ));
    assert!(matches!(
        parse_reply(&reply(json!([{"kind": "vote", "name": "delivery"}])), &q),
        Err(ParseError::Deserialize(_))
    ));
    assert_eq!(
        parse_reply(&reply(json!([])), &q),
        Err(ParseError::MissingAnswer("delivery".into()))
    );
    let noul_answer = json!([{"kind": "noul", "name": "delivery", "yes": true,
        "confidence": 0.9, "probabilities": {"yes": 0.9, "no": 0.1}}]);
    assert_eq!(
        parse_reply(&reply(noul_answer), &q),
        Err(ParseError::KindMismatch {
            expected: "choice",
            got: "noul"
        })
    );
    let score_answer = json!([{"kind": "score", "name": "done", "score": 0.0, "level": "x",
        "confidence": 0.9, "legend": {}, "probabilities": {}}]);
    assert_eq!(
        parse_reply(&reply(score_answer), &Question::noul("done").unwrap()),
        Err(ParseError::KindMismatch {
            expected: "noul",
            got: "score"
        })
    );
    let choice_answer = json!([{"kind": "choice", "name": "effort", "choice": "x",
        "confidence": 0.9, "probabilities": {}}]);
    assert_eq!(
        parse_reply(
            &reply(choice_answer),
            &Question::score("effort", &["a", "b"]).unwrap()
        ),
        Err(ParseError::KindMismatch {
            expected: "score",
            got: "choice"
        })
    );
    let other = AnsweredQuestion::Noul {
        name: "other".into(),
        yes: true,
        confidence: 0.9,
        probabilities: serde_json::Map::new(),
    };
    assert_eq!(
        parse_answer(&other, &Question::noul("done").unwrap()),
        Err(ParseError::NameMismatch {
            expected: "done".into(),
            got: "other".into()
        })
    );
}

#[test]
fn reply_picks_the_answer_by_name_in_any_order() {
    let body = reply(json!([
        {"kind": "noul", "name": "done", "yes": true, "confidence": 0.9,
         "probabilities": {"yes": 0.9, "no": 0.1}},
        {"kind": "choice", "name": "delivery", "choice": "interrupt", "confidence": 0.95,
         "probabilities": {"queue": 0.03, "steer": 0.02, "interrupt": 0.95}}
    ]));
    assert_eq!(
        parse_reply(&body, &delivery()).unwrap().chosen(),
        Some(OptionIndex::new(2))
    );
}

#[test]
fn escalation_labels_every_outcome() {
    let q = delivery();
    let ok = reply(
        json!([{"kind": "choice", "name": "delivery", "choice": "interrupt",
        "confidence": 0.95, "probabilities": {"queue": 0.03, "steer": 0.02, "interrupt": 0.95}}]),
    );
    let safe = Answer::Score {
        level: OptionIndex::SAFE_DEFAULT,
        confidence: Confidence::ZERO,
    };
    let fallback = |_: &FallbackWhy| safe.clone();

    let e = escalate(&q, Ok(&ok), |_| None, fallback);
    assert!(!e.is_fallback());
    assert_eq!(e.answer().chosen(), Some(OptionIndex::new(2)));
    assert_eq!(e.trail_record().stage(), StageKind::Escalation);
    assert_eq!(e.trail_record().text(), "jev answered");

    let vetoed = escalate(
        &q,
        Ok(&ok),
        |a| (a.chosen() == Some(OptionIndex::new(2))).then(|| "negated cue".to_owned()),
        fallback,
    );
    assert_eq!(
        vetoed,
        Escalation::Fallback {
            why: FallbackWhy::Vetoed("negated cue".into()),
            answer: safe.clone()
        }
    );
    assert_eq!(
        vetoed.trail_record().text(),
        "fallback: vetoed: negated cue"
    );

    let off = reply(
        json!([{"kind": "choice", "name": "delivery", "choice": "halt",
        "confidence": 0.9, "probabilities": {}}]),
    );
    assert_eq!(
        escalate(&q, Ok(&off), |_| None, fallback),
        Escalation::Fallback {
            why: FallbackWhy::Refused(ParseError::OffMenu("halt".into())),
            answer: safe.clone()
        }
    );

    let kinds = [
        JevError::Asked("no levels".into()),
        JevError::Unreachable("dns".into()),
        JevError::TimedOut { millis: 2500 },
        JevError::Refused {
            status: 429,
            message: "slow down".into(),
        },
    ];
    let mut texts = Vec::new();
    for k in kinds {
        let mut calls = 0;
        let e = escalate(
            &q,
            Err(k.clone()),
            |_| None,
            |why| {
                calls += 1;
                assert_eq!(why, &FallbackWhy::Jev(k.clone()));
                safe.clone()
            },
        );
        assert_eq!(calls, 1);
        assert!(e.is_fallback());
        assert_eq!(e.answer(), &safe);
        texts.push(e.trail_record().text().to_owned());
    }
    assert_eq!(
        texts,
        [
            "fallback: that question could not be put to Jev: no levels",
            "fallback: Jev is unreachable: dns",
            "fallback: Jev did not answer within 2500 ms",
            "fallback: Jev refused: 429 slow down",
        ]
    );
}

#[test]
fn error_texts_are_distinct() {
    let all = [
        ParseError::Deserialize("x".into()).to_string(),
        ParseError::MissingAnswer("x".into()).to_string(),
        ParseError::NameMismatch {
            expected: "a".into(),
            got: "b".into(),
        }
        .to_string(),
        ParseError::KindMismatch {
            expected: "noul",
            got: "score",
        }
        .to_string(),
        ParseError::OffMenu("x".into()).to_string(),
        ParseError::MissingProbability("x".into()).to_string(),
        ParseError::Convert {
            field: "f",
            error: ConvertError::NonFinite,
        }
        .to_string(),
        ParseError::InconsistentNoul.to_string(),
        RenderError::EmptyInstructions("x".into()).to_string(),
        RenderError::NoQuestions.to_string(),
        RenderError::BadState.to_string(),
        FallbackWhy::Refused(ParseError::InconsistentNoul).to_string(),
        ConvertError::OutOfRange.to_string(),
    ];
    let mut sorted = all.to_vec();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), all.len());
    assert_eq!(
        ParseError::KindMismatch {
            expected: "noul",
            got: "score"
        }
        .to_string(),
        "asked a noul question, got a score answer"
    );
}

#[test]
fn wire_choice_labels_and_convert_texts() {
    let bare: AskedChoice = serde_json::from_value(json!("queue")).unwrap();
    let described: AskedChoice =
        serde_json::from_value(json!({"label": "steer", "means": "nudge the run"})).unwrap();
    assert_eq!((bare.label(), described.label()), ("queue", "steer"));
    assert_eq!(
        ConvertError::NonFinite.to_string(),
        "probability is not finite"
    );
    assert_eq!(
        ConvertError::OutOfRange.to_string(),
        "probability is outside [0, 1]"
    );
}
