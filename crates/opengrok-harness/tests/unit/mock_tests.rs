use super::*;
use futures::StreamExt;

fn request(text: &str) -> ModelRequest {
    ModelRequest {
        model: "mock".to_string(),
        messages: vec![crate::model::ChatMessage::text("user", text.to_string())],
        ..ModelRequest::default()
    }
}

/// Pacing is real, and off unless asked for.
///
/// Asserts a FLOOR on elapsed time, never a ceiling: a sleep may overrun on a loaded machine
/// but cannot fire early, so this cannot flake the way a "finishes within N ms" assertion
/// would. The unpaced half is the half that matters for the suite — every other test in the
/// workspace depends on the default door being instant.
#[tokio::test]
async fn a_paced_door_takes_time_and_an_unpaced_one_does_not() {
    let script = vec![
        ModelDelta::Text("one ".to_string()),
        ModelDelta::Text("two ".to_string()),
        ModelDelta::Text("three".to_string()),
    ];

    let started = std::time::Instant::now();
    let paced: Vec<_> = MockDoor::with_script(script.clone())
        .paced_by_ms(20)
        .stream(request("hi"))
        .await
        .expect("stream")
        .collect()
        .await;
    let elapsed = started.elapsed();

    assert_eq!(paced.len(), 3, "pacing must not change what is emitted");
    assert!(
        elapsed >= std::time::Duration::from_millis(60),
        "three deltas at 20ms each cannot arrive in {elapsed:?}"
    );

    // Zero is an explicit off, not a zero-length sleep: an operator turning the pacing off by
    // setting the variable to 0 must get exactly the unset behaviour.
    let started = std::time::Instant::now();
    let instant: Vec<_> = MockDoor::with_script(script)
        .paced_by_ms(0)
        .stream(request("hi"))
        .await
        .expect("stream")
        .collect()
        .await;
    assert_eq!(instant.len(), 3);
    assert!(
        started.elapsed() < std::time::Duration::from_millis(50),
        "an unpaced door must stay instant — the whole suite depends on it"
    );
}

/// The floor holds a one-delta answer open — which per-delta pacing never could.
#[tokio::test]
async fn a_floored_door_holds_even_a_one_delta_answer_open() {
    let one = vec![ModelDelta::Text("done".to_string())];

    let started = std::time::Instant::now();
    let out: Vec<_> = MockDoor::with_script(one.clone())
        .min_turn_ms(80)
        .stream(request("hi"))
        .await
        .expect("stream")
        .collect()
        .await;
    assert_eq!(out.len(), 1, "the floor must not change what is emitted");
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(80),
        "a single delta cannot arrive before the floor: {:?}",
        started.elapsed()
    );

    let started = std::time::Instant::now();
    let out: Vec<_> = MockDoor::with_script(one)
        .min_turn_ms(0)
        .stream(request("hi"))
        .await
        .expect("stream")
        .collect()
        .await;
    assert_eq!(out.len(), 1);
    assert!(
        started.elapsed() < std::time::Duration::from_millis(50),
        "zero is off, exactly as unset — the suite depends on it"
    );
}

