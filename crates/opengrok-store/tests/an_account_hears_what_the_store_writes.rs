//! What `append_run` and `append_schedule` leave on the account's events stream
//! (`opengrok-events`), read straight from the outbox: which notes, for whom, in what words.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use opengrok_core::id::{AccountId, CoworkerId, MonitorId, RunId, ScheduleId};
use opengrok_core::monitor::{Monitor, MonitorCommand};
use opengrok_core::run::{Run, RunCommand, RunEvent, RunView, StartedTool, SuspendReason};
use opengrok_core::schedule::{
    FireCause, FiringBot, Schedule, ScheduleCommand, ScheduleEvent, Skip, Wake,
};
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

async fn store(url: &str) -> PgStore {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(url)
        .await
        .expect("connect");
    opengrok_store::migrations::run(&pool).await.expect("boot");
    PgStore::new(pool)
}

/// An account's notes, oldest first, as `(event, data)`.
async fn heard(store: &PgStore, account: &AccountId) -> Vec<(String, Value)> {
    let select = "select kind, payload from account_event where account_id = $1 order by id";
    sqlx::query_as(select)
        .bind(account.as_str())
        .fetch_all(store.pool())
        .await
        .unwrap()
}

/// The notes a step left, taking them off the front of what has been heard so far.
async fn since(store: &PgStore, account: &AccountId, seen: &mut usize) -> Vec<(String, Value)> {
    let all = heard(store, account).await;
    let fresh = all[*seen..].to_vec();
    *seen = all.len();
    fresh
}

/// Of the notes a step left, the data of its `thread.changed`.
async fn changed_since(store: &PgStore, account: &AccountId, seen: &mut usize) -> Vec<Value> {
    let told = since(store, account, seen).await;
    let changed = told
        .into_iter()
        .filter(|(event, _)| event == "thread.changed");
    changed.map(|(_, data)| data).collect()
}

fn note(event: &str, data: Value) -> (String, Value) {
    (event.to_string(), data)
}

/// One run's log, written through the aggregate as the server does.
struct Journal {
    store: PgStore,
    id: RunId,
    thread: String,
    run: Run,
    seq: i64,
}

impl Journal {
    fn on(store: &PgStore, thread: &str) -> Self {
        Self {
            store: store.clone(),
            id: RunId::new(),
            thread: thread.to_string(),
            run: Run::default(),
            seq: 0,
        }
    }

    /// Decide `command` and append what it decided, for `owner` (`None` is a caller with no
    /// session, as the sweep is): one write for one command, as a person's answer or stop is.
    async fn append(&mut self, command: RunCommand, owner: Option<&AccountId>) {
        self.round(vec![command], owner).await;
    }

    /// Decide `commands` one after the other and append all they decided in ONE write, as the
    /// run's own loop writes a round: its frames, its card or its ending together.
    async fn round(&mut self, commands: Vec<RunCommand>, owner: Option<&AccountId>) {
        let mut events = Vec::new();
        for command in commands {
            let decided = self.run.decide(command).unwrap();
            decided.iter().for_each(|event| self.run.apply(event));
            events.extend(decided);
        }
        self.write(&events, owner).await;
    }

    /// Append `events` as they are, without deciding them: the log's own bookkeeping, say.
    async fn write(&mut self, events: &[RunEvent], owner: Option<&AccountId>) {
        let view = RunView {
            id: self.id.clone(),
            thread_id: self.thread.clone(),
            status: self.run.status,
            event_count: self.run.emitted.len() as i64,
            updated_at_ms: 1,
        };
        let appended = self
            .store
            .append_run(&self.id, self.seq, events, &view, owner);
        self.seq = appended.await.unwrap();
    }

    async fn start(&mut self, coworker: Option<&CoworkerId>, owner: Option<&AccountId>) {
        let start = RunCommand::Start {
            thread_id: self.thread.clone(),
            coworker_id: coworker.cloned(),
            model: None,
            effort: Default::default(),
            inference_source: Default::default(),
            system: None,
            skill_id: None,
            offered_skills: Vec::new(),
            prompt: None,
            limits: Default::default(),
            at_ms: 1,
        };
        self.append(start, owner).await;
    }

    fn frame(payload: Value) -> RunCommand {
        RunCommand::Emit { payload, at_ms: 2 }
    }

    /// A round of the run's own loop: a frame.
    async fn say(&mut self, owner: Option<&AccountId>) {
        let words = json!({ "type": "TEXT_MESSAGE_CONTENT", "delta": "words that must not leak" });
        self.round(vec![Self::frame(words)], owner).await;
    }

