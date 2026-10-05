//! Property tests for the Jev adapter: conversion exactness and monotonicity, the ranked list
//! and off-menu guard over random menus, equivariance under option relabeling, and no panics on
//! hostile bodies.
#![allow(clippy::float_arithmetic, clippy::unwrap_used, clippy::expect_used)]

use opengrok_jev::{ConvertError, ParseError, confidence_from_unit, parse_reply};
use proptest::prelude::*;
use pua_core::{Answer, Question};
use serde_json::{Map, Value, json};

fn body(labels: &[String], millis: &[u16], chosen: usize) -> String {
    let probs: Map<String, Value> = labels
        .iter()
        .zip(millis)
        .map(|(l, m)| (l.clone(), json!(f64::from(*m) / 1000.0)))
        .collect();
    json!({"model": "m", "usage": {}, "answers": [{"kind": "choice", "name": "q",
        "choice": labels[chosen], "confidence": f64::from(millis[chosen]) / 1000.0,
        "probabilities": probs}]})
    .to_string()
}

fn ranked_labels(q: &Question, a: &Answer) -> Vec<String> {
    let (Question::Choice { options, .. }, Answer::Choice { ranked, .. }) = (q, a) else {
        panic!("{a:?}")
    };
    ranked
        .entries()
        .iter()
        .map(|(i, _)| options.get(*i).unwrap().as_str().to_owned())
        .collect()
}

fn menu() -> impl Strategy<Value = (Vec<String>, Vec<u16>, usize)> {
    proptest::collection::btree_set("[a-z]{1,6}", 2..6).prop_flat_map(|set| {
        let labels: Vec<String> = set.into_iter().collect();
        let n = labels.len();
        (
            Just(labels),
            proptest::collection::vec(0u16..=1000, n),
            0..n,
        )
    })
}

#[test]
fn every_whole_millis_round_trips() {
    for k in 0..=1000u16 {
        let c = confidence_from_unit(f64::from(k) / 1000.0).unwrap();
        assert_eq!(c.get(), i16::try_from(k).unwrap(), "{k}");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn conversion_is_monotone(a in 0.0f64..=1.0, b in 0.0f64..=1.0) {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        prop_assert!(confidence_from_unit(lo).unwrap() <= confidence_from_unit(hi).unwrap());
    }

    #[test]
    fn outside_the_unit_interval_is_an_error(x in prop_oneof![-1e9f64..-1e-12, 1.000_000_001f64..1e9]) {
        prop_assert_eq!(confidence_from_unit(x), Err(ConvertError::OutOfRange));
    }

    #[test]
    fn ranked_list_is_sorted_millis_with_index_tiebreak((labels, millis, chosen) in menu()) {
        let q = Question::choice("q", &labels).unwrap();
        let a = parse_reply(&body(&labels, &millis, chosen), &q).unwrap();
        let Answer::Choice { option, confidence, ranked } = &a else { panic!() };
        prop_assert_eq!(usize::from(option.get()), chosen);
        prop_assert_eq!(confidence.get(), i16::try_from(millis[chosen]).unwrap());
        let mut expect: Vec<(usize, u16)> = millis.iter().copied().enumerate().collect();
        expect.sort_by(|x, y| y.1.cmp(&x.1).then(x.0.cmp(&y.0)));
        let got: Vec<(usize, u16)> = ranked
            .entries()
            .iter()
            .map(|(i, c)| (usize::from(i.get()), u16::try_from(c.get()).unwrap()))
            .collect();
        prop_assert_eq!(got, expect);
    }

    /// Relabel the menu (reverse its order, carrying each label's probability): the chosen
    /// label and, away from ties, the ranked label sequence are unchanged.
    #[test]
    fn equivariant_under_reordering_the_menu((labels, millis, chosen) in menu()) {
        let q = Question::choice("q", &labels).unwrap();
        let a = parse_reply(&body(&labels, &millis, chosen), &q).unwrap();
        let rl: Vec<String> = labels.iter().rev().cloned().collect();
        let rm: Vec<u16> = millis.iter().rev().copied().collect();
        let rq = Question::choice("q", &rl).unwrap();
        let ra = parse_reply(&body(&rl, &rm, labels.len() - 1 - chosen), &rq).unwrap();
        let mut sorted = millis.clone();
        sorted.sort_unstable();
        sorted.dedup();
        if sorted.len() == millis.len() {
            prop_assert_eq!(ranked_labels(&q, &a), ranked_labels(&rq, &ra));
        }
        let label = |q: &Question, a: &Answer| q.options().unwrap().get(a.chosen().unwrap()).unwrap().as_str().to_owned();
        prop_assert_eq!(label(&q, &a), label(&rq, &ra));
    }

    #[test]
    fn off_menu_labels_are_refused((labels, millis, chosen) in menu(), stray in "[A-Z0-9 ]{1,6}") {
        let q = Question::choice("q", &labels).unwrap();
        let mut b: Value = serde_json::from_str(&body(&labels, &millis, chosen)).unwrap();
        b["answers"][0]["choice"] = json!(stray.clone());
        prop_assert_eq!(parse_reply(&b.to_string(), &q), Err(ParseError::OffMenu(stray)));
    }

    #[test]
    fn hostile_bodies_never_panic(s in "\\PC{0,200}") {
        let q = Question::choice("q", &["a", "b"]).unwrap();
        let _ = parse_reply(&s, &q);
    }
}
