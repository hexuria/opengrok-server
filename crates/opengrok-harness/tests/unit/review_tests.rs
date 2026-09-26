use super::*;
use crate::mock::MockDoor;
use std::sync::Mutex;

#[test]
fn exactly_one_bare_word_parses() {
    for (text, verdict) in [
        ("allow", ReviewVerdict::Allow),
        ("ALLOW\n", ReviewVerdict::Allow),
        ("`block`", ReviewVerdict::Block),
        ("ask.", ReviewVerdict::Ask),
        ("  Block  ", ReviewVerdict::Block),
    ] {
        assert_eq!(parse_verdict(text), verdict, "{text:?}");
    }
    for text in [
        "",
        "I think allow",
        "allow block",
        "allowed",
        "allow, but carefully",
    ] {
        assert_eq!(
            parse_verdict(text),
            ReviewVerdict::Unavailable(JudgeFailure::Unparseable),
            "{text:?}"
        );
    }
}

fn ask<'a>() -> ReviewAsk<'a> {
    ReviewAsk {
        call_id: "call_1",
        tool: "shell",
        arguments: r#"{"command":"brew install jq"}"#,
        allow_instructions: "",
        block_instructions: "anything that installs software",
    }
}

#[tokio::test]
async fn a_failing_door_is_unavailable_never_allow() {
    let judge = ModelJudge::new(
        Arc::new(MockDoor::failing_with("upstream hung up")),
        "oag/cheap",
    );
    assert_eq!(
        judge.judge(ask()).await,
        ReviewVerdict::Unavailable(JudgeFailure::StreamBroke)
    );
}

/// A door that refuses before it streams, the way the gateway and the spend guard do.
struct RefusingDoor(fn() -> ModelError);
#[async_trait::async_trait]
impl ModelDoor for RefusingDoor {
    async fn stream(
        &self,
        _request: ModelRequest,
    ) -> Result<crate::model::DeltaStream, ModelError> {
        Err((self.0)())
    }
}

/// #201: each refusal keeps its cause, so the card can say which one it was. A spend cap
/// is the one a capped coworker hits on every call, since the judge bills its key.
#[tokio::test]
async fn a_refusing_door_keeps_its_cause() {
    let cases: [(fn() -> ModelError, JudgeFailure); 4] = [
        (
            || ModelError::SpendCap("Ada has reached her monthly limit".to_string()),
            JudgeFailure::SpendCap,
        ),
        (
            || ModelError::Refused {
                status: 404,
                body: "unknown model route".to_string(),
                retry_after_s: None,
            },
            JudgeFailure::Refused(404),
        ),
        (
            || ModelError::Unreachable("connection refused".to_string()),
            JudgeFailure::Unreachable,
        ),
        (
            || ModelError::Held("the meter did not answer".to_string()),
            JudgeFailure::Held,
        ),
    ];
    for (error, cause) in cases {
        let judge = ModelJudge::new(Arc::new(RefusingDoor(error)), "oag/judge");
        assert_eq!(judge.judge(ask()).await, ReviewVerdict::Unavailable(cause));
    }
    let words = opengrok_tools::review::unavailable_reason(JudgeFailure::SpendCap);
    assert!(words.contains("spend limit"), "{words}");
    let words = opengrok_tools::review::unavailable_reason(JudgeFailure::Refused(404));
    assert!(words.contains("404"), "{words}");
}

#[tokio::test]
async fn the_mock_doors_canned_verdict_is_honoured() {
    for word in ["allow", "block", "ask"] {
        let judge = ModelJudge::new(
            Arc::new(MockDoor::echoing().with_judge_verdict(word)),
            "oag/cheap",
        );
        assert_eq!(judge.judge(ask()).await, parse_verdict(word));
    }
}

#[tokio::test]
async fn an_echoing_door_that_ignores_the_contract_is_unavailable() {
    // The plain echo door answers "You said: …" — many words — which must not be an allow.
    let judge = ModelJudge::new(Arc::new(MockDoor::echoing()), "oag/cheap");
    assert_eq!(
        judge.judge(ask()).await,
        ReviewVerdict::Unavailable(JudgeFailure::Unparseable)
    );
    let long = "You said: ".repeat(40);
    assert_eq!(
        clipped(&long, 80).chars().count(),
        81,
        "clipped, and says so"
    );
}

/// Records the request it was handed, to prove the judge asks on its OWN route with no tools
/// and no coworker prompt.
struct SpyDoor {
    seen: Mutex<Option<ModelRequest>>,
}
#[async_trait::async_trait]
impl ModelDoor for SpyDoor {
    async fn stream(
        &self,
        request: ModelRequest,
    ) -> Result<crate::model::DeltaStream, crate::model::ModelError> {
        if let Ok(mut seen) = self.seen.lock() {
            *seen = Some(request);
        }
        Ok(Box::pin(futures::stream::iter(vec![Ok(ModelDelta::Text(
            "block".to_string(),
        ))])))
    }
}