    /// The loop's last round: its `RUN_FINISHED` frame and the ending, in one write.
    async fn finish(&mut self, owner: Option<&AccountId>) {
        let closing = Self::frame(json!({ "type": "RUN_FINISHED" }));
        let finish = RunCommand::Finish {
            at_ms: 3,
            reason: None,
        };
        self.round(vec![closing, finish], owner).await;
    }
}

fn run_started(run: &Journal, coworker: &CoworkerId, routine: Option<&str>, cause: &str) -> Value {
    let mut data = json!({ "runId": run.id.as_str(), "threadId": run.thread,
                           "coworkerId": coworker.as_str(), "cause": cause });
    if let Some(routine) = routine {
        data["routineId"] = json!(routine);
    }
    data
}

/// A `thread.changed` the run's own loop caused: it names the run.
fn thread_changed(run: &Journal, coworker: &CoworkerId) -> Value {
    json!({ "threadId": run.thread, "coworkerId": coworker.as_str(), "runId": run.id.as_str() })
}

/// One no run's commit caused: `runId` is there, and null.
fn thread_settled(run: &Journal, coworker: &CoworkerId) -> Value {
    json!({ "threadId": run.thread, "coworkerId": coworker.as_str(), "runId": null })
}

/// The note a park leaves: the ids and the run's own word for what its card asks, in the order
/// the contract lists them.
fn run_waiting(run: &Journal, coworker: &CoworkerId, reason: &str) -> Value {
    json!({ "runId": run.id.as_str(), "threadId": run.thread, "coworkerId": coworker.as_str(),
            "reason": reason })
}

/// What the run's own loop writes to raise a card: its frame and the suspension, together. The
/// words on the card are in the frame and the call's arguments, and none may reach the stream.
fn card(call: &str, reason: SuspendReason) -> Vec<RunCommand> {
    let frame = json!({ "type": "CUSTOM", "name": "run-awaiting-approval", "callId": call,
                        "why": "words only the card may carry" });
    let park = RunCommand::Suspend {
        call_id: call.into(),
        tool: "shell".into(),
        arguments: json!({ "command": "arguments only the card may carry" }),
        reason,
        at_ms: 3,
    };
    vec![Journal::frame(frame), park]
}

/// A CHAT TURN IS TOLD AS IT BEGINS, GOES ON AND ENDS, and no more than that: its first batch is
/// `run.started` and `thread.changed`, each round after is a `thread.changed`, a card is a
/// `run.waiting` beside it and not an ending, and the end is `thread.changed` and then
/// `run.finished` with the history's word.
#[tokio::test]
async fn a_chat_turn_is_told_as_it_begins_goes_on_and_ends() {
    let url = database_or_skip!();
    let store = store(&url).await;
    let (ada, luna) = (AccountId::new(), CoworkerId::new());
    let mut seen = 0;
    let mut run = Journal::on(&store, "thread-one");

    run.start(Some(&luna), Some(&ada)).await;
    assert_eq!(
        since(&store, &ada, &mut seen).await,
        [
            note("run.started", run_started(&run, &luna, None, "chat")),
            note("thread.changed", thread_changed(&run, &luna)),
        ]
    );

    run.say(Some(&ada)).await;
    let changed = note("thread.changed", thread_changed(&run, &luna));
    assert_eq!(since(&store, &ada, &mut seen).await, vec![changed.clone()]);

    // A card: the run is waiting on a person, which is a change to the thread, and the run says
    // so; it is not an ending. The loop writes the card's frame and the suspension together.
    run.round(card("call_1", SuspendReason::ExecConsent), Some(&ada))
        .await;
    let waiting = note("run.waiting", run_waiting(&run, &luna, "exec-consent"));
    assert_eq!(
        since(&store, &ada, &mut seen).await,
        vec![changed.clone(), waiting]
    );

    // The person settles it: no run's commit, so the change names none.
    let answer = RunCommand::Answer {
        call_id: "call_1".into(),
        approved: true,
        by: ada.to_string(),
        at_ms: 4,
    };
    run.append(answer, Some(&ada)).await;
    let settled = note("thread.changed", thread_settled(&run, &luna));
    assert_eq!(since(&store, &ada, &mut seen).await, vec![settled]);

    run.finish(Some(&ada)).await;
    let mut ended = run_started(&run, &luna, None, "chat");
    ended.as_object_mut().unwrap().remove("cause");
    ended["state"] = json!("ok");
    assert_eq!(
        since(&store, &ada, &mut seen).await,
        [changed, note("run.finished", ended)]
    );
}

