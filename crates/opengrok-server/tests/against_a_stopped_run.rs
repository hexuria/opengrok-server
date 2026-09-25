//! A person can stop a run, and it stays stopped.
//!
//! A taught recipe ran, and the bot ran it again, and again, opening the browser and typing the
//! search term each time until the box's search field read `kabisadokabisado`. The person typed
//! "stop it" into the chat, which reached nothing — that is one more message to a model that is
//! mid-turn. `POST /ag-ui/runs/{id}/stop` is a command against the RUN, so it reaches the log
//! rather than the model, and everything that reads a run reads it there: the turn at its next
//! step boundary, the recovery sweep, `replay_run`.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, CoworkerId, RunId};
use opengrok_core::run::{Run, RunCommand, RunEvent, RunStatus, RunView, SuspendReason};
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::password::hash_password;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::host_state::HostState;
use opengrok_store::PgStore;
use serde_json::{Value, json};

macro_rules! database_or_skip {
    () => {
        match std::env::var("OG_DATABASE_URL") {
            Ok(url) => opengrok_store::gate_database_or_panic(url),
            Err(_) => {
                eprintln!("skipping: OG_DATABASE_URL is not set");
                return;
            }
        }
    };
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
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

/// How the seeded run is left: what a stop is being asked to do something about.
enum Ending<'a> {
    /// Still going, with nothing in the log to say otherwise — the shape a turn in flight has.
    Running,
    Finished,
    Failed(&'a str),
    AwaitingApproval,
    /// Already stopped, for the sweep to find.
    Stopped,
}

fn record(run: &mut Run, log: &mut Vec<RunEvent>, produced: Vec<RunEvent>) {
    for event in &produced {
        run.apply(event);
    }
    log.extend(produced);
}

/// Journal one whole run: a start, its frames, and whatever ending it has.
///
/// `at_ms` is the projection's `updated_at_ms`, which is what the recovery sweep reads to decide a
/// run has been silent long enough to be abandoned — so a test can date a run into the past rather
/// than wait out a sixty-second lease.
async fn seed_run(
    store: &PgStore,
    account: &AccountId,
    thread: &str,
    at_ms: i64,
    deltas: &[&str],
    ending: Ending<'_>,
) -> RunId {
    let id = RunId::new();
    let mut run = Run::default();
    let mut log = Vec::new();

    let produced = run
        .decide(RunCommand::Start {
            thread_id: thread.to_string(),
            coworker_id: Some(CoworkerId::from_stored("cw_stop_test")),
            model: Some("oag/cheap".to_string()),
            system: None,
            skill_id: None,
            at_ms,
        })
        .expect("start");
    record(&mut run, &mut log, produced);

    for delta in deltas {
        let produced = run
            .decide(RunCommand::Emit {
                payload: json!({
                    "type": "TEXT_MESSAGE_CONTENT",
                    "messageId": "msg-1",
                    "delta": delta,
                }),
                at_ms,
            })
            .expect("emit");
        record(&mut run, &mut log, produced);
    }

    let produced = match ending {
        Ending::Running => Ok(Vec::new()),
        Ending::Finished => run.decide(RunCommand::Finish { at_ms }),
        Ending::Failed(reason) => run.decide(RunCommand::Fail {
            reason: reason.to_string(),
            at_ms,
        }),
        Ending::AwaitingApproval => run.decide(RunCommand::Suspend {
            call_id: "call-1".to_string(),
            tool: "shell".to_string(),
            arguments: json!({ "command": "play the recipe" }),
            reason: SuspendReason::ExecConsent,
            at_ms,
        }),
        Ending::Stopped => run.decide(RunCommand::Stop {
            by: account.as_str().to_string(),
            at_ms,
        }),
    }
    .expect("ending");
    record(&mut run, &mut log, produced);

    let view = RunView {
        id: id.clone(),
        thread_id: thread.to_string(),
        status: run.status,
        event_count: run.emitted.len() as i64,
        updated_at_ms: at_ms,
    };
    store
        .append_run(&id, 0, &log, &view, Some(account))
        .await
        .expect("append run");
    id
}

struct Harness {
    base: String,
    client: reqwest::Client,
    store: PgStore,
    minter: Arc<TokenMinter>,
    state: AgUiState,
}

async fn harness(database_url: &str, email: &str) -> Harness {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let minter = Arc::new(TokenMinter::new(b"stop-a-run-secret"));
    let auth = AuthState::new(store.clone(), minter.clone(), email.to_string());
    let agui = AgUiState {
        auth,
        door: Arc::new(MockDoor::echoing()),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
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
    let app = opengrok_server::router(agui.clone(), gateway);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    Harness {
        base: format!("http://127.0.0.1:{}", addr.port()),
        client: reqwest::Client::new(),
        store,
        minter,
        state: agui,
    }
}

impl Harness {
    /// Somebody signed in. Every account minted here is a real one in the store, because the owner
    /// check reads the run's `account_id` and not the token's shape.
    async fn person(&self, email: &str) -> (AccountId, String) {
        let account = seed_account(&self.store, email).await;
        let access = self
            .minter
            .mint_access(
                account.as_str(),
                "sess-stop",
                email,
                "ultra",
                chrono::Utc::now().timestamp(),
                3600,
            )
            .expect("mint access");
        (account, access)
    }

    async fn stop(&self, access: &str, run_id: &str) -> (u16, String) {
        let res = self
            .client
            .post(format!("{}/ag-ui/runs/{run_id}/stop", self.base))
            .header("Authorization", format!("Bearer {access}"))
            .send()
            .await
            .expect("stop a run");
        let status = res.status().as_u16();
        (status, res.text().await.expect("body"))
    }

    async fn stop_json(&self, access: &str, run_id: &str) -> Value {
        let (status, body) = self.stop(access, run_id).await;
        assert_eq!(status, 202, "a stop is accepted, not merely ok: {body}");
        serde_json::from_str(&body).expect("json body")
    }

    async fn replay(&self, access: &str, run_id: &str) -> Value {
        let res = self
            .client
            .get(format!("{}/ag-ui/runs/{run_id}", self.base))
            .header("Authorization", format!("Bearer {access}"))
            .send()
            .await
            .expect("replay a run");
        assert_eq!(res.status().as_u16(), 200);
        res.json().await.expect("json body")
    }

    async fn approvals(&self, access: &str) -> Value {
        let res = self
            .client
            .get(format!("{}/ag-ui/approvals", self.base))
            .header("Authorization", format!("Bearer {access}"))
            .send()
            .await
            .expect("approvals");
        assert_eq!(res.status().as_u16(), 200);
        res.json().await.expect("json body")
    }
}

/// Sweep until THIS run is settled, rather than once.
///
/// `sweep_once` claims a bounded batch of whatever is abandoned database-wide, and a shared test
/// database accumulates stale runs from every earlier suite — so a single sweep is not guaranteed
/// to include ours, and asserting on its return count would test the fixture rather than the code.
async fn sweep_until_failed(state: &AgUiState, store: &PgStore, run_id: &RunId) {
    for _ in 0..40 {
        opengrok_server::recovery::sweep_once(state)
            .await
            .expect("sweep");
        if let Ok((run, _)) = store.load_run(run_id).await
            && run.status == RunStatus::Failed
        {
            return;
        }
    }
    panic!("the sweep never settled the abandoned run");
}

/// THE ONE THE BUTTON IS FOR. A run that is going is stopped, and stopped is an ending: nothing
/// can finish it, fail it or stop it again afterwards.
#[tokio::test]
async fn a_running_run_is_stopped_and_stays_stopped() {
    let database_url = database_or_skip!();
    let email = format!("stop-running-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let (account, access) = h.person(&email).await;
    let thread = format!("th-{}", uuid::Uuid::now_v7().simple());

    let run_id = seed_run(
        &h.store,
        &account,
        &thread,
        now_ms(),
        &["opening the browser"],
        Ending::Running,
    )
    .await;

    let body = h.stop_json(&access, run_id.as_str()).await;
    assert_eq!(body["runId"], json!(run_id.as_str()), "{body}");
    assert_eq!(body["status"], json!("stopped"), "{body}");
    assert_eq!(
        body["takesEffect"],
        json!("next-step"),
        "a turn that was going stops at its next step boundary, and the answer must not promise \
         more than that: {body}"
    );

    let (run, _) = h.store.load_run(&run_id).await.expect("load run");
    assert_eq!(run.status, RunStatus::Stopped);
    assert!(run.status.is_terminal(), "a stop is an ending");
    assert_eq!(
        run.failure, None,
        "and it is NOT a failure: a person changing their mind must not turn up in the answer to \
         'why did this run fail'"
    );
    assert_eq!(run.stopped_by.as_deref(), Some(account.as_str()));

    // Nothing may move it off that status afterwards, whatever arrives late.
    for late in [
        RunCommand::Finish { at_ms: now_ms() },
        RunCommand::Fail {
            reason: "a late failure".to_string(),
            at_ms: now_ms(),
        },
    ] {
        assert!(
            run.decide(late).is_err(),
            "a stopped run accepts no second ending"
        );
    }

    assert_eq!(
        h.replay(&access, run_id.as_str()).await["status"],
        json!("stopped"),
        "and a client reading the run back is told so"
    );
}

/// The button was pressed twice, or the client retried, or two devices pressed it together. All
/// three are the same request and all three are a success — an error here would make a retry look
/// like a fault to somebody who did exactly the right thing.
#[tokio::test]
async fn stopping_twice_is_a_success_both_times() {
    let database_url = database_or_skip!();
    let email = format!("stop-twice-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let (account, access) = h.person(&email).await;
    let thread = format!("th-{}", uuid::Uuid::now_v7().simple());

    let run_id = seed_run(
        &h.store,
        &account,
        &thread,
        now_ms(),
        &["still going"],
        Ending::Running,
    )
    .await;

    let first = h.stop_json(&access, run_id.as_str()).await;
    assert_eq!(first["takesEffect"], json!("next-step"), "{first}");

    let second = h.stop_json(&access, run_id.as_str()).await;
    assert_eq!(second["runId"], json!(run_id.as_str()), "{second}");
    assert_eq!(second["status"], json!("stopped"), "{second}");
    assert_eq!(
        second["takesEffect"],
        json!("already-ended"),
        "the second press is still a success, and says why there was nothing to do: {second}"
    );

    // The log records ONE stop however many times the button is pressed: every attempt after the
    // first produces no event at all, so the run's log does not grow per press.
    let (run, seq) = h.store.load_run(&run_id).await.expect("load run");
    assert_eq!(run.status, RunStatus::Stopped);
    let third = h.stop_json(&access, run_id.as_str()).await;
    assert_eq!(third["takesEffect"], json!("already-ended"), "{third}");
    let (_, after) = h.store.load_run(&run_id).await.expect("load run again");
    assert_eq!(seq, after, "a repeated stop writes nothing to the log");
}

/// Whether the person won the race with the model is not their problem. Stopping a run that has
/// already ended is a success — and it must not rewrite how that run ended.
#[tokio::test]
async fn stopping_a_finished_run_succeeds_without_changing_its_outcome() {
    let database_url = database_or_skip!();
    let email = format!("stop-finished-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let (account, access) = h.person(&email).await;
    let thread = format!("th-{}", uuid::Uuid::now_v7().simple());

    let finished = seed_run(
        &h.store,
        &account,
        &thread,
        now_ms(),
        &["all done"],
        Ending::Finished,
    )
    .await;
    let failed = seed_run(
        &h.store,
        &account,
        &thread,
        now_ms(),
        &["most of the way"],
        Ending::Failed("the model gateway refused"),
    )
    .await;

    let answer = h.stop_json(&access, finished.as_str()).await;
    assert_eq!(answer["status"], json!("stopped"), "{answer}");
    assert_eq!(answer["takesEffect"], json!("already-ended"), "{answer}");

    let replayed = h.replay(&access, finished.as_str()).await;
    assert_eq!(
        replayed["status"],
        json!("finished"),
        "the run's own outcome is untouched — the stop had nothing to stop: {replayed}"
    );
    assert_eq!(
        replayed["events"][0]["delta"],
        json!("all done"),
        "and what it did is still there: {replayed}"
    );

    let on_failed = h.stop_json(&access, failed.as_str()).await;
    assert_eq!(
        on_failed["takesEffect"],
        json!("already-ended"),
        "{on_failed}"
    );
    let failed_replay = h.replay(&access, failed.as_str()).await;
    assert_eq!(failed_replay["status"], json!("failed"), "{failed_replay}");
    assert_eq!(
        failed_replay["failure"],
        json!("the model gateway refused"),
        "a stop must not overwrite the reason a run actually failed: {failed_replay}"
    );
}

/// A run id is not a password, and run ids travel in client URLs and logs. "Not yours" and "no
/// such run" have to be indistinguishable down to the bytes, or this route becomes a way to find
/// out which runs exist.
#[tokio::test]
async fn somebody_elses_run_and_no_run_at_all_answer_exactly_the_same() {
    let database_url = database_or_skip!();
    let owner_email = format!("stop-owner-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &owner_email).await;
    let (owner, owner_access) = h.person(&owner_email).await;
    let stranger_email = format!("stop-stranger-{}@og.local", uuid::Uuid::now_v7().simple());
    let (_stranger, stranger_access) = h.person(&stranger_email).await;
    let thread = format!("th-{}", uuid::Uuid::now_v7().simple());

    let run_id = seed_run(
        &h.store,
        &owner,
        &thread,
        now_ms(),
        &["something private"],
        Ending::Running,
    )
    .await;

    let (theirs_status, theirs_body) = h.stop(&stranger_access, run_id.as_str()).await;
    let unknown = RunId::new();
    let (unknown_status, unknown_body) = h.stop(&stranger_access, unknown.as_str()).await;

    assert_eq!(
        theirs_status, 404,
        "a run that is not yours is not a 403: that would confirm it exists"
    );
    assert_eq!(
        (theirs_status, theirs_body.as_str()),
        (unknown_status, unknown_body.as_str()),
        "the two answers must be indistinguishable, or a run id can be probed"
    );

    // No token at all is the same answer again, for the same reason.
    let res = h
        .client
        .post(format!("{}/ag-ui/runs/{}/stop", h.base, run_id.as_str()))
        .send()
        .await
        .expect("no-token request");
    let anon_status = res.status().as_u16();
    let anon_body = res.text().await.expect("body");
    assert_eq!(
        (anon_status, anon_body.as_str()),
        (theirs_status, theirs_body.as_str()),
        "not signed in must look exactly like no such run"
    );

    // And the stranger's attempt did nothing: the owner's run is untouched.
    let (run, _) = h.store.load_run(&run_id).await.expect("load run");
    assert_eq!(
        run.status,
        RunStatus::Running,
        "a stranger must not be able to stop somebody else's work"
    );

    // The owner really can, so the 404s above are the owner check and not an empty database.
    let mine = h.stop_json(&owner_access, run_id.as_str()).await;
    assert_eq!(mine["takesEffect"], json!("next-step"), "{mine}");
}

/// A STOP THAT RECOVERY UNDOES IS NOT A STOP. The sweep exists to end runs whose process died —
/// and a run that was stopped and then abandoned looks exactly like that from the outside. It must
/// keep its own ending rather than be rewritten as "interrupted by a restart".
#[tokio::test]
async fn a_stopped_run_is_left_alone_by_the_recovery_sweep() {
    let database_url = database_or_skip!();
    let email = format!("stop-sweep-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let (account, _access) = h.person(&email).await;
    let thread = format!("th-{}", uuid::Uuid::now_v7().simple());

    // Old enough that the lease has certainly lapsed; the sweep reads this stamp, not a clock.
    let stale = now_ms() - (opengrok_server::recovery::LEASE_MS * 4);

    let stopped = seed_run(
        &h.store,
        &account,
        &thread,
        stale,
        &["opening the browser"],
        Ending::Stopped,
    )
    .await;
    // The control: the same fixture in every respect except that nobody stopped it. Without this,
    // a sweep that skipped the stopped run because it was too FRESH would pass unnoticed.
    let abandoned = seed_run(
        &h.store,
        &account,
        &thread,
        stale,
        &["opening the browser"],
        Ending::Running,
    )
    .await;

    sweep_until_failed(&h.state, &h.store, &abandoned).await;

    let (still_stopped, _) = h.store.load_run(&stopped).await.expect("load stopped run");
    assert_eq!(
        still_stopped.status,
        RunStatus::Stopped,
        "the sweep must not reopen a question a person already settled"
    );
    assert_eq!(
        still_stopped.failure, None,
        "and it must not be relabelled a failure: {:?}",
        still_stopped.failure
    );

    let (control, _) = h
        .store
        .load_run(&abandoned)
        .await
        .expect("load control run");
    assert_eq!(
        control.status,
        RunStatus::Failed,
        "the control proves the fixture really was claimable"
    );
}

/// WHAT THE COWORKER DID BEFORE IT WAS STOPPED IS STILL THE RECORD. The frames are the only
/// account of what happened — which recipe played, what it typed — and a stop that took them away
/// would leave the person with no way to see what they stopped.
#[tokio::test]
async fn the_frames_from_before_the_stop_are_still_readable() {
    let database_url = database_or_skip!();
    let email = format!("stop-frames-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let (account, access) = h.person(&email).await;
    let thread = format!("th-{}", uuid::Uuid::now_v7().simple());

    let run_id = seed_run(
        &h.store,
        &account,
        &thread,
        now_ms(),
        &["searching for ", "kabisado", " again"],
        Ending::Running,
    )
    .await;

    h.stop_json(&access, run_id.as_str()).await;

    let replayed = h.replay(&access, run_id.as_str()).await;
    assert_eq!(replayed["status"], json!("stopped"), "{replayed}");
    let deltas: Vec<&str> = replayed["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|event| event["delta"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        deltas,
        vec!["searching for ", "kabisado", " again"],
        "every frame the run emitted, in order, exactly as before the stop: {replayed}"
    );
}

/// Stopping a run that is waiting on a card is immediate, because nothing is in flight — and the
/// card goes with it. A question nobody is going to act on must not sit in somebody's approvals
/// queue forever.
#[tokio::test]
async fn stopping_a_run_that_was_waiting_closes_its_card_at_once() {
    let database_url = database_or_skip!();
    let email = format!("stop-waiting-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let (account, access) = h.person(&email).await;
    let thread = format!("th-{}", uuid::Uuid::now_v7().simple());

    let run_id = seed_run(
        &h.store,
        &account,
        &thread,
        now_ms(),
        &["about to run a command"],
        Ending::AwaitingApproval,
    )
    .await;

    let waiting = h.approvals(&access).await;
    assert!(
        waiting
            .as_array()
            .expect("an array")
            .iter()
            .any(|card| card["runId"] == json!(run_id.as_str())),
        "the card is really open before the stop: {waiting}"
    );

    let answer = h.stop_json(&access, run_id.as_str()).await;
    assert_eq!(
        answer["takesEffect"],
        json!("immediately"),
        "waiting on a person is not working, so there is no step to finish first: {answer}"
    );
    // #186. A stop settles a form's card, not an approval card, so the note must not say the
    // card is closed — only what is true of the run.
    let note = answer["note"].as_str().expect("a note");
    assert!(!note.contains("is closed"), "{answer}");
    assert!(note.contains("nothing it asked for will run"), "{answer}");

    let replayed = h.replay(&access, run_id.as_str()).await;
    assert_eq!(replayed["status"], json!("stopped"), "{replayed}");
    assert_eq!(
        replayed["pending"],
        Value::Null,
        "the question the run was asking is moot: {replayed}"
    );

    let after = h.approvals(&access).await;
    assert!(
        !after
            .as_array()
            .expect("an array")
            .iter()
            .any(|card| card["runId"] == json!(run_id.as_str())),
        "and it is gone from the queue: {after}"
    );
}

/// A ROUND JOURNALED WHILE A STOP LANDS KEEPS ITS FRAMES (`formal/tla/JournalAppend.tla`
/// RoundKept). Every writer reads the run and appends at the next seq, so two writers at once
/// means one of them is refused with a Conflict. The Stop retried its own; the turn's journal did
/// not, so the round on screen when the button was pressed was dropped from the log — the frames
/// `run.rs` goes on accepting on a stopped run for exactly that moment. Three rounds race a Stop
/// here, on several runs so the race is really run: every round lands once, and the run ends
/// stopped.
#[tokio::test]
async fn a_round_journaled_while_a_stop_lands_keeps_its_frames() {
    use opengrok_harness::RunJournal;
    use opengrok_wire::agui::{Event, EventType};

    let database_url = database_or_skip!();
    let email = format!("stop-race-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let (account, access) = h.person(&email).await;

    for attempt in 0..8 {
        let thread = format!("th-{}", uuid::Uuid::now_v7().simple());
        let run_id = seed_run(
            &h.store,
            &account,
            &thread,
            now_ms(),
            &["opening the browser"],
            Ending::Running,
        )
        .await;
        let journal = Arc::new(opengrok_server::agui::routes::StoreJournal {
            state: h.state.clone(),
            thread_id: thread.clone(),
            account_id: Some(account.clone()),
            coworker_id: Some(CoworkerId::from_stored("cw_stop_test")),
            model: Some("oag/cheap".to_string()),
            system: None,
            skill_id: None,
        });

        let writes: Vec<_> = (0..3)
            .map(|round| {
                let journal = journal.clone();
                let run_id = run_id.as_str().to_string();
                tokio::spawn(async move {
                    let frame = Event::new(EventType::TextMessageContent, now_ms())
                        .with("messageId", "msg-1")
                        .with("delta", format!("round {round}"));
                    journal.record(&run_id, &[frame]).await
                })
            })
            .collect();
        let (status, body) = h.stop(&access, run_id.as_str()).await;
        assert_eq!(status, 202, "{body}");
        for write in writes {
            write
                .await
                .expect("join")
                .expect("a round that lost the race is written again, not dropped");
        }

        let (run, _) = h.store.load_run(&run_id).await.expect("load");
        assert_eq!(run.status, RunStatus::Stopped, "attempt {attempt}");
        for round in 0..3 {
            let wanted = format!("round {round}");
            let landed = run
                .emitted
                .iter()
                .filter(|frame| frame.get("delta").and_then(Value::as_str) == Some(wanted.as_str()))
                .count();
            assert_eq!(
                landed, 1,
                "round {round} lands exactly once on attempt {attempt}: {:?}",
                run.emitted
            );
        }
    }
}

/// A PARK THAT FINDS ITS RUN ENDED WRITES NOTHING (the peer review's trace). A Stop that commits
/// between the close's `stopped` question and the park's write used to leave the card's frames in
/// the log with no `Suspended` — the refusal was dropped and the write said Ok, so the client got
/// a card whose answer was a 409. The same for a run the sweep failed, where even the frame is
/// refused.
#[tokio::test]
async fn a_park_written_after_the_run_ended_is_refused_whole() {
    use opengrok_harness::{JournalError, RunJournal};
    use opengrok_wire::agui::{Event, EventType};

    let database_url = database_or_skip!();
    let email = format!("park-race-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let (account, _) = h.person(&email).await;

    for ending in [Ending::Stopped, Ending::Failed("interrupted by a restart")] {
        let thread = format!("th-{}", uuid::Uuid::now_v7().simple());
        let run_id = seed_run(
            &h.store,
            &account,
            &thread,
            now_ms(),
            &["opening the browser"],
            ending,
        )
        .await;
        let (before, _) = h.store.load_run(&run_id).await.expect("load");
        let journal = opengrok_server::agui::routes::StoreJournal {
            state: h.state.clone(),
            thread_id: thread.clone(),
            account_id: Some(account.clone()),
            coworker_id: Some(CoworkerId::from_stored("cw_stop_test")),
            model: Some("oag/cheap".to_string()),
            system: None,
            skill_id: None,
        };
        let round = [
            Event::new(EventType::ToolCallStart, now_ms())
                .with("toolCallId", "call-1")
                .with("toolCallName", "shell"),
            Event::new(EventType::Custom, now_ms())
                .with("name", "run-awaiting-approval")
                .with("callId", "call-1")
                .with("tool", "shell")
                .with("arguments", json!({"command": "ls"})),
            Event::new(EventType::RunFinished, now_ms()),
        ];
        let written = journal.record(run_id.as_str(), &round).await;
        assert!(
            matches!(written, Err(JournalError::Ended(_))),
            "{:?}: {written:?}",
            before.status
        );
        let (after, _) = h.store.load_run(&run_id).await.expect("load");
        assert_eq!(after.status, before.status);
        assert_eq!(
            after.emitted.len(),
            before.emitted.len(),
            "nothing of the park is written: {:?}",
            after.emitted
        );
    }
}