/// The pause the ceiling produces, checked as arithmetic rather than by the clock.
///
/// The earlier version of this test asserted `elapsed < 4s` for 200 deltas. That is a CEILING
/// on elapsed time, which the test above forbids in as many words — a sleep can overrun on a
/// loaded machine but never fire early, so such an assertion can only fail spuriously. It was
/// also too loose to be worth the risk: with 0.4 s expected and 10 s unfixed, a 4 s threshold
/// caught only the ceiling being ignored ENTIRELY, and would have passed a regression that
/// divided by `len/2`, or used `max` instead of `min`, or overshot fivefold.
///
/// The arithmetic is exact and has no clock in it, so it can assert the precise value.
#[test]
fn the_pause_is_the_pacing_reduced_to_fit_the_ceiling() {
    let ms = std::time::Duration::from_millis;

    // The case this exists for: 200 deltas that would take 10 s, held to 400 ms.
    assert_eq!(paced_pause(Some(ms(50)), Some(ms(400)), 200), Some(ms(2)));

    // A SHORT ANSWER KEEPS ITS PACING. `min` picks the pause because the share is bigger —
    // and this is the assertion that distinguishes the fix from its inverse: with `max` the
    // answer would be 2 s, and with the ceiling ignored it would still be 50 ms, so only an
    // exact check tells the three apart.
    assert_eq!(paced_pause(Some(ms(50)), Some(ms(4_000)), 2), Some(ms(50)));

    // Each knob alone behaves as it did before the other existed.
    assert_eq!(paced_pause(Some(ms(40)), None, 4), Some(ms(40)));
    assert_eq!(paced_pause(None, Some(ms(400)), 4), None);
    assert_eq!(paced_pause(None, None, 4), None);

    // An empty script is REACHABLE — a door can answer with no deltas — and `Duration / 0`
    // panics, which is denied workspace-wide.
    assert_eq!(paced_pause(Some(ms(40)), Some(ms(400)), 0), Some(ms(40)));

    // A ceiling BELOW the pacing hurries everything, including a one-liner. Honest arithmetic
    // rather than a promise: an operator who tightens the ceiling to fix long answers does
    // reach the short ones, and this pins it so nobody has to rediscover it.
    assert_eq!(paced_pause(Some(ms(90)), Some(ms(50)), 1), Some(ms(50)));
}

/// The ceiling still bounds a real stream, and a short one still takes its time.
///
/// Only FLOORS on elapsed time here, per this file's convention: the long case asserts the
/// deltas all arrived (which the arithmetic test cannot observe), and the short case asserts
/// it was not hurried. Neither can fail on a slow machine.
#[tokio::test]
async fn the_ceiling_leaves_a_short_answer_paced() {
    let long: Vec<_> = (0..200)
        .map(|i| ModelDelta::Text(format!("w{i} ")))
        .collect();
    let out: Vec<_> = MockDoor::with_script(long)
        .paced_by_ms(50)
        .max_turn_ms(400)
        .stream(request("hi"))
        .await
        .expect("stream")
        .collect()
        .await;
    assert_eq!(
        out.len(),
        200,
        "the ceiling must not change what is emitted"
    );

    let started = std::time::Instant::now();
    let out: Vec<_> = MockDoor::with_script(vec![
        ModelDelta::Text("one ".to_string()),
        ModelDelta::Text("two".to_string()),
    ])
    .paced_by_ms(50)
    .max_turn_ms(4_000)
    .stream(request("hi"))
    .await
    .expect("stream")
    .collect()
    .await;
    assert_eq!(out.len(), 2);
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(100),
        "a short answer under a generous ceiling still paces: {:?}",
        started.elapsed()
    );
}

/// The shipped configuration: a floor AND a ceiling, which is what `serve.sh` sets and the
/// only combination a dev server ever runs — and which no test covered.
#[tokio::test]
async fn a_floored_and_capped_call_pays_the_floor_on_top_of_the_capped_pacing() {
    let script: Vec<_> = (0..100)
        .map(|i| ModelDelta::Text(format!("w{i} ")))
        .collect();
    let started = std::time::Instant::now();
    let out: Vec<_> = MockDoor::with_script(script)
        .paced_by_ms(50)
        .max_turn_ms(200)
        .min_turn_ms(150)
        .stream(request("hi"))
        .await
        .expect("stream")
        .collect()
        .await;
    assert_eq!(out.len(), 100);
    // THE FLOOR IS PAID ON TOP OF THE CEILING, not inside it — the knob caps the pacing, not
    // the call. A floor assertion, so a slow machine cannot fail it.
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(150),
        "the floor is still paid when a ceiling is set: {:?}",
        started.elapsed()
    );
}