/// WHOSE COMMIT A CHANGE WAS. The run's own loop names the run on everything it writes (its start,
/// its frames, its card, its failure); a person's answer or stop, and the sweep's resume and its
/// failing of a run it lost, name none, and say so with a `null`. The app that streams a run reads
/// nothing for its own, so a change it must read can never carry the run it is streaming.
#[tokio::test]
async fn a_change_names_the_run_whose_commit_it_was_and_a_persons_names_none() {
    let url = database_or_skip!();
    let store = store(&url).await;
    let (ada, luna) = (AccountId::new(), CoworkerId::new());
    let mut seen = 0;

    // Its loop's start, a frame, and a failure that ends it with the frame that says so.
    let mut run = Journal::on(&store, "thread-own");
    run.start(Some(&luna), Some(&ada)).await;
    let own = vec![thread_changed(&run, &luna)];
    assert_eq!(changed_since(&store, &ada, &mut seen).await, own);
    run.say(Some(&ada)).await;
    assert_eq!(changed_since(&store, &ada, &mut seen).await, own);
    let error = Journal::frame(json!({ "type": "RUN_ERROR", "message": "no model" }));
    let fail = RunCommand::Fail {
        reason: "no model".into(),
        at_ms: 3,
    };
    run.round(vec![error, fail], Some(&ada)).await;
    assert_eq!(changed_since(&store, &ada, &mut seen).await, own);

    // A person's stop of a run that is going, and of a parked run (which writes the frames that
    // say so beside it): the person's, whatever rides with it.
    for parked in [false, true] {
        let mut run = Journal::on(&store, "thread-stopped");
        run.start(Some(&luna), Some(&ada)).await;
        since(&store, &ada, &mut seen).await;
        let stop = RunCommand::Stop {
            by: ada.to_string(),
            at_ms: 3,
        };
        let mut commands = vec![stop];
        if parked {
            commands.insert(0, Journal::frame(json!({ "type": "RUN_FINISHED" })));
        }
        run.round(commands, Some(&ada)).await;
        let settled = vec![thread_settled(&run, &luna)];
        assert_eq!(changed_since(&store, &ada, &mut seen).await, settled);
    }

    // The sweep carries a run on, and fails one it could not: no session, and no run's commit.
    let mut run = Journal::on(&store, "thread-swept");
    run.start(Some(&luna), Some(&ada)).await;
    since(&store, &ada, &mut seen).await;
    let resume = RunCommand::Resume {
        reason: "interrupted by a restart".into(),
        at_ms: 3,
    };
    run.append(resume, None).await;
    let settled = vec![thread_settled(&run, &luna)];
    assert_eq!(changed_since(&store, &ada, &mut seen).await, settled);
    let lost = RunCommand::Fail {
        reason: "could not be carried on".into(),
        at_ms: 4,
    };
    run.append(lost, None).await;
    assert_eq!(changed_since(&store, &ada, &mut seen).await, settled);
}

/// A run that failed and a run that was stopped both end in the history's `error`.
#[tokio::test]
async fn a_failed_run_and_a_stopped_one_end_in_error() {
    let url = database_or_skip!();
    let store = store(&url).await;
    let (ada, luna) = (AccountId::new(), CoworkerId::new());
    let mut seen = 0;
    for (end, thread) in [
        (
            RunCommand::Fail {
                reason: "no model".into(),
                at_ms: 3,
            },
            "failed",
        ),
        (
            RunCommand::Stop {
                by: ada.to_string(),
                at_ms: 3,
            },
            "stopped",
        ),
    ] {
        let mut run = Journal::on(&store, thread);
        run.start(Some(&luna), Some(&ada)).await;
        since(&store, &ada, &mut seen).await;
        run.append(end, Some(&ada)).await;
        let told = since(&store, &ada, &mut seen).await;
        let finished = told.iter().find(|(event, _)| event == "run.finished");
        let (_, data) = finished.expect("run.finished");
        assert_eq!(data["state"], json!("error"), "{thread}");
        assert!(data.get("cause").is_none(), "{data}");
    }
}

/// A RUN THAT PARKS ON A CARD SAYS IT IS WAITING, in ids and the run's own word for what the card
/// asks (the one `GET /ag-ui/approvals` says), for each of the four kinds of card there are. Once
/// for the park, after the thread's change, and not a word of the card or of the call's arguments
/// is in it.
#[tokio::test]
async fn a_run_that_parks_on_a_card_says_it_is_waiting_in_the_words_of_its_card() {
    let url = database_or_skip!();
    let store = store(&url).await;
    let (ada, luna) = (AccountId::new(), CoworkerId::new());
    let mut seen = 0;
    for (reason, word) in [
        (SuspendReason::ExecConsent, "exec-consent"),
        (SuspendReason::PolicyApproval, "policy-approval"),
        (SuspendReason::AutoReview, "auto-review"),
        (SuspendReason::UserForm, "user-form"),
    ] {
        let mut run = Journal::on(&store, reason.as_str());
        run.start(Some(&luna), Some(&ada)).await;
        since(&store, &ada, &mut seen).await;

        run.round(card("call_1", reason), Some(&ada)).await;
        let told = since(&store, &ada, &mut seen).await;
        let waiting = note("run.waiting", run_waiting(&run, &luna, word));
        let changed = note("thread.changed", thread_changed(&run, &luna));
        assert_eq!(told, [changed, waiting], "{reason:?}");
        let said = told[1].1.to_string();
        for card_text in ["only the card may carry", "shell", "call_1"] {
            assert!(!said.contains(card_text), "{card_text:?} is in {said}");
        }
    }
}

