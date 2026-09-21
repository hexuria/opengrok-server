//! Jev answers the three kinds of question, and says plainly when it cannot answer at all.
//!
//! NOT ONE BYTE OF THIS LEAVES THE MACHINE. Two halves, and neither needs TypeSafe to be up: the
//! route tests hand the server a `MockJev`, which has no HTTP client in it at all; the client
//! tests build the real `TypeSafeJev` and point it at a socket this file opened a moment earlier.
//! The one way a stray client could reach the real service is by inheriting an address from the
//! process environment, and `no_client_here_can_inherit_an_address` is the assertion that it
//! cannot. A suite that needed a key would be a suite that gets deleted the first week it costs
//! somebody money.
//!
//! The route half needs Postgres and skips loudly without OG_DATABASE_URL; the client half does
//! not touch it, so the SDK wrapper is exercised on a machine with no database.

#![cfg(feature = "jev")]
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::AccountId;
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::password::hash_password;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::host_state::HostState;
use opengrok_server::jev::{
    Answer, ChoiceAnswer, JevConfig, JevDoor, JevError, JsonContent, MockJev, NoulAnswer, Question,
    ScoreAnswer, TypeSafeJev,
};
use opengrok_store::PgStore;
use serde_json::{Value, json};

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn close_enough(left: f64, right: f64) -> bool {
    (left - right).abs() < 1e-9
}

// ---------------------------------------------------------------------------------------------
// The route: a signed-in caller, a door that answers from a script.
// ---------------------------------------------------------------------------------------------

async fn connect() -> Option<PgStore> {
    let database_url =
        opengrok_store::gate_database_or_panic(std::env::var("OG_DATABASE_URL").ok()?);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    Some(PgStore::new(pool))
}

async fn seed_account(store: &PgStore, email: &str) -> AccountId {
    let id = AccountId::new();
    let hash = hash_password("password1").expect("hash");
    let at_ms = now_ms();
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.to_string(),
            password_hash: hash.clone(),
            first_name: "Test".to_string(),
            last_name: "User".to_string(),
            org_id: String::new(),
            plan: Plan::Ultra,
            verified: true,
            enabled: true,
            at_ms,
        })
        .expect("register");
    let view = AccountView {
        id: id.clone(),
        email: email.to_string(),
        plan: Plan::Ultra,
        trial: false,
        updated_at_ms: at_ms,
        password_hash: Some(hash),
        first_name: "Test".to_string(),
        last_name: "User".to_string(),
        org_id: None,
        verified: true,
        enabled: true,
        avatar_url: None,
    };
    store
        .append_account(&id, 0, &events, &view)
        .await
        .expect("append account");
    id
}

/// The server, with whatever Jev the test wants behind it.
fn app_with(store: PgStore, email: &str, jev: Option<Arc<dyn JevDoor>>) -> (Router, AgUiState) {
    let auth = AuthState::new(
        store,
        Arc::new(TokenMinter::new(b"jev-route-test-secret-jev-route!!!!!")),
        email.to_string(),
    )
    .with_jev(jev);
    let agui = AgUiState {
        auth,
        door: Arc::new(MockDoor::echoing()),
        model: "deployment/default".to_string(),
        auto_review_model: "deployment/default".to_string(),
        computer: None,
        vault: None,
        connectors: Connectors {
            providers: Arc::new(BTreeMap::new()),
            redirect_uri: "http://127.0.0.1/callback".to_string(),
        },
        plugins: Arc::new(BTreeMap::new()),
        host_settings: None,
    };
    let gateway = HostState::new(agui.clone(), Some("http://opengrok.lan:1447".to_string()));
    (opengrok_server::router(agui.clone(), gateway), agui)
}

async fn spawn(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://127.0.0.1:{}", addr.port())
}

fn token_for(state: &AgUiState, account: &AccountId, email: &str) -> String {
    state
        .auth
        .minter
        .mint_access(
            account.as_str(),
            "sess",
            email,
            "ultra",
            chrono::Utc::now().timestamp(),
            3600,
        )
        .expect("mint access")
}

