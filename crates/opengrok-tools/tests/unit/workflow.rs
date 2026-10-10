use std::collections::VecDeque;
use std::sync::Mutex;

use opengrok_box::{BoxError, BoxResult, CommandOutput, StartedCommand};

use super::*;

// ---- stands-in for the three things a walk touches -------------------------------------

/// A box that says what a test told it to say, and remembers what it was asked.
#[derive(Default)]
struct StubBox {
    /// What each probe answers, in order. When they run out the last repeats.
    says: Mutex<VecDeque<String>>,
    /// Appends the call number to every probe's output, so no two probes agree — what keeps a
    /// loop test measuring the step budget rather than the standing-still breaker.
    ever_changing: bool,
    /// What each recipe run answers, in order. When they run out the last repeats.
    receipts: Mutex<VecDeque<Value>>,
    /// The box is out of reach for a recipe.
    refuses: bool,
    /// Every probe takes this long. Virtual under a paused clock.
    slow_ms: u64,
    commands: Mutex<Vec<String>>,
    /// Every recipe body the walk sent, whole — what the walk ASKED the box, as against what
    /// the box answered.
    recipe_requests: Mutex<Vec<Value>>,
    calls: Mutex<usize>,
}

impl StubBox {
    fn saying(lines: &[&str]) -> Self {
        Self {
            says: Mutex::new(lines.iter().map(|line| (*line).to_string()).collect()),
            ..Self::default()
        }
    }

    fn with_receipts(mut self, receipts: Vec<Value>) -> Self {
        self.receipts = Mutex::new(receipts.into());
        self
    }

    fn ran(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }

    fn recipe_requests(&self) -> Vec<Value> {
        self.recipe_requests.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Computer for StubBox {
    async fn create(&self, _ttl_seconds: Option<u64>) -> BoxResult<String> {
        Ok("bx_stub".to_string())
    }
    async fn run(
        &self,
        _box_id: &str,
        command: &str,
        _timeout_seconds: u32,
    ) -> BoxResult<CommandOutput> {
        self.commands.lock().unwrap().push(command.to_string());
        let nth = {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            *calls
        };
        if self.slow_ms > 0 {
            tokio::time::sleep(Duration::from_millis(self.slow_ms)).await;
        }
        let mut says = self.says.lock().unwrap();
        let line = if says.len() > 1 {
            says.pop_front().unwrap_or_default()
        } else {
            says.front().cloned().unwrap_or_default()
        };
        let stdout = if self.ever_changing {
            format!("{line} {nth}")
        } else {
            line
        };
        Ok(CommandOutput {
            exit_code: 0,
            stdout,
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            timed_out: false,
        })
    }
    async fn start(&self, _box_id: &str, _command: &str) -> BoxResult<StartedCommand> {
        Ok(StartedCommand {
            process_id: "p".to_string(),
            running: false,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
        })
    }
    async fn watch(&self, _box_id: &str, _process_id: &str) -> BoxResult<StartedCommand> {
        self.start("", "").await
    }
    async fn read_file(&self, _box_id: &str, _path: &str) -> BoxResult<String> {
        Ok(String::new())
    }
    async fn write_file(&self, _box_id: &str, _path: &str, _content: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn expose_port(&self, _box_id: &str, _port: u16, _title: &str) -> BoxResult<String> {
        Ok("http://stub.invalid".to_string())
    }
    async fn stop(&self, _box_id: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn resume(&self, _box_id: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn destroy(&self, _box_id: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn state(&self, _box_id: &str) -> BoxResult<String> {
        Ok("running".to_string())
    }
    async fn run_recipe(
        &self,
        _box_id: &str,
        _screen: &opengrok_box::Screen,
        request: &Value,
    ) -> BoxResult<Value> {
        self.commands
            .lock()
            .unwrap()
            .push(format!("recipe {}", request["name"]));
        self.recipe_requests.lock().unwrap().push(request.clone());
        if self.refuses {
            return Err(BoxError::Unreachable("no route to the box".to_string()));
        }
        let mut receipts = self.receipts.lock().unwrap();
        Ok(if receipts.len() > 1 {
            receipts.pop_front().unwrap_or_else(|| json!({"ok": true}))
        } else {
            receipts
                .front()
                .cloned()
                .unwrap_or_else(|| json!({"ok": true, "ran": 3}))
        })
    }
}

#[derive(Default)]
struct StubRecipes {
    asked: Mutex<Vec<(String, Values)>>,
}

#[async_trait::async_trait]
impl RecipeSource for StubRecipes {
    async fn recipe_request(
        &self,
        recipe_id: &str,
        values: &Values,
    ) -> Result<(i32, Value), String> {
        self.asked
            .lock()
            .unwrap()
            .push((recipe_id.to_string(), values.clone()));
        if recipe_id == "rcp_gone" {
            return Err(format!("recipe `{recipe_id}` is gone"));
        }
        Ok((7, json!({ "name": recipe_id, "steps": [] })))
    }
    async fn record_run(
        &self,
        recipe_id: &str,
        _version: i32,
        _by: &CoworkerId,
        _receipt: &RecipeReceipt,
        _claimed: Option<&str>,
    ) -> Option<String> {
        Some(format!("rrun_{recipe_id}"))
    }
}

/// A judge that answers, or fails, the same way every time — and remembers being asked.
struct StubJudge {
    answer: Result<Verdict, JudgeError>,
    asked: Mutex<Vec<String>>,
}

impl StubJudge {
    fn saying(answer: &str, confidence: f64) -> Self {
        Self {
            answer: Ok(Verdict {
                answer: answer.to_string(),
                confidence,
            }),
            asked: Mutex::new(Vec::new()),
        }
    }
    fn failing(error: JudgeError) -> Self {
        Self {
            answer: Err(error),
            asked: Mutex::new(Vec::new()),
        }
    }
    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Judge for StubJudge {
    async fn judge(&self, ask: JudgeAsk<'_>) -> Result<Verdict, JudgeError> {
        self.asked.lock().unwrap().push(format!(
            "{}:{}",
            ask.name,
            ask.state["at"].as_str().unwrap_or("?")
        ));
        self.answer.clone()
    }
}

fn workflow(body: Value) -> Workflow {
    Workflow::parse(&body).expect("a body the tests wrote should parse")
}

fn bot() -> CoworkerId {
    CoworkerId::from_stored("cw_test".to_string())
}

async fn walk_with(
    body: Value,
    computer: &StubBox,
    recipes: &StubRecipes,
    allowed: &[&str],
    judging: Judging<'_>,
) -> Walk {
    let allowed: BTreeSet<String> = allowed.iter().map(|id| (*id).to_string()).collect();
    let bot = bot();
    let walker = Walker {
        computer,
        box_id: "bx_stub",
        screen: &opengrok_box::Screen::Shared,
        recipes,
        coworker: &bot,
        allowed: &allowed,
        judging,
        name: "Search once",
        // Named rather than taken from the deployment, so the suite says what it runs at
        // instead of inheriting whatever `OG_RECIPE_OBSERVE` happens to hold.
        observe: crate::observe::Observe::Input,
    };
    walker.walk(&workflow(body), &Values::new()).await
}

// ---- the body -------------------------------------------------------------------------

#[test]
fn a_body_has_to_say_which_shape_it_is_written_in() {
    let no_shape = Workflow::parse(&json!({
        "start": "done", "steps": { "done": { "do": "stop", "outcome": "done" } }
    }))
    .expect_err("a body with no shape number is refused");
    assert!(no_shape.contains("\"workflow\": 1"), "{no_shape}");

    let later = Workflow::parse(&json!({
        "workflow": 9, "start": "done",
        "steps": { "done": { "do": "stop", "outcome": "done" } }
    }))
    .expect_err("a shape from the future is refused");
    assert!(later.contains("shape 9"), "{later}");
}

#[test]
fn a_body_round_trips_through_the_shape_it_is_stored_in() {
    let body = json!({
        "workflow": 1,
        "start": "look",
        "budget": { "steps": 12, "seconds": 60 },
        "steps": {
            "look": { "do": "observe", "as": "windows", "shell": "wmctrl -lx", "then": "here" },
            "here": { "do": "when", "fact": "windows", "test": { "contains": "chrome" },
                      "yes": "done", "no": "done" },
            "done": { "do": "stop", "outcome": "done", "say": "looked" }
        }
    });
    let parsed = workflow(body);
    let again = Workflow::parse(&parsed.to_body()).expect("round trip");
    assert_eq!(parsed, again);
    assert_eq!(
        parsed.budget,
        Budget {
            steps: 12,
            seconds: 60
        }
    );
}

#[test]
fn a_budget_a_body_asks_for_is_clamped_rather_than_refused() {
    let parsed = workflow(json!({
        "workflow": 1, "start": "done", "budget": { "steps": 100000, "seconds": 0 },
        "steps": { "done": { "do": "stop", "outcome": "done" } }
    }));
    assert_eq!(parsed.budget.steps, Budget::MAX_STEPS);
    assert_eq!(parsed.budget.seconds, 1);
}

#[test]
fn a_jump_to_a_step_that_is_not_there_is_refused_by_name() {
    let why = Workflow::parse(&json!({
        "workflow": 1, "start": "a",
        "steps": {
            "a": { "do": "run", "recipe": "rcp_1", "then": "b" },
            "done": { "do": "stop", "outcome": "done" }
        }
    }))
    .expect_err("a dangling jump is refused");
    assert!(why.contains("\"a\" goes to \"b\""), "{why}");
}

#[test]
fn a_tree_that_could_never_end_is_refused_before_it_is_stored() {
    let why = Workflow::parse(&json!({
        "workflow": 1, "start": "a",
        "steps": {
            "a": { "do": "run", "recipe": "rcp_1", "then": "b" },
            "b": { "do": "run", "recipe": "rcp_1", "then": "a" },
            // Reachable from nowhere, so it does not rescue the tree.
            "done": { "do": "stop", "outcome": "done" }
        }
    }))
    .expect_err("a tree with no reachable stop is refused");
    assert!(why.contains("could never end"), "{why}");
}

#[test]
fn a_question_must_say_where_every_answer_it_offers_goes() {
    let why = Workflow::parse(&json!({
        "workflow": 1, "start": "a",
        "steps": {
            "a": { "do": "ask", "name": "tone",
                   "question": { "kind": "choice", "instructions": "How did it go?",
                                 "choices": ["fine", "badly"] },
                   "go": { "fine": "done" } },
            "done": { "do": "stop", "outcome": "done" }
        }
    }))
    .expect_err("an unrouted answer is refused");
    assert!(why.contains("\"badly\""), "{why}");

    let one = Workflow::parse(&json!({
        "workflow": 1, "start": "a",
        "steps": {
            "a": { "do": "ask", "name": "tone",
                   "question": { "kind": "choice", "instructions": "How did it go?",
                                 "choices": ["fine"] },
                   "go": { "fine": "done" } },
            "done": { "do": "stop", "outcome": "done" }
        }
    }))
    .expect_err("a choice of one is refused");
    assert!(one.contains("at least two choices"), "{one}");
}

#[test]
fn a_yes_or_no_question_uses_yes_and_no_and_nothing_else() {
    let why = Workflow::parse(&json!({
        "workflow": 1, "start": "a",
        "steps": {
            "a": { "do": "ask", "name": "empty",
                   "question": { "kind": "noul", "instructions": "Is it empty?" },
                   "go": { "yes": "done" } },
            "done": { "do": "stop", "outcome": "done" }
        }
    }))
    .expect_err("a noul with a `go` map is refused");
    assert!(why.contains("`yes` and `no`"), "{why}");
}

// ---- termination ----------------------------------------------------------------------

/// A loop whose facts change every time round, so only the step budget can end it.
fn a_loop_that_never_repeats_itself() -> Value {
    json!({
        "workflow": 1, "start": "look", "budget": { "steps": 5, "seconds": 600 },
        "steps": {
            "look": { "do": "observe", "as": "seen", "shell": "wmctrl -lx", "then": "again" },
            "again": { "do": "when", "fact": "seen", "test": { "contains": "never" },
                       "yes": "done", "no": "look" },
            "done": { "do": "stop", "outcome": "done" }
        }
    })
}

#[tokio::test]
async fn a_loop_runs_out_of_steps_and_says_which_step_it_was_on() {
    let computer = StubBox {
        ever_changing: true,
        says: Mutex::new(["a window".to_string()].into()),
        ..StubBox::default()
    };
    let recipes = StubRecipes::default();
    let walk = walk_with(
        a_loop_that_never_repeats_itself(),
        &computer,
        &recipes,
        &[],
        Judging::Off("no judge in this test".to_string()),
    )
    .await;
    assert_eq!(walk.steps, 5, "the budget is the number of steps taken");
    assert!(!walk.ok());
    match &walk.ending {
        Ending::OutOfSteps { at, budget } => {
            assert_eq!(*budget, 5);
            assert!(at == "look" || at == "again", "{at}");
        }
        other => panic!("expected a step budget, got {other:?}"),
    }
    // A REPORTED OUTCOME, NOT A SILENCE: the receipt names the bound and the trail is whole.
    let receipt = walk.receipt();
    assert_eq!(receipt["ending"], "out-of-steps");
    assert_eq!(receipt["ok"], false);
    assert!(
        receipt["say"]
            .as_str()
            .unwrap()
            .contains("all 5 of its steps"),
        "{receipt}"
    );
    assert_eq!(receipt["trail"].as_array().unwrap().len(), 5);
}

#[tokio::test(start_paused = true)]
async fn a_walk_with_no_time_left_starts_no_further_step() {
    // One probe of 1.2s against a one-second budget: the step that was already running is
    // allowed to finish, and nothing after it begins.
    let computer = StubBox {
        slow_ms: 1_200,
        ever_changing: true,
        says: Mutex::new(["a window".to_string()].into()),
        ..StubBox::default()
    };
    let recipes = StubRecipes::default();
    let mut body = a_loop_that_never_repeats_itself();
    body["budget"] = json!({ "steps": 50, "seconds": 1 });
    let walk = walk_with(
        body,
        &computer,
        &recipes,
        &[],
        Judging::Off("no judge in this test".to_string()),
    )
    .await;
    assert_eq!(walk.steps, 1, "the probe finished; nothing else started");
    match &walk.ending {
        Ending::OutOfTime { seconds, .. } => assert_eq!(*seconds, 1),
        other => panic!("expected the wall clock, got {other:?}"),
    }
    assert_eq!(walk.receipt()["ending"], "out-of-time");
}

#[tokio::test]
async fn coming_back_with_nothing_new_to_go_on_ends_the_walk() {
    // No probe at all, so the facts never change: the two steps hand each other the same
    // empty world for ever. This is the twenty-five-run shape, caught before it is twenty-five.
    let body = json!({
        "workflow": 1, "start": "a", "budget": { "steps": 100, "seconds": 600 },
        "steps": {
            "a": { "do": "when", "fact": "seen", "test": { "contains": "x" },
                   "yes": "done", "no": "b" },
            "b": { "do": "when", "fact": "seen", "test": { "contains": "x" },
                   "yes": "done", "no": "a" },
            "done": { "do": "stop", "outcome": "done" }
        }
    });
    let computer = StubBox::default();
    let recipes = StubRecipes::default();
    let walk = walk_with(
        body,
        &computer,
        &recipes,
        &[],
        Judging::Off("no judge in this test".to_string()),
    )
    .await;
    match &walk.ending {
        Ending::GoingInCircles { at, times } => {
            assert_eq!(*at, "a");
            assert_eq!(*times, SAME_PLACE_ALLOWED + 1);
        }
        other => panic!("expected the standing-still breaker, got {other:?}"),
    }
    assert!(walk.steps < 100, "it did not spend the whole budget first");
    assert_eq!(walk.receipt()["ending"], "going-in-circles");
}

// ---- what a condition sees ------------------------------------------------------------

#[tokio::test]
async fn a_probe_writes_the_fact_a_branch_reads_and_its_output_is_not_kept() {
    let body = json!({
        "workflow": 1, "start": "look",
        "steps": {
            "look": { "do": "observe", "as": "windows", "shell": "wmctrl -lx",
                      "seconds": 900, "then": "here" },
            "here": { "do": "when", "fact": "windows", "test": { "contains": "chrome" },
                      "yes": "found", "no": "missing" },
            "exit": { "do": "when", "fact": "windows.exit", "test": { "is": "0" },
                      "yes": "found", "no": "missing" },
            "found": { "do": "stop", "outcome": "found", "say": "chrome is up" },
            "missing": { "do": "stop", "outcome": "missing" }
        }
    });
    let computer = StubBox::saying(&["0x02 0 1234 chrome.Google-chrome laptop Inbox"]);
    let recipes = StubRecipes::default();
    let walk = walk_with(
        body,
        &computer,
        &recipes,
        &[],
        Judging::Off("no judge in this test".to_string()),
    )
    .await;
    assert!(walk.ok());
    assert_eq!(
        walk.ending,
        Ending::Stopped {
            outcome: "found".to_string(),
            say: "chrome is up".to_string()
        }
    );
    assert_eq!(computer.ran(), vec!["wmctrl -lx".to_string()]);

    let trail = walk.receipt();
    let probe = &trail["trail"][0];
    assert_eq!(probe["as"], "windows");
    assert_eq!(probe["exit"], 0);
    assert_eq!(probe["chars"], 45);
    // NOT A LEAK BACK TO THE AUTHOR. A shared workflow's runs are readable by whoever shared
    // it, so what the probe saw stays out of the record; the command, the exit code and the
    // size are what debugging needs.
    let whole = serde_json::to_string(&trail).unwrap();
    assert!(!whole.contains("Inbox"), "{whole}");
    assert_eq!(trail["trail"][1]["held"], true);
    assert_eq!(trail["trail"][1]["went"], "found");
}

#[tokio::test]
async fn a_recipes_receipt_becomes_the_facts_the_next_branch_reads() {
    let body = json!({
        "workflow": 1, "start": "play",
        "steps": {
            "play": { "do": "run", "recipe": "rcp_search", "then": "how", "otherwise": "how" },
            "how": { "do": "when", "fact": "last.ok", "test": { "is": "true" },
                     "yes": "done", "no": "sad" },
            "done": { "do": "stop", "outcome": "done" },
            "sad": { "do": "stop", "outcome": "gave-up" }
        }
    });
    let computer = StubBox::default().with_receipts(vec![json!({"ok": true, "ran": 4})]);
    let recipes = StubRecipes::default();
    let walk = walk_with(
        body,
        &computer,
        &recipes,
        &["rcp_search"],
        Judging::Off("no judge in this test".to_string()),
    )
    .await;
    assert_eq!(
        walk.ending,
        Ending::Stopped {
            outcome: "done".to_string(),
            say: String::new()
        }
    );
    let receipt = walk.receipt();
    assert_eq!(receipt["outcome"], "done");
    assert_eq!(receipt["trail"][0]["recipe"], "rcp_search");
    assert_eq!(receipt["trail"][0]["version"], 7);
    assert_eq!(receipt["trail"][0]["ran"], 4);
    // The inner run is pointed at, so a branch can be opened from the workflow's history.
    assert_eq!(receipt["trail"][0]["runId"], "rrun_rcp_search");
}

/// A receipt shaped the way the box writes one when it was asked to look.
fn observed_receipt(steps: Value) -> Value {
    json!({ "ok": true, "ran": 2, "stopped_at": null, "observe": "input", "steps": steps })
}

/// THE FACT `last.ok` COULD NOT CARRY. A tape whose coordinates have drifted onto another
/// window plays every step without error, so `ok` is true and the tree has nothing to branch
/// on. What the box saw is a different fact, and a `when` reads it like any other.
#[tokio::test]
async fn what_the_box_saw_is_a_fact_the_next_branch_can_read() {
    let body = json!({
        "workflow": 1, "start": "play",
        "steps": {
            "play": { "do": "run", "recipe": "rcp_search", "then": "where", "otherwise": "sad" },
            "where": { "do": "when", "fact": "last.targets", "test": { "contains": "xterm" },
                       "yes": "wrong-window", "no": "done" },
            "done": { "do": "stop", "outcome": "done" },
            "wrong-window": { "do": "stop", "outcome": "gave-up" },
            "sad": { "do": "stop", "outcome": "gave-up" }
        }
    });
    let computer = StubBox::default().with_receipts(vec![observed_receipt(json!([
        { "index": 0, "op": "click", "ok": true, "observed": {
            "target": { "id": "0x1", "class": "xterm.XTerm", "title": "Terminal" },
            "observe_ms": 9 } },
        { "index": 1, "op": "type", "ok": true, "observed": {
            "focus": { "state": "none" }, "observe_ms": 4 } },
    ]))]);
    let recipes = StubRecipes::default();
    let walk = walk_with(
        body,
        &computer,
        &recipes,
        &["rcp_search"],
        Judging::Off("no judge in this test".to_string()),
    )
    .await;
    // Every step played and nothing threw, and the tree still declined to call it done —
    // which is the whole of what this wire is for.
    assert_eq!(
        walk.ending,
        Ending::Stopped {
            outcome: "gave-up".to_string(),
            say: String::new()
        }
    );
    // And the walk asked for it: a level the box never received reports nothing.
    assert_eq!(
        computer
            .recipe_requests()
            .first()
            .and_then(|body| body.get("observe").cloned()),
        Some(json!("input"))
    );
}

/// `page` is the level that scales badly, so a body pays for it one step at a time rather
/// than the deployment paying for it on every run of every recipe.
#[tokio::test]
async fn one_run_step_can_pay_for_the_page_without_the_rest_of_the_walk_paying() {
    let body = json!({
        "workflow": 1, "start": "first",
        "steps": {
            "first": { "do": "run", "recipe": "rcp_search", "then": "second" },
            "second": { "do": "run", "recipe": "rcp_search", "observe": "page",
                        "then": "done" },
            "done": { "do": "stop", "outcome": "done" }
        }
    });
    let computer = StubBox::default().with_receipts(vec![
        json!({"ok": true, "ran": 1}),
        json!({"ok": true, "ran": 1}),
    ]);
    let recipes = StubRecipes::default();
    let walk = walk_with(
        body,
        &computer,
        &recipes,
        &["rcp_search"],
        Judging::Off("no judge in this test".to_string()),
    )
    .await;
    assert!(matches!(walk.ending, Ending::Stopped { .. }), "{walk:?}");
    let asked: Vec<Value> = computer
        .recipe_requests()
        .iter()
        .map(|body| body["observe"].clone())
        .collect();
    assert_eq!(asked, vec![json!("input"), json!("page")]);
}

/// A shared workflow's runs are read by whoever shared it, and a run happens on somebody
/// else's box. The record says the box looked and how much of the run it looked at; what was
/// on the screen stays in the walk that asked for it.
#[tokio::test]
async fn what_the_box_saw_is_not_written_into_the_walks_record() {
    let body = json!({
        "workflow": 1, "start": "play",
        "steps": {
            "play": { "do": "run", "recipe": "rcp_search", "then": "done" },
            "done": { "do": "stop", "outcome": "done" }
        }
    });
    let computer = StubBox::default().with_receipts(vec![observed_receipt(json!([
        { "index": 0, "op": "click", "ok": true, "observed": {
            "target": { "id": "0x1", "class": "chromium.Chromium",
                        "title": "quarterly-plan — Docs" },
            "observe_ms": 9 } },
    ]))]);
    let recipes = StubRecipes::default();
    let walk = walk_with(
        body,
        &computer,
        &recipes,
        &["rcp_search"],
        Judging::Off("no judge in this test".to_string()),
    )
    .await;
    let receipt = walk.receipt();
    let whole = serde_json::to_string(&receipt).unwrap();
    assert!(!whole.contains("quarterly-plan"), "{whole}");
    assert_eq!(receipt["trail"][0]["observe"], "input");
    assert_eq!(receipt["trail"][0]["observed"], 1);
}

/// A box too old to know the field answers without one, and the facts say so rather than
/// reading as "it looked and the desktop was empty". A tree that cares can test `last.observe`
/// before it trusts the other three.
#[tokio::test]
async fn a_box_that_never_looked_leaves_the_facts_empty_and_says_which() {
    let body = json!({
        "workflow": 1, "start": "play",
        "steps": {
            "play": { "do": "run", "recipe": "rcp_search", "then": "asked" },
            "asked": { "do": "when", "fact": "last.observe", "test": "empty",
                       "yes": "blind", "no": "done" },
            "done": { "do": "stop", "outcome": "done" },
            "blind": { "do": "stop", "outcome": "nobody-looked" }
        }
    });
    let computer = StubBox::default()
        .with_receipts(vec![json!({"ok": true, "ran": 1, "steps": [{"ok": true}]})]);
    let recipes = StubRecipes::default();
    let walk = walk_with(
        body,
        &computer,
        &recipes,
        &["rcp_search"],
        Judging::Off("no judge in this test".to_string()),
    )
    .await;
    assert_eq!(
        walk.ending,
        Ending::Stopped {
            outcome: "nobody-looked".to_string(),
            say: String::new()
        }
    );
}

#[tokio::test]
async fn a_stopped_recipe_takes_otherwise_and_ends_the_walk_when_there_is_none() {
    let with_a_way_out = json!({
        "workflow": 1, "start": "play",
        "steps": {
            "play": { "do": "run", "recipe": "rcp_search", "then": "done",
                      "otherwise": "gave-up" },
            "done": { "do": "stop", "outcome": "done" },
            "gave-up": { "do": "stop", "outcome": "gave-up" }
        }
    });
    let stopped = json!({"ok": false, "ran": 2, "stopped_at": 2,
                         "steps": [{}, {"error": "nothing at 40,40"}]});
    let computer = StubBox::default().with_receipts(vec![stopped.clone()]);
    let recipes = StubRecipes::default();
    let walk = walk_with(
        with_a_way_out,
        &computer,
        &recipes,
        &["rcp_search"],
        Judging::Off("off".to_string()),
    )
    .await;
    assert_eq!(
        walk.ending,
        Ending::Stopped {
            outcome: "gave-up".to_string(),
            say: String::new()
        }
    );

    let with_none = json!({
        "workflow": 1, "start": "play",
        "steps": {
            "play": { "do": "run", "recipe": "rcp_search", "then": "done" },
            "done": { "do": "stop", "outcome": "done" }
        }
    });
    let computer = StubBox::default().with_receipts(vec![stopped]);
    let walk = walk_with(
        with_none,
        &computer,
        &recipes,
        &["rcp_search"],
        Judging::Off("off".to_string()),
    )
    .await;
    match &walk.ending {
        Ending::RecipeFailed { at, recipe, why } => {
            assert_eq!(at, "play");
            assert_eq!(recipe, "rcp_search");
            assert_eq!(why, "nothing at 40,40");
        }
        other => panic!("expected a stopped recipe, got {other:?}"),
    }
    assert!(!walk.ok());
}

#[tokio::test]
async fn a_box_out_of_reach_is_not_a_recipe_that_failed() {
    let body = json!({
        "workflow": 1, "start": "play",
        "steps": {
            "play": { "do": "run", "recipe": "rcp_search", "then": "done",
                      "otherwise": "gave-up" },
            "done": { "do": "stop", "outcome": "done" },
            "gave-up": { "do": "stop", "outcome": "gave-up" }
        }
    });
    let computer = StubBox {
        refuses: true,
        ..StubBox::default()
    };
    let recipes = StubRecipes::default();
    let walk = walk_with(
        body,
        &computer,
        &recipes,
        &["rcp_search"],
        Judging::Off("off".to_string()),
    )
    .await;
    // `otherwise` was there and was NOT taken: a machine out of reach is a state, not a
    // verdict about the task.
    match &walk.ending {
        Ending::Broken { at, why } => {
            assert_eq!(at, "play");
            assert!(why.contains("would not play"), "{why}");
        }
        other => panic!("expected a fault, got {other:?}"),
    }
}

#[tokio::test]
async fn a_recipe_this_run_may_not_play_is_refused_by_name_before_the_box_is_touched() {
    let body = json!({
        "workflow": 1, "start": "play",
        "steps": {
            "play": { "do": "run", "recipe": "rcp_secret", "then": "done" },
            "done": { "do": "stop", "outcome": "done" }
        }
    });
    let computer = StubBox::default();
    let recipes = StubRecipes::default();
    let walk = walk_with(
        body,
        &computer,
        &recipes,
        &["rcp_search"],
        Judging::Off("off".to_string()),
    )
    .await;
    match &walk.ending {
        Ending::Broken { why, .. } => assert!(why.contains("rcp_secret"), "{why}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(computer.ran().is_empty(), "the box was never asked");
}

// ---- Jev, and the fallback --------------------------------------------------------------

fn a_question(question: Value, exits: Value) -> Value {
    let mut step = json!({ "do": "ask", "name": "q", "question": question });
    if let (Some(step), Some(exits)) = (step.as_object_mut(), exits.as_object()) {
        for (key, value) in exits {
            step.insert(key.clone(), value.clone());
        }
    }
    json!({
        "workflow": 1, "start": "q",
        "steps": {
            "q": step,
            "acted": { "do": "stop", "outcome": "acted" },
            "skipped": { "do": "stop", "outcome": "skipped" },
            "middling": { "do": "stop", "outcome": "middling" }
        }
    })
}

fn a_noul() -> Value {
    a_question(
        json!({ "kind": "noul", "instructions": "Is the field empty?" }),
        json!({ "yes": "acted", "no": "skipped" }),
    )
}

#[tokio::test]
async fn an_answered_question_branches_on_the_answer_and_records_the_confidence() {
    let judge = StubJudge::saying("yes", 0.93);
    let computer = StubBox::default();
    let recipes = StubRecipes::default();
    let walk = walk_with(a_noul(), &computer, &recipes, &[], Judging::Ask(&judge)).await;
    assert_eq!(
        walk.ending,
        Ending::Stopped {
            outcome: "acted".to_string(),
            say: String::new()
        }
    );
    assert_eq!(judge.asked(), vec!["q:q".to_string()]);
    let receipt = walk.receipt();
    assert_eq!(receipt["jev"], "asked");
    assert_eq!(receipt["fallbacks"], 0);
    assert_eq!(receipt["trail"][0]["answer"], "yes");
    assert_eq!(receipt["trail"][0]["confidence"], 0.93);
    assert!(receipt["trail"][0]["fallback"].is_null());
}

#[tokio::test]
async fn an_unreachable_jev_answers_no_and_the_run_says_who_answered() {
    let judge = StubJudge::failing(JudgeError::Unavailable(
        "Jev is unreachable: no route to host".to_string(),
    ));
    let computer = StubBox::default();
    let recipes = StubRecipes::default();
    let walk = walk_with(a_noul(), &computer, &recipes, &[], Judging::Ask(&judge)).await;
    assert_eq!(
        walk.ending,
        Ending::Stopped {
            outcome: "skipped".to_string(),
            say: String::new()
        },
        "a yes/no nobody could answer takes the branch that skips"
    );
    assert_eq!(walk.fallbacks, 1);
    let receipt = walk.receipt();
    assert_eq!(receipt["fallbacks"], 1);
    let fallback = &receipt["trail"][0]["fallback"];
    assert!(
        fallback["because"]
            .as_str()
            .unwrap()
            .contains("no route to host"),
        "{receipt}"
    );
    assert_eq!(fallback["rule"], "the branch that skips");
    // The confidence is absent rather than invented: nobody was confident about anything.
    assert!(receipt["trail"][0]["confidence"].is_null());
}

#[tokio::test]
async fn an_unreachable_jev_takes_the_first_choice_and_the_middle_level() {
    let computer = StubBox::default();
    let recipes = StubRecipes::default();
    let down = || StubJudge::failing(JudgeError::Unavailable("Jev is unreachable: x".to_string()));

    let judge = down();
    let choice = a_question(
        json!({ "kind": "choice", "instructions": "How did it go?",
                "choices": ["skipped", "acted"] }),
        json!({ "go": { "skipped": "skipped", "acted": "acted" } }),
    );
    let walk = walk_with(choice, &computer, &recipes, &[], Judging::Ask(&judge)).await;
    assert_eq!(
        walk.ending,
        Ending::Stopped {
            outcome: "skipped".to_string(),
            say: String::new()
        },
        "the FIRST option is taken, which is why an author makes it the safe one"
    );
    assert_eq!(
        walk.receipt()["trail"][0]["fallback"]["rule"],
        "the first option offered"
    );

    let judge = down();
    let score = a_question(
        json!({ "kind": "score", "instructions": "How much is left?",
                "levels": ["none", "some", "all of it"] }),
        json!({ "go": { "none": "acted", "some": "middling", "all of it": "skipped" } }),
    );
    let walk = walk_with(score, &computer, &recipes, &[], Judging::Ask(&judge)).await;
    assert_eq!(
        walk.ending,
        Ending::Stopped {
            outcome: "middling".to_string(),
            say: String::new()
        },
        "the MIDDLE level is taken: the ends of a rubric are the opinions"
    );
    assert_eq!(
        walk.receipt()["trail"][0]["fallback"]["rule"],
        "the middle level"
    );
}

#[tokio::test]
async fn a_malformed_question_stops_the_walk_instead_of_answering_itself() {
    let judge = StubJudge::failing(JudgeError::Malformed("a rubric with no levels".to_string()));
    let computer = StubBox::default();
    let recipes = StubRecipes::default();
    let walk = walk_with(a_noul(), &computer, &recipes, &[], Judging::Ask(&judge)).await;
    match &walk.ending {
        Ending::Broken { at, why } => {
            assert_eq!(at, "q");
            assert!(why.contains("could not be put to Jev"), "{why}");
            assert!(why.contains("a rubric with no levels"), "{why}");
        }
        other => panic!("expected a fault, got {other:?}"),
    }
    assert_eq!(
        walk.fallbacks, 0,
        "our own bug is never dressed up as a cautious answer"
    );
    assert!(!walk.ok());
}

#[tokio::test]
async fn jev_switched_off_is_never_asked_and_the_run_says_it_was_off_on_purpose() {
    let judge = StubJudge::saying("yes", 1.0);
    let computer = StubBox::default();
    let recipes = StubRecipes::default();
    let walk = walk_with(
        a_noul(),
        &computer,
        &recipes,
        &[],
        Judging::Off("Jev was switched off for this run".to_string()),
    )
    .await;
    assert!(
        judge.asked().is_empty(),
        "nothing is put to the classifier at all"
    );
    assert_eq!(
        walk.ending,
        Ending::Stopped {
            outcome: "skipped".to_string(),
            say: String::new()
        }
    );
    let receipt = walk.receipt();
    assert_eq!(receipt["jev"], "off");
    assert_eq!(receipt["fallbacks"], 1);
    assert_eq!(
        receipt["trail"][0]["fallback"]["because"],
        "Jev was switched off for this run"
    );
}

#[tokio::test]
async fn an_answer_that_was_never_offered_falls_back_and_says_what_it_heard() {
    let judge = StubJudge::saying("maybe", 0.6);
    let computer = StubBox::default();
    let recipes = StubRecipes::default();
    let walk = walk_with(a_noul(), &computer, &recipes, &[], Judging::Ask(&judge)).await;
    assert_eq!(
        walk.ending,
        Ending::Stopped {
            outcome: "skipped".to_string(),
            say: String::new()
        }
    );
    let receipt = walk.receipt();
    assert_eq!(receipt["fallbacks"], 1);
    assert!(
        receipt["trail"][0]["fallback"]["because"]
            .as_str()
            .unwrap()
            .contains("\"maybe\""),
        "{receipt}"
    );
    assert_eq!(receipt["trail"][0]["answer"], "no");
}

#[tokio::test]
async fn what_jev_judges_is_where_the_walk_is_and_what_it_has_gathered() {
    let body = json!({
        "workflow": 1, "start": "look",
        "steps": {
            "look": { "do": "observe", "as": "windows", "shell": "wmctrl -lx", "then": "q" },
            "q": { "do": "ask", "name": "empty",
                   "question": { "kind": "noul", "instructions": "Is the field empty?" },
                   "yes": "acted", "no": "acted" },
            "acted": { "do": "stop", "outcome": "acted" }
        }
    });
    struct Peeking(Mutex<Option<Value>>);
    #[async_trait::async_trait]
    impl Judge for Peeking {
        async fn judge(&self, ask: JudgeAsk<'_>) -> Result<Verdict, JudgeError> {
            *self.0.lock().unwrap() = Some(ask.state.clone());
            Ok(Verdict {
                answer: "yes".to_string(),
                confidence: 1.0,
            })
        }
    }
    let judge = Peeking(Mutex::new(None));
    let computer = StubBox::saying(&["chrome.Google-chrome"]);
    let recipes = StubRecipes::default();
    let walk = walk_with(body, &computer, &recipes, &[], Judging::Ask(&judge)).await;
    assert!(walk.ok());
    let state = judge.0.lock().unwrap().clone().expect("a state was sent");
    assert_eq!(state["workflow"], "Search once");
    assert_eq!(state["at"], "q");
    assert_eq!(state["facts"]["windows"], "chrome.Google-chrome");
    assert_eq!(state["facts"]["windows.exit"], "0");
    assert_eq!(state["did"][0]["step"], "look");
    assert!(state.is_object(), "never a bare string or number");
}

#[test]
fn the_recipes_a_tree_can_play_are_listed_for_the_route_to_pre_flight() {
    let parsed = workflow(json!({
        "workflow": 1, "start": "a",
        "steps": {
            "a": { "do": "run", "recipe": "rcp_one", "then": "b" },
            "b": { "do": "run", "recipe": "rcp_two", "then": "a", "otherwise": "done" },
            "done": { "do": "stop", "outcome": "done" }
        }
    }));
    assert_eq!(
        parsed.recipes(),
        ["rcp_one".to_string(), "rcp_two".to_string()]
            .into_iter()
            .collect::<BTreeSet<_>>()
    );
}