/// A RUN THAT PARKS AGAIN SAYS SO AGAIN, AND GOING ON SAYS NOTHING. The person's answer, the
/// frames that follow it and the sweep's carrying a run on are not parks: each is a change to the
/// thread and no more, and only the batch that parks the run again is told as one.
#[tokio::test]
async fn a_run_that_parks_again_says_so_again_and_going_on_says_nothing() {
    let url = database_or_skip!();
    let store = store(&url).await;
    let (ada, luna) = (AccountId::new(), CoworkerId::new());
    let mut seen = 0;
    let mut run = Journal::on(&store, "thread-twice");
    run.start(Some(&luna), Some(&ada)).await;
    since(&store, &ada, &mut seen).await;
    let (own, settled) = (thread_changed(&run, &luna), thread_settled(&run, &luna));
    let waiting = |run: &Journal, word| note("run.waiting", run_waiting(run, &luna, word));

    run.round(card("call_1", SuspendReason::PolicyApproval), Some(&ada))
        .await;
    let first = since(&store, &ada, &mut seen).await;
    assert_eq!(first[1], waiting(&run, "policy-approval"));

    // The person says yes: the run is back to running, and the stream says the thread changed.
    let yes = RunCommand::Answer {
        call_id: "call_1".into(),
        approved: true,
        by: ada.to_string(),
        at_ms: 4,
    };
    run.append(yes, Some(&ada)).await;
    assert_eq!(
        since(&store, &ada, &mut seen).await,
        [note("thread.changed", settled.clone())]
    );
    // What the run does with the answer, and the sweep carrying it on after a restart.
    run.say(Some(&ada)).await;
    let resume = RunCommand::Resume {
        reason: "interrupted by a restart".into(),
        at_ms: 5,
    };
    run.append(resume, None).await;
    assert_eq!(
        since(&store, &ada, &mut seen).await,
        [
            note("thread.changed", own.clone()),
            note("thread.changed", settled.clone())
        ]
    );

    // It parks again, on a form this time: told again.
    run.round(card("call_2", SuspendReason::UserForm), Some(&ada))
        .await;
    let second = since(&store, &ada, &mut seen).await;
    let again = waiting(&run, "user-form");
    assert_eq!(second, [note("thread.changed", own), again]);

    let no = RunCommand::Answer {
        call_id: "call_2".into(),
        approved: false,
        by: ada.to_string(),
        at_ms: 6,
    };
    run.append(no, Some(&ada)).await;
    run.finish(Some(&ada)).await;
    let all = heard(&store, &ada).await;
    let waits: Vec<&Value> = all
        .iter()
        .filter(|(event, _)| event == "run.waiting")
        .map(|(_, data)| data)
        .collect();
    let one = run_waiting(&run, &luna, "policy-approval");
    let two = run_waiting(&run, &luna, "user-form");
    assert_eq!(waits, [&one, &two], "two parks, two notes, in order");
}