#[tokio::test]
async fn the_judge_asks_on_its_own_route_with_no_tools() {
    let door = Arc::new(SpyDoor {
        seen: Mutex::new(None),
    });
    let judge = ModelJudge::new(door.clone(), "oag/judge-route");
    assert_eq!(judge.judge(ask()).await, ReviewVerdict::Block);
    let seen = door.seen.lock().ok().and_then(|seen| seen.clone());
    let request = seen.expect("the door was asked");
    assert_eq!(request.model, "oag/judge-route");
    assert!(request.tools.is_empty());
    assert!(
        request
            .system
            .as_deref()
            .is_some_and(|s| s.starts_with(JUDGE_MARKER))
    );
    assert!(request.messages[0].content.contains("<<<ARGS"));
    assert!(
        request.messages[0]
            .content
            .contains("ASK-FIRST INSTRUCTIONS")
    );
    assert!(request.messages[0].content.contains("(none)"));
}

/// A door that never yields is a timeout, which is `Unavailable`.
struct HangingDoor;
#[async_trait::async_trait]
impl ModelDoor for HangingDoor {
    async fn stream(
        &self,
        _request: ModelRequest,
    ) -> Result<crate::model::DeltaStream, crate::model::ModelError> {
        Ok(Box::pin(futures::stream::pending()))
    }
}

#[tokio::test]
async fn a_hanging_door_times_out_to_unavailable() {
    let judge =
        ModelJudge::new(Arc::new(HangingDoor), "oag/cheap").with_timeout(Duration::from_millis(50));
    assert_eq!(
        judge.judge(ask()).await,
        ReviewVerdict::Unavailable(JudgeFailure::TimedOut)
    );
}

fn card(call: &str, reason: &str, why: &str) -> Value {
    serde_json::json!({"type": "CUSTOM", "name": "run-awaiting-approval", "callId": call,
                       "reason": reason, "why": why})
}

fn result(call: &str) -> Value {
    serde_json::json!({"type": "TOOL_CALL_RESULT", "toolCallId": call, "ok": true})
}

/// The streak a resume seeds the executor with. The run parks at every failure, so only the
/// journal can count past one.
#[test]
fn the_streak_is_read_back_from_the_runs_own_journal() {
    let down = opengrok_tools::review::unavailable_reason(JudgeFailure::SpendCap);
    let asked = opengrok_tools::REVIEW_ASK_REASON;
    assert_eq!(judge_failure_streak(&[]), 0);
    // Three failures, each approved and run: the results are the person's answers.
    let journal = vec![
        card("c1", "auto-review", &down),
        result("c1"),
        card("c2", "auto-review", &down),
        result("c2"),
        card("c3", "auto-review", &down),
    ];
    assert_eq!(judge_failure_streak(&journal), 3);
    // A call the judge let through, between two failures, starts the count again.
    let mut answered = journal.clone();
    answered.insert(2, result("c_allowed"));
    assert_eq!(judge_failure_streak(&answered), 2);
    // So does a card the judge DID answer, and any other kind of card.
    for other in [
        card("c0", "auto-review", asked),
        card("c0", "user-form", "Waiting for you"),
        card("c0", "policy-approval", &down),
    ] {
        let mut journal = vec![other];
        journal.push(card("c9", "auto-review", &down));
        assert_eq!(judge_failure_streak(&journal), 1);
    }
}

/// The judge is a real model call made on a coworker's behalf. It used to carry neither a
/// key nor a scope, so its tokens went out on the deployment's key and were checked against
/// nobody: a coworker at its cap could still spend through the safety check, once per tool
/// call. Both halves are asserted, because either alone leaves the hole open — the scope is
/// what the guard checks, the key is what the gateway bills, and a cap can only be reached
/// by spending the meter it is read from actually sees.
#[tokio::test]
async fn the_judge_is_billed_and_checked_as_the_coworker_that_raised_it() {
    let spy = Arc::new(SpyDoor {
        seen: Mutex::new(None),
    });
    let judge = ModelJudge::new(spy.clone(), "oag/cheap").for_coworker(
        "cw_ada",
        "acct_ada",
        Some(crate::model::GatewayKey::new("oag_live_adas_own")),
    );
    let _ = judge.judge(ask()).await;
    let seen = spy.seen.lock().unwrap().clone().expect("the judge called");
    assert_eq!(seen.spend_scope, Some("cw_ada".to_string()));
    assert_eq!(
        seen.spend_actor,
        Some("acct_ada".to_string()),
        "and whose pool it draws on — without this the guard holds every judge call"
    );
    assert_eq!(
        seen.gateway_key,
        Some(crate::model::GatewayKey::new("oag_live_adas_own")),
        "billed to the coworker, not the deployment"
    );

    // A judge with no coworker is billed to nobody rather than to a guessed owner, and
    // passes the guard as it always did.
    let bare = Arc::new(SpyDoor {
        seen: Mutex::new(None),
    });
    let _ = ModelJudge::new(bare.clone(), "oag/cheap")
        .judge(ask())
        .await;
    let seen = bare.seen.lock().unwrap().clone().expect("called");
    assert_eq!(seen.spend_scope, None);
    assert_eq!(seen.spend_actor, None);
    assert_eq!(seen.gateway_key, None);
}