/// The judge is never floored, however high the floor is set.
///
/// It is a second model call per tool call and it is invisible in the transcript, so flooring
/// it multiplies a person's wait by something they cannot see. Without this exclusion a
/// fixture turn — ask the tool, judge, say it back — pays the floor three times.
#[tokio::test]
async fn the_auto_review_judge_is_never_floored() {
    let door = MockDoor::echoing()
        .with_judge_verdict("allow")
        .min_turn_ms(3_000);

    let mut judged = request("anything");
    judged.system = Some(format!(
        "{}\nrest of the prompt",
        crate::review::JUDGE_MARKER
    ));

    let started = std::time::Instant::now();
    let out: Vec<_> = door.stream(judged).await.expect("stream").collect().await;
    assert_eq!(out.len(), 1, "the judge answers in one word");
    assert!(
        started.elapsed() < std::time::Duration::from_millis(500),
        "the judge must not pay the floor: took {:?}",
        started.elapsed()
    );

    // And an ordinary call on the same door still does.
    let started = std::time::Instant::now();
    let _ = MockDoor::with_script(vec![ModelDelta::Text("hi".to_string())])
        .min_turn_ms(120)
        .stream(request("hi"))
        .await
        .expect("stream")
        .collect::<Vec<_>>()
        .await;
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(120),
        "a visible call still pays it"
    );
}

/// A judge request is exempt from the floor WHETHER OR NOT a verdict is canned.
///
/// The first version keyed the exemption on the `judge_verdict` branch — so with no
/// `OG_AUTO_REVIEW_MOCK_VERDICT` (the dev server's actual state) a judge request fell through
/// to the echo branch and paid the floor after all. The exemption was dead on the
/// one server it was measured on, and the shorter "Working" window seen there came from the
/// lowered default alone. The exemption has to read the REQUEST, not the configuration.
#[tokio::test]
async fn a_judge_request_is_never_floored_even_without_a_canned_verdict() {
    let door = MockDoor::echoing().min_turn_ms(3_000); // no with_judge_verdict on purpose
    let mut judged = request("anything");
    judged.system = Some(format!("{}\nrest", crate::review::JUDGE_MARKER));

    let started = std::time::Instant::now();
    let out: Vec<_> = door.stream(judged).await.expect("stream").collect().await;
    assert!(!out.is_empty(), "the echo branch still answers");
    assert!(
        started.elapsed() < std::time::Duration::from_millis(500),
        "a judge request must not pay the floor, canned verdict or not: took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn the_default_door_streams_more_than_one_frame() {
    let door = MockDoor::echoing();
    let deltas: Vec<_> = door.stream(request("hello")).await.unwrap().collect().await;
    assert!(deltas.len() > 1, "a single frame would not test streaming");
    let text: String = deltas
        .into_iter()
        .filter_map(|delta| match delta {
            Ok(ModelDelta::Text(text)) => Some(text),
            _ => None,
        })
        .collect();
    assert!(text.contains("hello"), "{text}");
}

#[tokio::test]
async fn a_scripted_door_replays_exactly_what_it_was_given() {
    let script = vec![
        ModelDelta::ToolCallStart {
            id: "c1".to_string(),
            name: "shell".to_string(),
        },
        ModelDelta::ToolCallEnd {
            id: "c1".to_string(),
        },
    ];
    let door = MockDoor::with_script(script.clone());
    let deltas: Vec<_> = door.stream(request("x")).await.unwrap().collect().await;
    let got: Vec<_> = deltas.into_iter().map(|delta| delta.unwrap()).collect();
    assert_eq!(got, script);
}

#[tokio::test]
async fn a_failing_door_yields_an_error_the_harness_must_handle() {
    let door = MockDoor::failing_with("upstream hung up");
    let deltas: Vec<_> = door.stream(request("x")).await.unwrap().collect().await;
    assert_eq!(deltas.len(), 1);
    assert!(deltas[0].is_err());
}