/// WAITING IS WHAT A BATCH LEAVES THE RUN AS. A park that rode with an ending left nothing to
/// answer; a frame written to a run already waiting parked nothing; and a round that stacks cards
/// is one park, told with the reason of the call the run now waits on.
#[tokio::test]
async fn only_a_batch_that_leaves_the_run_waiting_says_so() {
    let url = database_or_skip!();
    let store = store(&url).await;
    let (ada, luna) = (AccountId::new(), CoworkerId::new());
    let mut seen = 0;

    // A park and a failure in one write: the run is over, and that is all the stream says.
    let mut run = Journal::on(&store, "thread-parked-and-lost");
    run.start(Some(&luna), Some(&ada)).await;
    since(&store, &ada, &mut seen).await;
    let mut commands = card("call_1", SuspendReason::ExecConsent);
    commands.push(RunCommand::Fail {
        reason: "the box went away".into(),
        at_ms: 4,
    });
    run.round(commands, Some(&ada)).await;
    let told = since(&store, &ada, &mut seen).await;
    let words: Vec<&str> = told.iter().map(|(event, _)| event.as_str()).collect();
    assert_eq!(words, ["thread.changed", "run.finished"]);

    // A frame written to a run that is already waiting (a form's own, say).
    let mut run = Journal::on(&store, "thread-already-waiting");
    run.start(Some(&luna), Some(&ada)).await;
    run.round(card("call_1", SuspendReason::UserForm), Some(&ada))
        .await;
    since(&store, &ada, &mut seen).await;
    run.say(Some(&ada)).await;
    let changed = note("thread.changed", thread_changed(&run, &luna));
    assert_eq!(since(&store, &ada, &mut seen).await, [changed]);

    // Two cards in one round: one park, and the run waits on the last.
    let mut run = Journal::on(&store, "thread-stacked");
    run.start(Some(&luna), Some(&ada)).await;
    since(&store, &ada, &mut seen).await;
    let mut commands = card("call_1", SuspendReason::PolicyApproval);
    commands.extend(card("call_2", SuspendReason::UserForm));
    run.round(commands, Some(&ada)).await;
    let told = since(&store, &ada, &mut seen).await;
    let waiting = note("run.waiting", run_waiting(&run, &luna, "user-form"));
    let changed = note("thread.changed", thread_changed(&run, &luna));
    assert_eq!(told, [changed, waiting]);
}

/// A PARK IS TOLD TO THE RUN'S OWNER, whoever writes it, and to no one else; a run with no owner or
/// no coworker is told to nobody. The thread id is a Bot's chat, which two people share, and says
/// nothing about whose run it is.
#[tokio::test]
async fn a_park_is_told_to_the_runs_owner_and_to_no_one_else() {
    let url = database_or_skip!();
    let store = store(&url).await;
    let (ada, bob, luna) = (AccountId::new(), AccountId::new(), CoworkerId::new());

    let mut run = Journal::on(&store, "thread-shared");
    run.start(Some(&luna), Some(&ada)).await;
    run.round(card("call_1", SuspendReason::PolicyApproval), Some(&bob))
        .await;
    let theirs =
        |told: Vec<(String, Value)>| told.iter().filter(|(e, _)| e == "run.waiting").count();
    assert_eq!(
        theirs(heard(&store, &ada).await),
        1,
        "ada's run, bob wrote it"
    );
    assert!(
        heard(&store, &bob).await.is_empty(),
        "bob took nothing by writing"
    );

    let mut bobs = Journal::on(&store, "thread-shared");
    bobs.start(Some(&luna), Some(&bob)).await;
    bobs.round(card("call_1", SuspendReason::UserForm), Some(&bob))
        .await;
    assert_eq!(theirs(heard(&store, &bob).await), 1);
    assert_eq!(theirs(heard(&store, &ada).await), 1, "and nothing of bob's");

    // Told to nobody, not even to an account that is not there: no note names these runs at all.
    let nobody = AccountId::new();
    let mut unowned = Journal::on(&store, "thread-unowned");
    unowned.start(Some(&luna), None).await;
    unowned
        .round(card("call_1", SuspendReason::UserForm), None)
        .await;
    let mut unplaced = Journal::on(&store, "thread-unplaced");
    unplaced.start(None, Some(&nobody)).await;
    unplaced
        .round(card("call_1", SuspendReason::UserForm), Some(&nobody))
        .await;
    let anywhere = "select count(*) from account_event where payload->>'runId' = $1";
    for run in [&unowned, &unplaced] {
        let told: i64 = sqlx::query_scalar(anywhere)
            .bind(run.id.as_str())
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(told, 0, "{}", run.thread);
    }
    assert!(heard(&store, &nobody).await.is_empty());
}

/// THE LOG'S OWN BOOKKEEPING IS NOT A CHANGE: a round's `ToolStarted` and `Spent` are written
/// apart from its frames, and the app has nothing to read again for either.
#[tokio::test]
async fn bookkeeping_alone_tells_nobody_anything() {
    let url = database_or_skip!();
    let store = store(&url).await;
    let (ada, luna) = (AccountId::new(), CoworkerId::new());
    let mut run = Journal::on(&store, "thread-two");
    run.start(Some(&luna), Some(&ada)).await;
    let told = heard(&store, &ada).await.len();

    let tool = StartedTool {
        call_id: "call_1".into(),
        tool: "shell".into(),
    };
    let started = RunEvent::ToolStarted {
        tools: vec![tool],
        at_ms: 2,
    };
    let spent = RunEvent::Spent {
        recipes: vec!["recipe_1".into()],
        round: None,
        at_ms: 2,
    };
    run.write(&[started], Some(&ada)).await;
    run.write(std::slice::from_ref(&spent), Some(&ada)).await;
    assert_eq!(heard(&store, &ada).await.len(), told);

    // With a frame in the same write, it is a round like any other.
    let frame = RunEvent::Emitted {
        seq: 1,
        payload: json!({ "type": "TEXT_MESSAGE_CONTENT" }),
        at_ms: 3,
    };
    run.write(&[spent, frame], Some(&ada)).await;
    assert_eq!(heard(&store, &ada).await.len(), told + 1);
}