async fn post_ask(base: &str, token: &str, body: Value) -> (u16, String) {
    let response = reqwest::Client::new()
        .post(format!("{base}/jev/ask"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .expect("request");
    let status = response.status().as_u16();
    (status, response.text().await.expect("body"))
}

/// The three kinds, in one call, with a second noul that says no — the case the confidence
/// derivation gets wrong if `noul` is passed off as a confidence.
fn three_kinds_body() -> Value {
    json!({
        "state": {"asked": "search youtube for kabisado", "did": "searched, then played a video"},
        "questions": [
            {"name": "done", "kind": "noul", "instructions": "Is what was asked now true?"},
            {"name": "overshot", "kind": "noul", "instructions": "Did it do more than was asked?",
             "yesMeans": "it carried on past the request", "noMeans": "it stopped where it was told"},
            {"name": "tone", "kind": "choice", "instructions": "How did the turn go?",
             "choices": ["fine", {"label": "overshot", "means": "it did more than was asked"}]},
            {"name": "effort", "kind": "score", "instructions": "How much work is left?",
             "levels": ["none", "a little", "most of it"]}
        ]
    })
}

fn three_kinds_answers() -> Vec<(String, Answer)> {
    vec![
        ("done".to_string(), Answer::Noul(NoulAnswer { noul: 0.93 })),
        // A CONFIDENT NO. 0.02 is two per cent sure the answer is yes, which is 98% sure it is no.
        // A route that called `noul` the confidence would report this as a 0.02 confidence and a
        // caller thresholding at 0.8 would throw away the clearest answer in the set.
        (
            "overshot".to_string(),
            Answer::Noul(NoulAnswer { noul: 0.02 }),
        ),
        (
            "tone".to_string(),
            Answer::Choice(ChoiceAnswer {
                choice: "overshot".to_string(),
                confidence: 0.71,
                probabilities: [("fine".to_string(), 0.29), ("overshot".to_string(), 0.71)]
                    .into_iter()
                    .collect(),
            }),
        ),
        (
            "effort".to_string(),
            Answer::Score(ScoreAnswer {
                score: 2.0,
                confidence: 0.64,
                legend: [
                    (0, JsonContent::String("none".to_string())),
                    (1, JsonContent::String("a little".to_string())),
                    (2, JsonContent::String("most of it".to_string())),
                ]
                .into_iter()
                .collect(),
                probabilities: [(0, 0.10), (1, 0.26), (2, 0.64)].into_iter().collect(),
            }),
        ),
    ]
}

#[tokio::test]
async fn the_three_kinds_of_question_come_back_as_three_kinds_of_answer() {
    let Some(store) = connect().await else {
        eprintln!("skipping: OG_DATABASE_URL is not set");
        return;
    };
    let email = format!("jev-kinds-{}@og.local", now_ms());
    let account = seed_account(&store, &email).await;
    let jev = Arc::new(MockJev::answering(three_kinds_answers()).with_usage(41, 3));
    let (app, state) = app_with(store, &email, Some(jev.clone()));
    let token = token_for(&state, &account, &email);
    let base = spawn(app).await;

    let (status, body) = post_ask(&base, &token, three_kinds_body()).await;
    assert_eq!(status, 200, "{body}");
    let answered: Value = serde_json::from_str(&body).expect("json");

    assert_eq!(answered["model"], json!("jev-mock"));
    assert_eq!(answered["requestId"], json!("req_mock"));
    assert_eq!(
        answered["usage"],
        json!({"inputTokens": 41, "outputTokens": 3}),
        "the counts Jev reported come back camelCased, so a caller can account for them"
    );

    let answers = answered["answers"].as_array().expect("an array of answers");
    assert_eq!(
        answers.iter().map(|a| &a["name"]).collect::<Vec<_>>(),
        vec![
            &json!("done"),
            &json!("overshot"),
            &json!("tone"),
            &json!("effort")
        ],
        "answers come back in the order the questions were asked"
    );

    // ---- noul: a probability, read as an answer plus the confidence in it ----
    let done = &answers[0];
    assert_eq!(done["kind"], json!("noul"));
    assert_eq!(done["yes"], json!(true));
    assert!(close_enough(done["confidence"].as_f64().unwrap(), 0.93));
    assert!(close_enough(
        done["probabilities"]["yes"].as_f64().unwrap(),
        0.93
    ));
    assert!(close_enough(
        done["probabilities"]["no"].as_f64().unwrap(),
        0.07
    ));

    let overshot = &answers[1];
    assert_eq!(overshot["yes"], json!(false));
    assert!(
        close_enough(overshot["confidence"].as_f64().unwrap(), 0.98),
        "a 0.02 chance of yes is a 98% confident NO, not a 2% confident anything: {overshot}"
    );

    // ---- choice: the label, and the whole distribution ----
    let tone = &answers[2];
    assert_eq!(tone["kind"], json!("choice"));
    assert_eq!(tone["choice"], json!("overshot"));
    assert!(close_enough(tone["confidence"].as_f64().unwrap(), 0.71));
    assert!(close_enough(
        tone["probabilities"]["fine"].as_f64().unwrap(),
        0.29
    ));

    // ---- score: the rung, the words for it, and the rubric it came from ----
    let effort = &answers[3];
    assert_eq!(effort["kind"], json!("score"));
    assert!(close_enough(effort["score"].as_f64().unwrap(), 2.0));
    assert_eq!(
        effort["level"],
        json!("most of it"),
        "a rung with no legend beside it means nothing to a reader"
    );
    assert_eq!(effort["legend"]["1"], json!("a little"));
    assert!(close_enough(
        effort["probabilities"]["2"].as_f64().unwrap(),
        0.64
    ));

    // ---- and the question that went out is the question that was written ----
    let asked = jev.asked();
    assert_eq!(asked.len(), 1, "one call, not one per question");
    let ask = &asked[0];
    assert_eq!(
        ask.questions
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        vec!["done", "overshot", "tone", "effort"]
    );
    assert_eq!(
        ask.questions[0].1,
        Question::noul("Is what was asked now true?")
    );
    assert_eq!(
        ask.questions[2].1,
        Question::choice(
            "How did the turn go?",
            [
                ("fine", None),
                (
                    "overshot",
                    Some(JsonContent::String(
                        "it did more than was asked".to_string()
                    ))
                )
            ]
        ),
        "a described choice keeps its description, and a bare one has none"
    );
    assert_eq!(
        ask.questions[3].1,
        Question::score("How much work is left?", ["none", "a little", "most of it"])
    );
    assert!(
        ask.model.is_none(),
        "a body that names no model leaves the deployment's default alone"
    );
}

#[tokio::test]
async fn a_deployment_with_no_jev_key_says_so_in_words() {
    let Some(store) = connect().await else {
        eprintln!("skipping: OG_DATABASE_URL is not set");
        return;
    };
    let email = format!("jev-nokey-{}@og.local", now_ms());
    let account = seed_account(&store, &email).await;
    // No door at all: what `AuthState::new` resolves to on a server with no OG_JEV_API_KEY.
    let (app, state) = app_with(store, &email, None);
    let token = token_for(&state, &account, &email);
    let base = spawn(app).await;

    let (status, body) = post_ask(&base, &token, three_kinds_body()).await;
    assert_eq!(
        status, 503,
        "an unconfigured classifier is a service that is not there, not a server error: {body}"
    );
    assert!(
        body.contains("OG_JEV_API_KEY"),
        "the refusal has to name the thing that is missing: {body}"
    );
    assert!(
        !body.contains("panic"),
        "and it is a sentence, not a crash: {body}"
    );

    // A caller with no bearer is turned away before any of that.
    let anonymous = reqwest::Client::new()
        .post(format!("{base}/jev/ask"))
        .json(&three_kinds_body())
        .send()
        .await
        .expect("request");
    assert_eq!(anonymous.status().as_u16(), 401);
}

#[tokio::test]
async fn an_unreachable_jev_a_slow_one_and_a_refused_one_are_three_different_answers() {
    let Some(store) = connect().await else {
        eprintln!("skipping: OG_DATABASE_URL is not set");
        return;
    };
    let email = format!("jev-failures-{}@og.local", now_ms());
    let account = seed_account(&store, &email).await;

    let cases: Vec<(JevError, u16, &str)> = vec![
        (
            JevError::Unreachable("connection refused".to_string()),
            502,
            "unreachable",
        ),
        (
            JevError::TimedOut(Duration::from_secs(10)),
            504,
            "did not answer within",
        ),
        (
            JevError::Refused {
                status: 401,
                message: "401 authentication failed".to_string(),
            },
            502,
            "refused",
        ),
        (
            JevError::Refused {
                status: 429,
                message: "429 slow down".to_string(),
            },
            429,
            "refused",
        ),
    ];

    let mut seen: Vec<(u16, String)> = Vec::new();
    for (error, expected, fragment) in cases {
        let jev = Arc::new(MockJev::failing_with(error.clone()));
        let (app, state) = app_with(store.clone(), &email, Some(jev));
        let token = token_for(&state, &account, &email);
        let base = spawn(app).await;
        let (status, body) = post_ask(&base, &token, three_kinds_body()).await;
        assert_eq!(status, expected, "{error:?} answered {status}: {body}");
        assert!(
            body.to_lowercase().contains(fragment),
            "{error:?} must say so in words, got {body}"
        );
        seen.push((status, body));
    }

    // THE POINT OF THE WHOLE TEST. Four failures, and a caller can still tell them apart: an
    // expired key must never be indistinguishable from a network blip, because the first needs a
    // person and the second needs a minute.
    assert_eq!(
        seen.iter()
            .map(|(_, body)| body)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        4,
        "four different failures came back as fewer than four different sentences: {seen:?}"
    );
}

#[tokio::test]
async fn a_question_that_could_never_be_answered_is_refused_before_it_is_asked() {
    let Some(store) = connect().await else {
        eprintln!("skipping: OG_DATABASE_URL is not set");
        return;
    };
    let email = format!("jev-refused-{}@og.local", now_ms());
    let account = seed_account(&store, &email).await;
    let jev = Arc::new(MockJev::answering(three_kinds_answers()));
    let (app, state) = app_with(store, &email, Some(jev.clone()));
    let token = token_for(&state, &account, &email);
    let base = spawn(app).await;

    let bad: Vec<(Value, &str)> = vec![
        (
            json!({"state": "something happened", "questions": []}),
            "at least one question",
        ),
        (
            json!({"state": "something happened", "questions": [
                {"name": "done", "kind": "noul", "instructions": "Is it done?"},
                {"name": "done", "kind": "noul", "instructions": "Really?"}]}),
            "both called",
        ),
        (
            json!({"state": "something happened", "questions": [
                {"name": "tone", "kind": "choice", "instructions": "How did it go?",
                 "choices": ["fine"]}]}),
            "at least two choices",
        ),
        (
            json!({"state": "something happened", "questions": [
                {"name": "effort", "kind": "score", "instructions": "How much is left?",
                 "levels": []}]}),
            "no levels",
        ),
        (
            // A bare number is not a state the classifier can read, and the SDK says so; saying it
            // here means the caller is told before a request is built.
            json!({"state": 7, "questions": [
                {"name": "done", "kind": "noul", "instructions": "Is it done?"}]}),
            "string, object, or array",
        ),
    ];

    for (body, fragment) in bad {
        let (status, said) = post_ask(&base, &token, body.clone()).await;
        assert_eq!(status, 400, "{body} answered {status}: {said}");
        assert!(
            said.contains(fragment),
            "{body} was refused with {said:?}, which does not say {fragment:?}"
        );
    }

    assert!(
        jev.asked().is_empty(),
        "not one of those reached Jev: a question that cannot be answered must not be billed"
    );
}

// ---------------------------------------------------------------------------------------------
// The client wrapper: the real SDK, pointed at sockets this file owns.
// ---------------------------------------------------------------------------------------------

/// A client with the SDK's own defaults except the two a test cannot wait for: one attempt, and
/// a short patience. Retries are the SDK's everywhere else — this turns them off so a test that
/// asserts on a failure does not pay 1.5 s of backoff to learn what it already knows.
fn client_at(base_url: &str, timeout: Duration) -> TypeSafeJev {
    let mut config = JevConfig::with_key("tsk_test_key_never_logged");
    config.base_url = base_url.to_string();
    config.model = "jev-test".to_string();
    config.timeout = timeout;
    config.retry.max_retries = 0;
    TypeSafeJev::new(config).expect("build a client")
}

fn one_question() -> opengrok_server::jev::Ask {
    opengrok_server::jev::Ask {
        state: JsonContent::String("something happened".to_string()),
        questions: vec![("done".to_string(), Question::noul("Is it done?"))],
        model: None,
    }
}

#[tokio::test]
async fn the_wrapper_keeps_the_sdks_failures_apart() {
    // ---- unreachable: a port with nothing behind it ----
    let dead = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let dead_port = dead.local_addr().expect("addr").port();
    drop(dead);
    let error = client_at(
        &format!("http://127.0.0.1:{dead_port}"),
        Duration::from_secs(2),
    )
    .ask(one_question())
    .await
    .expect_err("nothing is listening there");
    assert!(
        matches!(error, JevError::Unreachable(_)),
        "a refused connection is unreachable, not a timeout: {error:?}"
    );

    // ---- timed out: a socket that accepts and then says nothing, ever ----
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let silent_port = silent.local_addr().expect("addr").port();
    tokio::spawn(async move {
        // The accepted sockets are held rather than dropped: a dropped one closes the connection,
        // which the client would see as a transport failure instead of the silence this needs.
        let mut held = Vec::new();
        while let Ok((socket, _)) = silent.accept().await {
            held.push(socket);
        }
    });
    let error = client_at(
        &format!("http://127.0.0.1:{silent_port}"),
        Duration::from_millis(150),
    )
    .ask(one_question())
    .await
    .expect_err("nothing ever answers there");
    assert!(
        matches!(error, JevError::TimedOut(_)),
        "a silent server is a timeout, not an unreachable one: {error:?}"
    );

    // ---- refused: a service that answers, with a status ----
    let base = stand_in_jev(Arc::new(Mutex::new(Vec::new())), StandInReply::Status(429)).await;
    let error = client_at(&base, Duration::from_secs(5))
        .ask(one_question())
        .await
        .expect_err("429 is not an answer");
    assert!(
        matches!(error, JevError::Refused { status: 429, .. }),
        "the status TypeSafe sent survives to the caller: {error:?}"
    );

    // ---- asked: a question the SDK refuses to send at all ----
    let mut nonsense = one_question();
    nonsense.questions = vec![(
        "effort".to_string(),
        Question::score("How much is left?", Vec::<String>::new()),
    )];
    let error = client_at(&base, Duration::from_secs(5))
        .ask(nonsense)
        .await
        .expect_err("a rubric with no rungs cannot be asked");
    assert!(
        matches!(error, JevError::Asked(_)),
        "our own malformed question is not the service's fault: {error:?}"
    );
}

#[tokio::test]
async fn a_question_goes_out_as_jev_expects_and_the_answer_comes_back_typed() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = stand_in_jev(seen.clone(), StandInReply::Answers).await;
    let judged = client_at(&base, Duration::from_secs(5))
        .ask(opengrok_server::jev::Ask {
            state: JsonContent::Object(
                json!({"asked": "search youtube", "did": "searched, then played"})
                    .as_object()
                    .expect("object")
                    .clone(),
            ),
            questions: vec![
                (
                    "done".to_string(),
                    Question::noul("Is what was asked now true?"),
                ),
                (
                    "tone".to_string(),
                    Question::choice(
                        "How did the turn go?",
                        [
                            ("fine", None),
                            (
                                "overshot",
                                Some(JsonContent::String(
                                    "it did more than was asked".to_string(),
                                )),
                            ),
                        ],
                    ),
                ),
                (
                    "effort".to_string(),
                    Question::score("How much work is left?", ["none", "a little", "most of it"]),
                ),
            ],
            model: Some("jev-asked-for".to_string()),
        })
        .await
        .expect("the stand-in answers");

    // ---- what went out ----
    let sent = seen.lock().expect("lock").clone();
    assert_eq!(sent.len(), 1, "one request, not one per question");
    let (headers, body) = &sent[0];
    assert_eq!(
        headers.get("authorization").map(|v| v.to_str().unwrap()),
        Some("Bearer tsk_test_key_never_logged"),
        "the key travels in the header and nowhere else"
    );
    assert_eq!(
        body["state"],
        json!({"asked": "search youtube", "did": "searched, then played"})
    );
    assert_eq!(
        body["model"],
        json!("jev-asked-for"),
        "a per-call model overrides the client's default"
    );
    assert_eq!(
        body["questions"]["done"],
        json!({"type": "noul", "instructions": "Is what was asked now true?"})
    );
    assert_eq!(
        body["questions"]["tone"],
        json!({"type": "choice", "instructions": "How did the turn go?",
               "criteria": {"fine": null, "overshot": "it did more than was asked"}})
    );
    assert_eq!(
        body["questions"]["effort"],
        json!({"type": "score", "instructions": "How much work is left?",
               "criteria": ["none", "a little", "most of it"]})
    );

    // ---- what came back ----
    assert_eq!(judged.model, "jev-1");
    assert_eq!(judged.request_id.as_deref(), Some("req_stand_in"));
    assert_eq!(judged.usage.input_tokens, Some(41));
    assert_eq!(judged.usage.output_tokens, Some(3));
    let names: Vec<&str> = judged
        .answers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(names, vec!["done", "tone", "effort"]);
    match &judged.answers[0].1 {
        Answer::Noul(noul) => assert!(close_enough(noul.noul, 0.93)),
        other => panic!("a noul question was answered {other:?}"),
    }
    match &judged.answers[1].1 {
        Answer::Choice(choice) => {
            assert_eq!(choice.choice, "overshot");
            assert!(close_enough(choice.confidence, 0.71));
            assert!(close_enough(choice.probabilities["fine"], 0.29));
        }
        other => panic!("a choice question was answered {other:?}"),
    }
    match &judged.answers[2].1 {
        Answer::Score(score) => {
            assert!(close_enough(score.score, 2.0));
            assert_eq!(
                score.legend.get(&2),
                Some(&JsonContent::String("most of it".to_string()))
            );
            assert!(close_enough(score.probabilities[&2], 0.64));
        }
        other => panic!("a score question was answered {other:?}"),
    }
}

#[tokio::test]
async fn the_api_key_never_reaches_a_log_line() {
    let jev = client_at("http://127.0.0.1:9", Duration::from_secs(1));
    let printed = format!("{jev:?}");
    assert!(
        !printed.contains("tsk_test_key_never_logged"),
        "the SDK's own client derives Debug over a config holding the key in a String, so this \
         type must not print it: {printed}"
    );
    assert!(printed.contains("127.0.0.1:9"), "{printed}");

    let mut config = JevConfig::with_key("tsk_test_key_never_logged");
    config.base_url = "http://127.0.0.1:9".to_string();
    let printed = format!("{config:?}");
    assert!(
        !printed.contains("tsk_test_key_never_logged"),
        "a config printed into a log must redact its key: {printed}"
    );
    assert!(printed.contains("redacted"), "{printed}");
}

#[tokio::test]
async fn no_client_here_can_inherit_an_address_from_the_environment() {
    let jev = client_at("http://127.0.0.1:9", Duration::from_secs(1));
    assert_eq!(
        jev.base_url(),
        "http://127.0.0.1:9",
        "a configured address wins, so TYPESAFE_BASE_URL cannot retarget a deployment"
    );
    assert_ne!(
        jev.base_url(),
        JevConfig::with_key("unused").base_url,
        "and no test in this file points at the real service"
    );
    assert_eq!(jev.model(), "jev-test");
}

// ---------------------------------------------------------------------------------------------
// A Jev-shaped stand-in on loopback: what the SDK would have talked to.
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum StandInReply {
    /// One canned System One body with all three kinds in it.
    Answers,
    /// A status and nothing worth reading, for the refusal path.
    Status(u16),
}

type Seen = Arc<Mutex<Vec<(HeaderMap, Value)>>>;

async fn stand_in_jev(seen: Seen, reply: StandInReply) -> String {
    let app = Router::new()
        .route("/v1/systemone", post(stand_in_system_one))
        .with_state((seen, reply));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://127.0.0.1:{port}")
}

async fn stand_in_system_one(
    State((seen, reply)): State<(Seen, StandInReply)>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    match seen.lock() {
        Ok(mut seen) => seen.push((headers, parsed)),
        Err(poisoned) => poisoned.into_inner().push((headers, parsed)),
    }
    match reply {
        StandInReply::Status(status) => (
            axum::http::StatusCode::from_u16(status).expect("a status"),
            axum::Json(json!({"error": {"message": "not today"}})),
        )
            .into_response(),
        StandInReply::Answers => (
            [("x-typesafe-request-id", "req_stand_in")],
            axum::Json(json!({
                "model": "jev-1",
                "usage": {"input_tokens": 41, "output_tokens": 3},
                "answers": {
                    "done": {"type": "noul", "noul": 0.93},
                    "tone": {"type": "choice", "choice": "overshot", "confidence": 0.71,
                             "probabilities": {"fine": 0.29, "overshot": 0.71}},
                    "effort": {"type": "score", "score": 2, "confidence": 0.64,
                               "legend": {"0": "none", "1": "a little", "2": "most of it"},
                               "probabilities": {"0": 0.10, "1": 0.26, "2": 0.64}}
                }
            })),
        )
            .into_response(),
    }
}