/// A RUN HAS ONE OWNER, the view's. A run started with no session is told to nobody, a later write
/// by someone else does not take it, and the sweep (which has no session) is still told to the
/// owner it finds on the view.
#[tokio::test]
async fn a_run_is_told_to_its_owner_whoever_writes() {
    let url = database_or_skip!();
    let store = store(&url).await;
    let (ada, bob, luna) = (AccountId::new(), AccountId::new(), CoworkerId::new());
    let mut run = Journal::on(&store, "thread-three");

    run.start(Some(&luna), None).await;
    run.say(None).await;
    for account in [&ada, &bob] {
        assert!(
            heard(&store, account).await.is_empty(),
            "nobody owns it yet"
        );
    }

    run.say(Some(&ada)).await;
    run.say(Some(&bob)).await;
    run.say(None).await;
    assert_eq!(
        heard(&store, &ada).await.len(),
        3,
        "ada's, however it was written"
    );
    assert!(
        heard(&store, &bob).await.is_empty(),
        "bob took nothing by writing"
    );
}

/// A run with no coworker has nowhere to be placed in an app, and says nothing.
#[tokio::test]
async fn a_run_with_no_coworker_tells_nobody() {
    let url = database_or_skip!();
    let store = store(&url).await;
    let ada = AccountId::new();
    let mut run = Journal::on(&store, "thread-four");
    run.start(None, Some(&ada)).await;
    run.say(Some(&ada)).await;
    run.finish(Some(&ada)).await;
    assert!(heard(&store, &ada).await.is_empty());
}

/// A routine, written through its aggregate; `fire` is the firing a clock, a hook, a press or a Bot
/// leaves, and the run it names is started on the routine's thread.
struct Routine {
    store: PgStore,
    id: ScheduleId,
    owner: AccountId,
    state: Schedule,
    seq: i64,
}

impl Routine {
    async fn made(store: &PgStore, owner: &AccountId, coworker: &CoworkerId) -> Self {
        let mut routine = Self {
            store: store.clone(),
            id: ScheduleId::new(),
            owner: owner.clone(),
            state: Schedule::default(),
            seq: 0,
        };
        let create = ScheduleCommand::Create {
            coworker_id: coworker.clone(),
            prompt: "write the report".into(),
            name: "Weekly".into(),
            wake: Wake::Cron {
                cron: "0 9 * * 1".into(),
            },
            run_limits: Default::default(),
            tz: "UTC".into(),
            at_ms: 1,
        };
        routine.append(create).await;
        routine
    }

    async fn append(&mut self, command: ScheduleCommand) {
        let events = self.state.decide(command).unwrap();
        self.write(&events).await;
    }

    async fn write(&mut self, events: &[ScheduleEvent]) {
        for event in events {
            self.state.apply(event);
        }
        let appended =
            self.store
                .append_schedule(&self.id, &self.owner, self.seq, events, &self.state, 1);
        self.seq = appended.await.unwrap();
    }

    async fn fire(&mut self, run: &RunId, cause: FireCause, by: Option<FiringBot>) {
        let fire = ScheduleCommand::Fire {
            run_id: run.clone(),
            cause,
            by,
            at_ms: 2,
        };
        self.append(fire).await;
    }
}

/// A ROUTINE'S RUN SAYS WHICH ROUTINE FIRED IT AND THE WORD ITS HISTORY USES FOR WHY: `clock`,
/// `manual` (a person's Test run), `webhook` and `bot`. Started and finished both name the routine.
#[tokio::test]
async fn a_routines_run_names_its_routine_and_the_word_its_history_gives_its_cause() {
    let url = database_or_skip!();
    let store = store(&url).await;
    let (ada, luna, sol) = (AccountId::new(), CoworkerId::new(), CoworkerId::new());
    let bot = FiringBot {
        coworker_id: sol,
        name: "Sol".into(),
    };
    for (cause, by, word) in [
        (FireCause::Clock, None, "clock"),
        (FireCause::Manual, None, "manual"),
        (FireCause::Webhook, None, "webhook"),
        (FireCause::Bot, Some(bot), "bot"),
    ] {
        let mut routine = Routine::made(&store, &ada, &luna).await;
        let mut run = Journal::on(&store, routine.id.as_str());
        routine.fire(&run.id, cause, by).await;
        let mut seen = heard(&store, &ada).await.len();

        run.start(Some(&luna), Some(&ada)).await;
        let told = since(&store, &ada, &mut seen).await;
        let started = run_started(&run, &luna, Some(routine.id.as_str()), word);
        assert_eq!(told[0], note("run.started", started), "{word}");

        run.finish(Some(&ada)).await;
        let told = since(&store, &ada, &mut seen).await;
        let (event, data) = told.last().unwrap();
        assert_eq!(event, "run.finished", "{word}");
        assert_eq!(data["routineId"], json!(routine.id.as_str()), "{word}");
        assert_eq!(data["state"], json!("ok"), "{word}");
    }
}

/// A person replying in a routine's thread is no firing: the run is a `chat` and has no routine.
/// The same for a run whose firing names another run. The thread id says where to look, not what
/// the answer is.
#[tokio::test]
async fn a_reply_in_a_routines_thread_is_a_chat_and_not_the_routines_run() {
    let url = database_or_skip!();
    let store = store(&url).await;
    let (ada, luna) = (AccountId::new(), CoworkerId::new());
    let mut routine = Routine::made(&store, &ada, &luna).await;
    routine.fire(&RunId::new(), FireCause::Clock, None).await;

    let mut reply = Journal::on(&store, routine.id.as_str());
    let mut seen = heard(&store, &ada).await.len();
    reply.start(Some(&luna), Some(&ada)).await;
    let told = since(&store, &ada, &mut seen).await;
    assert_eq!(
        told[0],
        note("run.started", run_started(&reply, &luna, None, "chat"))
    );
}

/// A monitor's run is told in the monitor's history words, `event` and `manual`, and has no
/// routine: the app has no monitors.
#[tokio::test]
async fn a_monitors_run_is_told_in_the_monitors_words_with_no_routine() {
    let url = database_or_skip!();
    let store = store(&url).await;
    let (ada, luna) = (AccountId::new(), CoworkerId::new());
    let id = MonitorId::new();
    let mut monitor = Monitor::default();
    let create = MonitorCommand::Create {
        coworker_id: luna.clone(),
        watches: "run-failed".into(),
        prompt: "look".into(),
        at_ms: 1,
    };
    let events = monitor.decide(create).unwrap();
    events.iter().for_each(|event| monitor.apply(event));
    store
        .append_monitor(&id, &ada, 0, &events, &monitor, 1)
        .await
        .unwrap();
    let mut seq = 1;

    for (manual, word) in [(false, "event"), (true, "manual")] {
        let mut run = Journal::on(&store, id.as_str());
        let fire = MonitorCommand::Fire {
            run_id: run.id.clone(),
            matched_stream: "run/x".into(),
            manual,
            at_ms: 2,
        };
        let events = monitor.decide(fire).unwrap();
        events.iter().for_each(|event| monitor.apply(event));
        seq = store
            .append_monitor(&id, &ada, seq, &events, &monitor, 2)
            .await
            .unwrap();
        let mut seen = heard(&store, &ada).await.len();
        run.start(Some(&luna), Some(&ada)).await;
        let told = since(&store, &ada, &mut seen).await;
        let started = run_started(&run, &luna, None, word);
        assert_eq!(told[0], note("run.started", started), "{word}");
    }
}

fn changed(routine: &Routine, coworker: &CoworkerId, change: &str) -> (String, Value) {
    note(
        "routine.changed",
        json!({ "routineId": routine.id.as_str(), "coworkerId": coworker.as_str(),
                "change": change }),
    )
}

/// EVERY WRITE TO A ROUTINE IS A NOTE, in the contract's five words: made, edited, paused, resumed
/// and deleted say so, and a firing, a skipped firing and a rotated key are an `updated`. The
/// Bot is the one the routine is on after the write.
#[tokio::test]
async fn every_write_to_a_routine_is_told_in_five_words() {
    let url = database_or_skip!();
    let store = store(&url).await;
    let (ada, luna, sol) = (AccountId::new(), CoworkerId::new(), CoworkerId::new());
    let mut routine = Routine::made(&store, &ada, &luna).await;
    let mut seen = 0;
    assert_eq!(
        since(&store, &ada, &mut seen).await,
        [changed(&routine, &luna, "created")]
    );

    let update = ScheduleCommand::Update {
        name: "Weekly".into(),
        prompt: "write the report, shorter".into(),
        wake: Wake::Cron {
            cron: "0 10 * * 1".into(),
        },
        coworker_id: Some(sol.clone()),
        run_limits: None,
        tz: None,
        at_ms: 2,
    };
    routine.append(update).await;
    assert_eq!(
        since(&store, &ada, &mut seen).await,
        [changed(&routine, &sol, "updated")],
        "now on the Bot it was handed to"
    );

    routine.append(ScheduleCommand::Pause { at_ms: 3 }).await;
    assert_eq!(
        since(&store, &ada, &mut seen).await,
        [changed(&routine, &sol, "paused")]
    );
    routine.append(ScheduleCommand::Resume { at_ms: 4 }).await;
    assert_eq!(
        since(&store, &ada, &mut seen).await,
        [changed(&routine, &sol, "resumed")]
    );

    // A firing changes what the row shows (`lastRun`, `nextDueMs`), and so does a skipped one.
    routine.fire(&RunId::new(), FireCause::Clock, None).await;
    assert_eq!(
        since(&store, &ada, &mut seen).await,
        [changed(&routine, &sol, "updated")]
    );
    let skip = Skip {
        cause: FireCause::Clock,
        code: "relay_offline".into(),
        at_ms: 5,
        by: None,
    };
    routine.append(ScheduleCommand::Skip(skip)).await;
    assert_eq!(
        since(&store, &ada, &mut seen).await,
        [changed(&routine, &sol, "updated")]
    );

    routine.append(ScheduleCommand::Delete { at_ms: 6 }).await;
    assert_eq!(
        since(&store, &ada, &mut seen).await,
        [changed(&routine, &sol, "deleted")]
    );
}

/// A webhook routine's rotated key is an `updated` too.
#[tokio::test]
async fn a_rotated_key_is_an_update_to_the_routine() {
    let url = database_or_skip!();
    let store = store(&url).await;
    let (ada, luna) = (AccountId::new(), CoworkerId::new());
    let id = ScheduleId::new();
    let create = ScheduleCommand::Create {
        coworker_id: luna.clone(),
        prompt: "when called".into(),
        name: "Hook".into(),
        wake: Wake::Webhook {
            hook_id: format!("hook_{}", uuid::Uuid::now_v7().simple()),
            secret_hash: "hash".into(),
            webhook_key: "key".into(),
        },
        run_limits: Default::default(),
        tz: "UTC".into(),
        at_ms: 1,
    };
    let mut state = Schedule::default();
    let events = state.decide(create).unwrap();
    events.iter().for_each(|event| state.apply(event));
    let seq = store
        .append_schedule(&id, &ada, 0, &events, &state, 1)
        .await
        .unwrap();
    let rotate = ScheduleCommand::RotateWebhookSecret {
        secret_hash: "new-hash".into(),
        webhook_key: "new-key".into(),
        at_ms: 2,
    };
    let events = state.decide(rotate).unwrap();
    events.iter().for_each(|event| state.apply(event));
    store
        .append_schedule(&id, &ada, seq, &events, &state, 2)
        .await
        .unwrap();

    let told = heard(&store, &ada).await;
    let words: Vec<&str> = told
        .iter()
        .map(|(_, data)| data["change"].as_str().unwrap())
        .collect();
    assert_eq!(words, ["created", "updated"]);
}

/// A change twice over in a row is told once; different ones are told in order.
#[tokio::test]
async fn a_write_that_changes_a_routine_twice_over_is_told_once() {
    let url = database_or_skip!();
    let store = store(&url).await;
    let (ada, luna) = (AccountId::new(), CoworkerId::new());
    let mut routine = Routine::made(&store, &ada, &luna).await;
    let mut seen = heard(&store, &ada).await.len();

    let fired = |n: u32| ScheduleEvent::Fired {
        run_id: RunId::from_stored(format!("run_{n}")),
        manual: false,
        webhook: false,
        by: None,
        at_ms: 2,
    };
    let rotated = ScheduleEvent::SecretRotated {
        secret_hash: "h".into(),
        webhook_key: "k".into(),
        at_ms: 2,
    };
    routine
        .write(&[
            fired(1),
            rotated,
            ScheduleEvent::Paused { at_ms: 3 },
            fired(2),
        ])
        .await;
    let told = since(&store, &ada, &mut seen).await;
    let words: Vec<&str> = told
        .iter()
        .map(|(_, data)| data["change"].as_str().unwrap())
        .collect();
    assert_eq!(words, ["updated", "paused", "updated"]);
}

/// A ROUTINE IS TOLD TO ITS OWNER, and to no one else, whichever Bot it is on.
#[tokio::test]
async fn a_routine_is_told_to_its_owner_and_no_one_else() {
    let url = database_or_skip!();
    let store = store(&url).await;
    let (ada, bob, shared) = (AccountId::new(), AccountId::new(), CoworkerId::new());
    Routine::made(&store, &bob, &shared).await;
    assert!(heard(&store, &ada).await.is_empty());
    assert_eq!(heard(&store, &bob).await.len(), 1);
}
