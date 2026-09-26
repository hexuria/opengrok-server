use super::*;
use async_trait::async_trait;
use opengrok_box::{BoxResult, CommandOutput, Screenshot, StartedCommand};
use opengrok_core::coworker::{BoxMode, CoworkerCommand};
use serde_json::json;
use std::sync::Mutex;

#[test]
fn computer_step_images_default_to_agent_visibility() {
    let image = ToolImage::from(Screenshot {
        mime: "image/png".into(),
        png_base64: "AAAA".into(),
        width: 8,
        height: 8,
    });
    assert_eq!(image.visibility, ImageVisibility::Agent);
    let json = serde_json::to_value(&image).unwrap();
    assert_eq!(json["visibility"], "agent");
    let back: ToolImage = serde_json::from_value(json!({
        "mime": "image/png",
        "base64": "AAAA",
        "width": 8,
        "height": 8
    }))
    .unwrap();
    assert_eq!(
        back.visibility,
        ImageVisibility::Agent,
        "serde default for ToolImage is agent; AG-UI frames without visibility stay transcript"
    );
}

/// Records which box it was asked to act on, which is the assertion that matters here.
#[derive(Default)]
struct SpyComputer {
    ran_on: Mutex<Vec<(String, String)>>,
    fail_with: Option<BoxError>,
    /// The guest advertises an attached tunnel (`/v1/info`), for the after-wake tests.
    tunnel_ready: bool,
}

fn copy_error(error: &BoxError) -> BoxError {
    match error {
        BoxError::NoSuchBox => BoxError::NoSuchBox,
        BoxError::Secret(reason) => BoxError::Secret(reason.clone()),
        BoxError::Unreachable(detail) => BoxError::Unreachable(detail.clone()),
        BoxError::Interrupted(detail) => BoxError::Interrupted(detail.clone()),
        BoxError::Refused { status, body } => BoxError::Refused {
            status: *status,
            body: body.clone(),
        },
    }
}

impl SpyComputer {
    fn last_box(&self) -> Option<String> {
        self.ran_on
            .lock()
            .ok()
            .and_then(|calls| calls.last().map(|(box_id, _)| box_id.clone()))
    }
}

#[async_trait]
impl Computer for SpyComputer {
    async fn egress_tunnel(&self, _box_id: &str) -> Option<opengrok_box::EgressTunnel> {
        self.tunnel_ready.then_some(opengrok_box::EgressTunnel {
            enabled: true,
            ready: true,
        })
    }
    async fn create(&self, _ttl: Option<u64>) -> BoxResult<String> {
        Ok("box_new".to_string())
    }
    async fn run(&self, box_id: &str, command: &str, _t: u32) -> BoxResult<CommandOutput> {
        if let Ok(mut calls) = self.ran_on.lock() {
            calls.push((box_id.to_string(), command.to_string()));
        }
        if let Some(error) = &self.fail_with {
            return Err(copy_error(error));
        }
        Ok(CommandOutput {
            exit_code: 0,
            stdout: format!("ran `{command}`"),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            timed_out: false,
        })
    }
    async fn run_recipe(&self, box_id: &str, request: &Value) -> BoxResult<Value> {
        if let Ok(mut calls) = self.ran_on.lock() {
            calls.push((box_id.to_string(), format!("recipe:{request}")));
        }
        if let Some(error) = &self.fail_with {
            return Err(copy_error(error));
        }
        let name = request.get("name").and_then(Value::as_str).unwrap_or("");
        // The box answers the level it was asked for, and answers nothing about observation
        // when it was not asked — which is what a box that predates the field also does.
        let observed = request.get("observe").and_then(Value::as_str).is_some();
        if name == "stops" {
            let mut receipt = json!({
                "ok": false, "ran": 1, "stopped_at": 1,
                "steps": [{"index": 0, "op": "click", "ok": true},
                          {"index": 1, "op": "click", "ok": false, "error": "nothing at (5, 5)"}],
            });
            if observed && let Some(object) = receipt.as_object_mut() {
                object.insert("observe".to_string(), request["observe"].clone());
                object["steps"][0]["observed"] = json!({
                    "target": {"id": "0x1", "class": "chromium.Chromium", "title": "Inbox"},
                    "observe_ms": 8,
                });
                object["steps"][1]["observed"] = json!({ "observe_ms": 7 });
            }
            return Ok(receipt);
        }
        let mut receipt = json!({
            "ok": true, "ran": 2, "stopped_at": null,
            "steps": [{"index": 0, "op": "click", "ok": true},
                      {"index": 1, "op": "type", "ok": true}],
            "screenshot": {"mime": "image/png", "png_base64": "iVBORw0KGgo=", "width": 1280, "height": 800},
        });
        if observed && let Some(object) = receipt.as_object_mut() {
            object.insert("observe".to_string(), request["observe"].clone());
            object["steps"][0]["observed"] = json!({
                "target": {"id": "0x1", "class": "xterm.XTerm", "title": "Terminal"},
                "observe_ms": 9,
            });
            object["steps"][1]["observed"] = json!({
                "focus": {"state": "none"}, "observe_ms": 5,
            });
        }
        Ok(receipt)
    }
    async fn start(&self, _b: &str, _c: &str) -> BoxResult<StartedCommand> {
        unimplemented!("not used by these tests")
    }
    async fn watch(&self, _b: &str, _p: &str) -> BoxResult<StartedCommand> {
        unimplemented!("not used by these tests")
    }
    async fn read_file(&self, box_id: &str, path: &str) -> BoxResult<String> {
        if let Ok(mut calls) = self.ran_on.lock() {
            calls.push((box_id.to_string(), path.to_string()));
        }
        Ok("file contents".to_string())
    }
    async fn write_file(&self, box_id: &str, path: &str, _c: &str) -> BoxResult<()> {
        if let Ok(mut calls) = self.ran_on.lock() {
            calls.push((box_id.to_string(), path.to_string()));
        }
        Ok(())
    }
    async fn expose_port(&self, _b: &str, _p: u16, _t: &str) -> BoxResult<String> {
        Ok("https://example".to_string())
    }
    async fn stop(&self, _b: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn resume(&self, _b: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn destroy(&self, _b: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn state(&self, _b: &str) -> BoxResult<String> {
        Ok("running".to_string())
    }
}

/// A policy that allows everything, for tests about something other than policy.
fn permissive() -> opengrok_policy::Context {
    opengrok_policy::Context {
        grant: Some(opengrok_policy::Grant {
            principal: AccountId::from_stored("acct_1"),
            coworker: CoworkerId::from_stored("cw_1"),
            profile: opengrok_policy::ToolSet::All,
            needs_approval: opengrok_policy::ToolSet::None,
            revoked: false,
        }),
        ceiling: Some(opengrok_policy::Ceiling {
            coworker: CoworkerId::from_stored("cw_1"),
            tools: opengrok_policy::ToolSet::All,
        }),
    }
}

fn allowing(computer: Arc<dyn Computer>) -> Executor {
    Executor::with_policy(computer, permissive())
}

/// A box whose reported states are scripted (the last repeats), counting resumes and the
/// commands that reached it.
struct SleepyComputer {
    states: Mutex<std::collections::VecDeque<&'static str>>,
    resumes: std::sync::atomic::AtomicUsize,
    ran: Mutex<Vec<String>>,
}

impl SleepyComputer {
    fn new(states: &[&'static str]) -> Self {
        Self {
            states: Mutex::new(states.iter().copied().collect()),
            resumes: std::sync::atomic::AtomicUsize::new(0),
            ran: Mutex::new(Vec::new()),
        }
    }
    fn resumes(&self) -> usize {
        self.resumes.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn ran(&self) -> usize {
        self.ran.lock().map(|ran| ran.len()).unwrap_or(0)
    }
}

#[async_trait]
impl Computer for SleepyComputer {
    async fn create(&self, _ttl: Option<u64>) -> BoxResult<String> {
        Ok("box_new".to_string())
    }
    async fn run(&self, _b: &str, command: &str, _t: u32) -> BoxResult<CommandOutput> {
        if let Ok(mut ran) = self.ran.lock() {
            ran.push(command.to_string());
        }
        Ok(CommandOutput {
            exit_code: 0,
            stdout: format!("ran `{command}`"),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            timed_out: false,
        })
    }
    async fn start(&self, _b: &str, _c: &str) -> BoxResult<StartedCommand> {
        Err(BoxError::NoSuchBox)
    }
    async fn watch(&self, _b: &str, _p: &str) -> BoxResult<StartedCommand> {
        Err(BoxError::NoSuchBox)
    }
    async fn read_file(&self, _b: &str, _p: &str) -> BoxResult<String> {
        Err(BoxError::NoSuchBox)
    }
    async fn write_file(&self, _b: &str, _p: &str, _c: &str) -> BoxResult<()> {
        Err(BoxError::NoSuchBox)
    }
    async fn expose_port(&self, _b: &str, _p: u16, _t: &str) -> BoxResult<String> {
        Err(BoxError::NoSuchBox)
    }
    async fn stop(&self, _b: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn resume(&self, _b: &str) -> BoxResult<()> {
        self.resumes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    async fn destroy(&self, _b: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn state(&self, _b: &str) -> BoxResult<String> {
        let mut states = self.states.lock().unwrap();
        let next = if states.len() > 1 {
            states.pop_front().unwrap()
        } else {
            states.front().copied().unwrap_or("absent")
        };
        Ok(next.to_string())
    }
}

/// The first box-bound call of a turn wakes a sleeping box; the second finds it awake and
/// does not ask again.
#[tokio::test]
async fn a_sleeping_box_is_woken_by_the_first_tool_that_needs_it_and_not_again() {
    // The probe, the executor and the wake each read the state once before the start lands.
    let sleepy = Arc::new(SleepyComputer::new(&[
        "exited", "exited", "exited", "running",
    ]));
    let executor = allowing(sleepy.clone());
    let context = context_with_box("box_mine");
    assert!(
        executor
            .box_needs_wake(&context, &call("shell", json!({"command": "ls"})))
            .await
    );

    let first = executor
        .execute(&context, &call("shell", json!({"command": "ls"})))
        .await;
    assert!(first.ok, "{first:?}");
    let second = executor
        .execute(&context, &call("shell", json!({"command": "pwd"})))
        .await;
    assert!(second.ok, "{second:?}");
    assert_eq!(sleepy.resumes(), 1, "one start for the whole turn");
    assert_eq!(sleepy.ran(), 2);
    assert!(
        !executor
            .box_needs_wake(&context, &call("shell", json!({"command": "ls"})))
            .await,
        "a box seen running is not asked about again"
    );
}

/// The desktop image has no BIR binary. A box shell that names one must not wake the
/// box or raise an egress card. The same command on the user machine is a different tool.
#[tokio::test]
async fn a_box_shell_that_names_gpui_agent_is_refused_before_the_box_starts() {
    let sleepy = Arc::new(SleepyComputer::new(&["exited"]));
    let executor = allowing(sleepy.clone());
    let context = context_with_box("box_mine");
    for command in [
        "gpui-agent hello",
        "bir-headless serve --wait",
        "GPUI_AGENT_ADDR=127.0.0.1:17423 gpui-agent invoke nav.go --arg page=settings",
        "/usr/local/bin/gpui-agent invoke profile.list",
    ] {
        let result = executor
            .execute(&context, &call("shell", json!({"command": command})))
            .await;
        assert!(!result.ok, "{command}: {result:?}");
        assert!(
            result.content.contains("not on this computer"),
            "{command}: {result:?}"
        );
        assert!(!result.awaiting_approval, "{command}: {result:?}");
    }
    assert_eq!(sleepy.ran(), 0, "the box must not run the command");
    assert_eq!(sleepy.resumes(), 0, "the box must not be woken");

    let awake = Arc::new(SleepyComputer::new(&["running"]));
    let listed = allowing(awake.clone())
        .execute(&context, &call("shell", json!({"command": "ls"})))
        .await;
    assert!(listed.ok, "{listed:?}");
    assert_eq!(awake.ran(), 1, "a normal shell still reaches the box");

    let on_the_mac = executor
        .execute(
            &context,
            &call(USER_MACHINE_SHELL, json!({"command": "gpui-agent hello"})),
        )
        .await;
    assert!(!on_the_mac.ok, "{on_the_mac:?}");
    assert!(
        !on_the_mac.content.contains("not on this computer"),
        "the user machine is not the box image: {on_the_mac:?}"
    );
}

/// A box that will not come up ends the call with the sentence the model relays, and the
/// command never runs. The wake gives up in two polls, not the full patience.
#[tokio::test]
async fn a_box_that_will_not_come_up_answers_that_the_computer_is_down() {
    let sleepy = Arc::new(SleepyComputer::new(&["exited", "exited", "exited"]));
    let executor = allowing(sleepy.clone()).with_wake_patience(std::time::Duration::from_secs(60));
    let context = context_with_box("box_mine");
    let began = std::time::Instant::now();
    let result = executor
        .execute(&context, &call("shell", json!({"command": "ls"})))
        .await;
    assert!(!result.ok, "{result:?}");
    assert!(result.content.contains(COMPUTER_DOWN), "{result:?}");
    assert_eq!(sleepy.ran(), 0, "nothing ran on a box that is down");
    assert!(
        began.elapsed() < std::time::Duration::from_secs(10),
        "gave up after {:?}",
        began.elapsed()
    );
}

/// A box that is down costs one wait per turn: the second call answers at once with the same
/// sentence, and the frame is not announced again.
#[tokio::test]
async fn a_box_that_is_down_costs_one_wait_per_turn() {
    let sleepy = Arc::new(SleepyComputer::new(&["exited", "exited", "exited"]));
    let executor = allowing(sleepy.clone()).with_wake_patience(std::time::Duration::from_secs(60));
    let context = context_with_box("box_mine");
    let first = executor
        .execute(&context, &call("shell", json!({"command": "ls"})))
        .await;
    assert!(first.content.contains(COMPUTER_DOWN), "{first:?}");
    let began = std::time::Instant::now();
    let second = executor
        .execute(&context, &call("shell", json!({"command": "pwd"})))
        .await;
    assert!(second.content.contains(COMPUTER_DOWN), "{second:?}");
    assert!(
        began.elapsed() < std::time::Duration::from_millis(500),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(sleepy.resumes(), 1, "one start for the whole turn");
    assert!(
        !executor
            .box_needs_wake(&context, &call("shell", json!({"command": "ls"})))
            .await,
        "a box already answered for is not announced as waking"
    );
}

/// A call that will park on a card or be refused before it reaches the box does not announce
/// a wake — nothing is going to wake.
#[tokio::test]
async fn a_call_that_never_reaches_the_box_does_not_announce_a_wake() {
    let sleepy = Arc::new(SleepyComputer::new(&["exited"]));
    let mut policy = permissive();
    if let Some(grant) = policy.grant.as_mut() {
        grant.needs_approval = opengrok_policy::ToolSet::All;
    }
    let executor = Executor::with_policy(sleepy.clone(), policy);
    let context = context_with_box("box_mine");
    assert!(
        !executor
            .box_needs_wake(&context, &call("shell", json!({"command": "ls"})))
            .await
    );
    let held = allowing(sleepy.clone()).with_screen(true);
    let mut holding = context_with_box("box_mine");
    holding.screen_hold = true;
    assert!(
        !held
            .box_needs_wake(&holding, &call("computer", json!({"action": "screenshot"})))
            .await
    );
    assert_eq!(sleepy.resumes(), 0);
}

/// "Still starting" invites a retry, so the next call asks again and finds the box up; the
/// in-use stamp fires only for the call that actually woke it.
#[tokio::test]
async fn a_box_still_starting_is_asked_about_again_and_stamped_once_woken() {
    // state reads: executor, wake start, wake poll → "provisioning" past a 1 s patience;
    // then the retry: executor, wake start, poll → running.
    let sleepy = Arc::new(SleepyComputer::new(&[
        "archived",
        "archived",
        "provisioning",
        "provisioning",
        "archived",
        "running",
    ]));
    let stamped = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = stamped.clone();
    let executor = allowing(sleepy.clone())
        .with_wake_patience(std::time::Duration::from_secs(1))
        .with_on_woken(Arc::new(move |_| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));
    let context = context_with_box("box_mine");
    let first = executor
        .execute(&context, &call("shell", json!({"command": "ls"})))
        .await;
    assert!(first.content.contains(COMPUTER_STARTING), "{first:?}");
    assert_eq!(stamped.load(std::sync::atomic::Ordering::SeqCst), 0);
    let second = executor
        .execute(&context, &call("shell", json!({"command": "ls"})))
        .await;
    assert!(second.ok, "the retry found the box up: {second:?}");
    assert_eq!(stamped.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(sleepy.ran(), 1);
}

/// A running box is never resumed, and a plugin tool never asks about the box at all.
#[tokio::test]
async fn a_running_box_is_left_alone_and_only_box_tools_ask_about_it() {
    let awake = Arc::new(SleepyComputer::new(&["running"]));
    let executor = allowing(awake.clone());
    let context = context_with_box("box_mine");
    assert!(
        !executor
            .box_needs_wake(&context, &call("shell", json!({"command": "ls"})))
            .await
    );
    assert!(
        !executor
            .box_needs_wake(&context, &call("some_plugin_tool", json!({})))
            .await
    );
    let result = executor
        .execute(&context, &call("shell", json!({"command": "ls"})))
        .await;
    assert!(result.ok, "{result:?}");
    assert_eq!(awake.resumes(), 0);
}

fn context_with_box(box_id: &str) -> ToolContext {
    let mut coworker = opengrok_core::coworker::Coworker::default();
    for event in coworker
        .decide(CoworkerCommand::Hire {
            name: "Ada".to_string(),
            model: "m".to_string(),
            at_ms: 1,
        })
        .unwrap()
    {
        coworker.apply(&event);
    }
    for event in coworker
        .decide(CoworkerCommand::AssignComputer {
            box_id: BoxId::from_stored(box_id),
            mode: BoxMode::Dedicated,
            at_ms: 2,
        })
        .unwrap()
    {
        coworker.apply(&event);
    }
    ToolContext::from_coworker(
        AccountId::from_stored("acct_1"),
        CoworkerId::from_stored("cw_1"),
        &coworker,
    )
}

fn call(name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: "call_1".to_string(),
        name: name.to_string(),
        arguments,
    }
}

/// THE TEST THIS MODULE EXISTS FOR. A model that asks for someone else's box gets its own.
#[tokio::test]
async fn a_model_cannot_run_a_command_on_another_box() {
    let spy = Arc::new(SpyComputer::default());
    let executor = allowing(spy.clone());
    let context = context_with_box("box_mine");

    let result = executor
        .execute(
            &context,
            &call(
                "shell",
                json!({"command": "whoami", "box_id": "box_of_someone_else"}),
            ),
        )
        .await;

    assert!(result.ok, "{result:?}");
    assert_eq!(
        spy.last_box().as_deref(),
        Some("box_mine"),
        "the session's box must win over the model's argument"
    );
}

/// The same rule for every tool that touches a box, not just the interesting one.
#[tokio::test]
async fn the_file_tools_are_pinned_to_the_session_box_too() {
    let spy = Arc::new(SpyComputer::default());
    let executor = allowing(spy.clone());
    let context = context_with_box("box_mine");

    for tool in ["read_file", "write_file"] {
        let result = executor
            .execute(
                &context,
                &call(
                    tool,
                    json!({"path": "/tmp/a", "content": "x", "box_id": "box_elsewhere"}),
                ),
            )
            .await;
        assert!(result.ok, "{tool}: {result:?}");
        assert_eq!(spy.last_box().as_deref(), Some("box_mine"), "{tool}");
    }
}

/// Overwriting is additive: a key the model omitted is still set, so no tool can be reached
/// with an absent identity.
#[test]
fn identity_is_set_even_when_the_model_omitted_it() {
    let context = context_with_box("box_mine");
    let overwritten = overwrite_identity(&json!({"command": "ls"}), &context);
    assert_eq!(overwritten["coworker_id"], "cw_1");
    assert_eq!(overwritten["box_id"], "box_mine");
    assert_eq!(
        overwritten["command"], "ls",
        "and the real argument survives"
    );
}

/// A non-object argument carries no identity, but must not be able to smuggle one either.
#[test]
fn a_non_object_argument_cannot_smuggle_an_identity() {
    let context = context_with_box("box_mine");
    let overwritten = overwrite_identity(&json!("box_id=box_elsewhere"), &context);
    assert_eq!(overwritten["box_id"], "box_mine");
}

/// Every identity alias the model might write — snake_case AND camelCase, for account, coworker
/// and box — is overwritten or removed, never forwarded. The camelCase twins used to ride
/// through to a remote plugin tool verbatim; this is the regression guard for that gap.
#[test]
fn a_camel_case_identity_alias_cannot_ride_through() {
    let context = context_with_box("box_mine");
    let overwritten = overwrite_identity(
        &json!({
            "coworkerId": "cw_somebody_else",
            "boxId": "box_elsewhere",
            "accountId": "acct_elsewhere",
            "command": "ls",
        }),
        &context,
    );
    // The model's camelCase choices are gone; only the session's snake_case identity remains.
    assert!(overwritten.get("coworkerId").is_none(), "{overwritten:?}");
    assert!(overwritten.get("boxId").is_none(), "{overwritten:?}");
    assert!(overwritten.get("accountId").is_none(), "{overwritten:?}");
    assert_eq!(overwritten["coworker_id"], "cw_1");
    assert_eq!(overwritten["box_id"], "box_mine");
    assert_eq!(overwritten["command"], "ls", "the real argument survives");
}

/// `strip_identity` (arguments about to leave for a remote MCP server) removes every alias too,
/// so nothing the model chose about identity — in any spelling — reaches a connector.
#[test]
fn strip_identity_removes_every_alias_before_a_remote_call() {
    let stripped = strip_identity(json!({
        "coworker_id": "cw_1",
        "coworkerId": "cw_2",
        "box_id": "box_1",
        "boxId": "box_2",
        "account_id": "acct_1",
        "accountId": "acct_2",
        "query": "hello",
    }));
    let object = stripped.as_object().unwrap();
    for key in [
        "coworker_id",
        "coworkerId",
        "box_id",
        "boxId",
        "account_id",
        "accountId",
    ] {
        assert!(object.get(key).is_none(), "{key} survived: {stripped:?}");
    }
    assert_eq!(stripped["query"], "hello", "the real argument survives");
}

/// With no computer assigned, a model's `box_id` must not become the one used.
#[test]
fn without_a_computer_the_models_box_id_is_removed_not_kept() {
    let context = ToolContext {
        account_id: AccountId::from_stored("acct_1"),
        coworker_id: CoworkerId::from_stored("cw_1"),
        box_id: None,
        group_box: None,
        screen_hold: false,
        screen_held_in: None,
    };
    let overwritten = overwrite_identity(&json!({"box_id": "box_elsewhere"}), &context);
    assert!(
        overwritten.get("box_id").is_none(),
        "{overwritten:?} still carries a box the model chose"
    );
}

#[tokio::test]
async fn a_coworker_without_a_computer_is_refused_not_crashed() {
    let executor = allowing(Arc::new(SpyComputer::default()));
    let context = ToolContext {
        account_id: AccountId::from_stored("acct_1"),
        coworker_id: CoworkerId::from_stored("cw_1"),
        box_id: None,
        group_box: None,
        screen_hold: false,
        screen_held_in: None,
    };
    let result = executor
        .execute(&context, &call("shell", json!({"command": "ls"})))
        .await;
    assert!(!result.ok);
    assert!(result.content.contains("no computer"), "{result:?}");
}

/// A refusal is a result the model can reason about, never a thrown error that kills the run.
#[tokio::test]
async fn an_unknown_tool_is_a_result_not_an_error() {
    let executor = allowing(Arc::new(SpyComputer::default()));
    let result = executor
        .execute(
            &context_with_box("box_mine"),
            &call("rm_rf_everything", json!({})),
        )
        .await;
    assert!(!result.ok);
    assert!(result.content.starts_with("refused:"), "{result:?}");
    assert!(result.content.contains("rm_rf_everything"));
}

#[tokio::test]
async fn bad_arguments_are_refused_with_a_reason_the_model_can_fix() {
    let executor = allowing(Arc::new(SpyComputer::default()));
    let result = executor
        .execute(&context_with_box("box_mine"), &call("shell", json!({})))
        .await;
    assert!(!result.ok);
    assert!(result.content.contains("bad arguments"), "{result:?}");
}

/// A box that is gone must reach the model as words, not as a dead run.
#[tokio::test]
async fn an_unreachable_computer_is_reported_to_the_model() {
    let spy = Arc::new(SpyComputer {
        ran_on: Mutex::new(Vec::new()),
        fail_with: Some(BoxError::NoSuchBox),
        tunnel_ready: false,
    });
    let executor = allowing(spy);
    let result = executor
        .execute(
            &context_with_box("box_mine"),
            &call("shell", json!({"command": "ls"})),
        )
        .await;
    assert!(!result.ok);
    assert!(result.content.contains("no longer exists"), "{result:?}");
}

/// A tool needing approval suspends instead of running, and does not read as success.
#[tokio::test]
async fn a_tool_needing_approval_does_not_run() {
    let mut policy = permissive();
    if let Some(grant) = policy.grant.as_mut() {
        grant.needs_approval = opengrok_policy::ToolSet::only(["shell"]);
    }
    let spy = Arc::new(SpyComputer::default());
    let executor = Executor::with_policy(spy.clone(), policy);

    let result = executor
        .execute(
            &context_with_box("box_mine"),
            &call("shell", json!({"command": "rm -rf /"})),
        )
        .await;

    assert!(result.awaiting_approval, "{result:?}");
    assert!(!result.ok, "a pending approval must not read as success");
    // And nothing reached the computer: approval gates the action, not its undo.
    assert_eq!(spy.last_box(), None);
}

/// The invariant, now that the set is dynamic: what a model is offered is what it can call.
#[tokio::test]
async fn plugin_tools_join_the_offered_set() {
    let executor = allowing(Arc::new(SpyComputer::default())).with_plugin_tools(
        BTreeMap::new(),
        vec![crate::mcp::McpTool {
            qualified_name: "gmail.api.send".to_string(),
            remote_name: "send".to_string(),
            description: Some("Send a message".to_string()),
            ..Default::default()
        }],
    );

    let offered = executor.tool_names();
    // The built-ins are still there — a plugin adds, it does not replace.
    assert!(offered.contains(&"shell".to_string()), "{offered:?}");
    assert!(
        offered.contains(&"gmail.api.send".to_string()),
        "{offered:?}"
    );
}

fn openai_safe_wire(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[test]
fn plugin_tool_schemas_advertise_openai_safe_names() {
    let long = format!("{}.api.{}", "plug".repeat(20), "tool".repeat(20));
    let executor = allowing(Arc::new(SpyComputer::default())).with_plugin_tools(
        BTreeMap::new(),
        vec![
            crate::mcp::McpTool {
                qualified_name: "gmail.api.send".to_string(),
                remote_name: "send".to_string(),
                description: Some("Send a message".to_string()),
                ..Default::default()
            },
            crate::mcp::McpTool {
                qualified_name: "a.b.c.d".to_string(),
                remote_name: "c.d".to_string(),
                description: None,
                ..Default::default()
            },
            crate::mcp::McpTool {
                qualified_name: "a.b.c_d".to_string(),
                remote_name: "c_d".to_string(),
                description: None,
                ..Default::default()
            },
            crate::mcp::McpTool {
                qualified_name: long.clone(),
                remote_name: "t".to_string(),
                description: None,
                ..Default::default()
            },
        ],
    );
    let account = AccountId::from_stored("acct_1");
    let coworker = CoworkerId::from_stored("cw_1");
    let schemas = executor.tool_schemas(&account, &coworker);
    let names: Vec<String> = schemas
        .iter()
        .filter_map(|schema| schema["function"]["name"].as_str().map(str::to_string))
        .collect();
    let truncated = crate::mcp::openai_safe_tool_name(&long);
    assert_eq!(truncated.len(), 64);
    assert!(
        names.contains(&truncated),
        "names longer than 64 must be truncated on the wire: {names:?}"
    );
    assert!(names.contains(&"shell".to_string()), "{names:?}");
    assert!(
        names.contains(&"gmail_api_send".to_string()),
        "gmail.api.send must be advertised without dots: {names:?}"
    );
    assert!(
        !names.iter().any(|name| name.contains('.')),
        "OpenAI function.name must not contain dots: {names:?}"
    );
    assert!(names.contains(&"a_b_c_d".to_string()), "{names:?}");
    assert!(names.contains(&"a_b_c_d_2".to_string()), "{names:?}");
    for name in &names {
        assert!(openai_safe_wire(name), "illegal OpenAI name {name}");
    }
}

#[tokio::test]
async fn a_model_calling_the_openai_safe_name_reaches_the_plugin() {
    let executor = allowing(Arc::new(SpyComputer::default())).with_plugin_tools(
        BTreeMap::new(),
        vec![crate::mcp::McpTool {
            qualified_name: "gmail.api.send".to_string(),
            remote_name: "send".to_string(),
            description: None,
            ..Default::default()
        }],
    );
    let result = executor
        .execute(
            &context_with_box("box_mine"),
            &call("gmail_api_send", json!({})),
        )
        .await;
    assert!(!result.ok);
    assert!(
        result.content.contains("not connected"),
        "safe wire name must resolve to the plugin: {result:?}"
    );
    assert!(
        !result.content.contains("there is no tool called"),
        "{result:?}"
    );
}

#[tokio::test]
async fn colliding_wire_names_round_trip_to_the_matching_plugin() {
    let executor = allowing(Arc::new(SpyComputer::default())).with_plugin_tools(
        BTreeMap::new(),
        vec![
            crate::mcp::McpTool {
                qualified_name: "a.b.c.d".to_string(),
                remote_name: "c.d".to_string(),
                description: None,
                ..Default::default()
            },
            crate::mcp::McpTool {
                qualified_name: "a.b.c_d".to_string(),
                remote_name: "c_d".to_string(),
                description: None,
                ..Default::default()
            },
        ],
    );
    let first = executor
        .execute(&context_with_box("box_mine"), &call("a_b_c_d", json!({})))
        .await;
    assert!(
        first.content.contains("a.b.c.d"),
        "plain sanitised name is the first plugin: {first:?}"
    );
    let second = executor
        .execute(&context_with_box("box_mine"), &call("a_b_c_d_2", json!({})))
        .await;
    assert!(
        second.content.contains("a.b.c_d"),
        "suffix _2 is the colliding plugin: {second:?}"
    );
}

/// A plugin whose session is gone must say so, rather than reading as "no such tool" — those
/// send a person to different places.
#[tokio::test]
async fn an_offered_plugin_tool_with_no_session_says_it_is_not_connected() {
    let executor = allowing(Arc::new(SpyComputer::default())).with_plugin_tools(
        BTreeMap::new(),
        vec![crate::mcp::McpTool {
            qualified_name: "gmail.api.send".to_string(),
            remote_name: "send".to_string(),
            description: None,
            ..Default::default()
        }],
    );

    let result = executor
        .execute(
            &context_with_box("box_mine"),
            &call("gmail.api.send", json!({})),
        )
        .await;
    assert!(!result.ok);
    assert!(result.content.contains("not connected"), "{result:?}");
    assert!(
        !result.content.contains("there is no tool called"),
        "a connection problem must not read as a missing tool: {result:?}"
    );
}

/// A plugin tool is governed exactly like `shell`: policy first, and nothing reaches the wire.
#[tokio::test]
async fn a_plugin_tool_outside_the_ceiling_never_reaches_the_network() {
    let mut policy = permissive();
    if let Some(ceiling) = policy.ceiling.as_mut() {
        // The coworker may use the built-ins and nothing a plugin brought.
        ceiling.tools = opengrok_policy::ToolSet::only(["shell"]);
    }
    let executor = Executor::with_policy(Arc::new(SpyComputer::default()), policy)
        .with_plugin_tools(
            BTreeMap::new(),
            vec![crate::mcp::McpTool {
                qualified_name: "gmail.api.send".to_string(),
                remote_name: "send".to_string(),
                description: None,
                ..Default::default()
            }],
        );

    let result = executor
        .execute(
            &context_with_box("box_mine"),
            &call("gmail.api.send", json!({})),
        )
        .await;
    assert!(!result.ok);
    assert!(result.content.contains("may never run"), "{result:?}");
    let via_wire = executor
        .execute(
            &context_with_box("box_mine"),
            &call("gmail_api_send", json!({})),
        )
        .await;
    assert!(
        via_wire.content.contains("may never run"),
        "policy uses the internal dotted name: {via_wire:?}"
    );
}

/// Our identity keys are for LOCAL tools. A remote server never asked for them, and a strict
/// one would reject the call over a field the model never wrote.
#[test]
fn identity_keys_are_stripped_before_reaching_a_remote_server() {
    let stripped = strip_identity(json!({
        "to": "someone@example.com",
        "coworker_id": "cw_1",
        "box_id": "box_1"
    }));
    assert!(stripped.get("coworker_id").is_none(), "{stripped}");
    assert!(stripped.get("box_id").is_none(), "{stripped}");
    // And the tool's own arguments survive untouched.
    assert_eq!(stripped["to"], "someone@example.com");
}

/// An approved call runs; another call of the same tool still waits.
#[tokio::test]
async fn approval_releases_one_call_and_not_the_tool() {
    let mut policy = permissive();
    if let Some(grant) = policy.grant.as_mut() {
        grant.needs_approval = opengrok_policy::ToolSet::only(["shell"]);
    }
    let spy = Arc::new(SpyComputer::default());
    let executor =
        Executor::with_policy(spy.clone(), policy).with_approved(["approved-call".to_string()]);
    let context = context_with_box("box_mine");

    let allowed = executor
        .execute(
            &context,
            &ToolCall {
                id: "approved-call".to_string(),
                name: "shell".to_string(),
                arguments: json!({"command": "ls"}),
            },
        )
        .await;
    assert!(allowed.ok, "the approved call should run: {allowed:?}");

    // A DIFFERENT call of the same tool is still waiting. Approving `shell` once must not
    // approve every later `shell`.
    let still_waiting = executor
        .execute(
            &context,
            &ToolCall {
                id: "some-other-call".to_string(),
                name: "shell".to_string(),
                arguments: json!({"command": "rm -rf /"}),
            },
        )
        .await;
    assert!(still_waiting.awaiting_approval, "{still_waiting:?}");
}

/// An approval cannot rescue a tool that policy denies outright — it releases waiting, not
/// refusal.
#[tokio::test]
async fn approval_does_not_override_a_denial() {
    let mut policy = permissive();
    if let Some(ceiling) = policy.ceiling.as_mut() {
        ceiling.tools = opengrok_policy::ToolSet::only(["read_file"]);
    }
    let executor = Executor::with_policy(Arc::new(SpyComputer::default()), policy)
        .with_approved(["c1".to_string()]);
    let result = executor
        .execute(
            &context_with_box("box_mine"),
            &ToolCall {
                id: "c1".to_string(),
                name: "shell".to_string(),
                arguments: json!({"command": "ls"}),
            },
        )
        .await;
    assert!(!result.ok);
    assert!(result.content.contains("may never run"), "{result:?}");
}

/// An executor built without policy refuses everything. The default being useless is the
/// point: a missing policy must never read as permission.
#[tokio::test]
async fn an_executor_without_a_policy_allows_nothing() {
    let executor = Executor::new(Arc::new(SpyComputer::default()));
    let result = executor
        .execute(
            &context_with_box("box_mine"),
            &call("shell", json!({"command": "ls"})),
        )
        .await;
    assert!(!result.ok, "{result:?}");
    assert!(result.content.contains("no grant"), "{result:?}");
}

/// Policy is consulted before the tool runs, and its reason reaches the model.
#[tokio::test]
async fn a_tool_outside_the_ceiling_is_refused_with_the_rule() {
    let mut policy = permissive();
    if let Some(ceiling) = policy.ceiling.as_mut() {
        ceiling.tools = opengrok_policy::ToolSet::only(["read_file"]);
    }
    let spy = Arc::new(SpyComputer::default());
    let executor = Executor::with_policy(spy.clone(), policy);

    let result = executor
        .execute(
            &context_with_box("box_mine"),
            &call("shell", json!({"command": "ls"})),
        )
        .await;
    assert!(!result.ok, "{result:?}");
    assert!(result.content.contains("may never run shell"), "{result:?}");
    // Refused BEFORE the computer was touched: a denied tool must not run and then be undone.
    assert_eq!(
        spy.last_box(),
        None,
        "the command must never have reached a box"
    );
}

/// A revoked grant stops the next tool call, not the next session.
#[tokio::test]
async fn a_revoked_grant_stops_tools_immediately() {
    let mut policy = permissive();
    if let Some(grant) = policy.grant.as_mut() {
        grant.revoked = true;
    }
    let executor = Executor::with_policy(Arc::new(SpyComputer::default()), policy);
    let result = executor
        .execute(
            &context_with_box("box_mine"),
            &call("shell", json!({"command": "ls"})),
        )
        .await;
    assert!(!result.ok);
    assert!(result.content.contains("revoked"), "{result:?}");
}

/// Every offered tool must be executable — one that is offered but unknown is a dead end the
/// model will keep trying.
#[tokio::test]
async fn every_offered_tool_is_actually_implemented() {
    let executor = allowing(Arc::new(SpyComputer::default()));
    let context = context_with_box("box_mine");
    for name in executor.tool_names() {
        let result = executor
            .execute(
                &context,
                &call(
                    &name,
                    json!({
                        "command": "ls",
                        "path": "/tmp/a",
                        "content": "x",
                        "origin": "accounts.google.com"
                    }),
                ),
            )
            .await;
        assert!(
            !result.content.contains("there is no tool called"),
            "{name} is offered but not implemented"
        );
    }
}

// ---- The reverse-exec tool (slice 6) ------------------------------------------------------

/// A sink whose reply is fixed, and which records what command it was asked to run.
struct FakeSink {
    reply: UserMachineReply,
    seen: std::sync::Mutex<Vec<String>>,
    /// The `approved` flag each `run` was handed — the machine-consent release, by call.
    approved_flags: std::sync::Mutex<Vec<bool>>,
}
impl FakeSink {
    fn new(reply: UserMachineReply) -> Arc<Self> {
        Arc::new(Self {
            reply,
            seen: std::sync::Mutex::new(Vec::new()),
            approved_flags: std::sync::Mutex::new(Vec::new()),
        })
    }
}
#[async_trait]
impl UserMachineSink for FakeSink {
    async fn decide(&self, _account_id: &AccountId, _command: &str) -> UserMachineVerdict {
        match &self.reply {
            UserMachineReply::Ran(_) => UserMachineVerdict::Allow,
            UserMachineReply::Refused(why) => UserMachineVerdict::Deny(why.clone()),
            UserMachineReply::NeedsApproval => UserMachineVerdict::Ask,
        }
    }
    async fn run(
        &self,
        _account_id: &AccountId,
        command: &str,
        _call_id: &str,
        approved: bool,
    ) -> UserMachineReply {
        if let Ok(mut seen) = self.seen.lock() {
            seen.push(command.to_string());
        }
        if let Ok(mut flags) = self.approved_flags.lock() {
            flags.push(approved);
        }
        self.reply.clone()
    }
}

fn no_box_context() -> ToolContext {
    ToolContext {
        account_id: AccountId::from_stored("acct_1"),
        coworker_id: CoworkerId::from_stored("cw_1"),
        box_id: None,
        group_box: None,
        screen_hold: false,
        screen_held_in: None,
    }
}

#[tokio::test]
async fn user_machine_shell_is_offered_only_when_a_machine_is_attached() {
    let without = allowing(Arc::new(SpyComputer::default()));
    assert!(!without.tool_names().iter().any(|n| n == USER_MACHINE_SHELL));
    let schemas = without.tool_schemas(
        &AccountId::from_stored("acct_1"),
        &CoworkerId::from_stored("cw_1"),
    );
    assert!(
        !schemas
            .iter()
            .any(|s| s["function"]["name"] == USER_MACHINE_SHELL)
    );

    let with = allowing(Arc::new(SpyComputer::default()))
        .with_user_machine(FakeSink::new(UserMachineReply::Ran("exit 0".into())));
    assert!(with.tool_names().iter().any(|n| n == USER_MACHINE_SHELL));
    let schemas = with.tool_schemas(
        &AccountId::from_stored("acct_1"),
        &CoworkerId::from_stored("cw_1"),
    );
    assert!(
        schemas
            .iter()
            .any(|s| s["function"]["name"] == USER_MACHINE_SHELL)
    );
}

#[tokio::test]
async fn user_machine_shell_is_offered_and_runs_even_when_the_grant_omits_it() {
    // The real bug: a coworker's grant lists only the box tools, so the per-coworker policy would
    // DENY "user_machine_shell". The reverse-exec tool must be authorized by the local-exec
    // policy (the sink), not the grant — so it is still offered AND still runs.
    let restrictive = opengrok_policy::Context {
        grant: Some(opengrok_policy::Grant {
            principal: AccountId::from_stored("acct_1"),
            coworker: CoworkerId::from_stored("cw_1"),
            profile: opengrok_policy::ToolSet::only(["read_file", "shell", "write_file"]),
            needs_approval: opengrok_policy::ToolSet::None,
            revoked: false,
        }),
        ceiling: Some(opengrok_policy::Ceiling {
            coworker: CoworkerId::from_stored("cw_1"),
            tools: opengrok_policy::ToolSet::only(["read_file", "shell", "write_file"]),
        }),
    };
    let sink = FakeSink::new(UserMachineReply::Ran("exit 0".into()));
    let executor = Executor::with_policy(Arc::new(SpyComputer::default()), restrictive)
        .with_user_machine(sink.clone());

    // Offered despite the grant omitting it.
    let schemas = executor.tool_schemas(
        &AccountId::from_stored("acct_1"),
        &CoworkerId::from_stored("cw_1"),
    );
    assert!(
        schemas
            .iter()
            .any(|s| s["function"]["name"] == USER_MACHINE_SHELL),
        "reverse-exec tool must be offered even when the grant lists only box tools"
    );
    // And it RUNS (routes to the sink) rather than being refused by the grant.
    let result = executor
        .execute(
            &no_box_context(),
            &call(USER_MACHINE_SHELL, json!({"command": "mkdir ~/Code/x"})),
        )
        .await;
    assert!(result.ok, "{result:?}");
    assert_eq!(
        sink.seen.lock().unwrap().as_slice(),
        &["mkdir ~/Code/x".to_string()]
    );
}

#[tokio::test]
async fn user_machine_shell_routes_the_command_to_the_sink_without_a_box() {
    let sink = FakeSink::new(UserMachineReply::Ran(
        "exit 0\n--- stdout ---\nuriah\n".into(),
    ));
    let executor = allowing(Arc::new(SpyComputer::default())).with_user_machine(sink.clone());
    // No box on the context — the reverse-exec tool must not need one.
    let result = executor
        .execute(
            &no_box_context(),
            &call(USER_MACHINE_SHELL, json!({"command": "whoami"})),
        )
        .await;
    assert!(result.ok, "{result:?}");
    assert!(result.content.contains("uriah"));
    assert_eq!(
        sink.seen.lock().unwrap().as_slice(),
        &["whoami".to_string()]
    );
}

#[tokio::test]
async fn user_machine_shell_suspends_the_run_when_the_owner_must_approve() {
    let executor = allowing(Arc::new(SpyComputer::default()))
        .with_user_machine(FakeSink::new(UserMachineReply::NeedsApproval));
    let result = executor
        .execute(
            &no_box_context(),
            &call(USER_MACHINE_SHELL, json!({"command": "rm -rf x"})),
        )
        .await;
    assert!(!result.ok);
    assert!(result.awaiting_approval, "an Ask must suspend the run");
}

#[tokio::test]
async fn user_machine_shell_relays_a_refusal_from_the_gate() {
    let executor = allowing(Arc::new(SpyComputer::default())).with_user_machine(FakeSink::new(
        UserMachineReply::Refused("a deny rule matched".into()),
    ));
    let result = executor
        .execute(
            &no_box_context(),
            &call(USER_MACHINE_SHELL, json!({"command": "rm -rf /"})),
        )
        .await;
    assert!(!result.ok);
    assert!(!result.awaiting_approval);
    assert!(result.content.contains("deny rule"));
}

#[tokio::test]
async fn user_machine_shell_refuses_cleanly_when_no_sink_is_attached() {
    // Offered-set == executed-set: it is never offered without a sink, but if a stale call
    // arrives it refuses rather than pretending, and never touches the bot's box.
    let executor = allowing(Arc::new(SpyComputer::default()));
    let result = executor
        .execute(
            &no_box_context(),
            &call(USER_MACHINE_SHELL, json!({"command": "whoami"})),
        )
        .await;
    assert!(!result.ok);
    assert!(!result.awaiting_approval);
}

// ---- Auto-review at the seam ---------------------------------------------------------------

/// A judge with a fixed word, recording what it was shown.
struct CountingJudge {
    verdict: ReviewVerdict,
    shown: std::sync::Mutex<Vec<String>>,
}
impl CountingJudge {
    fn new(verdict: ReviewVerdict) -> Arc<Self> {
        Arc::new(Self {
            verdict,
            shown: std::sync::Mutex::new(Vec::new()),
        })
    }
    fn calls(&self) -> usize {
        self.shown.lock().map(|shown| shown.len()).unwrap_or(0)
    }
    fn first_shown(&self) -> String {
        self.shown
            .lock()
            .ok()
            .and_then(|shown| shown.first().cloned())
            .unwrap_or_default()
    }
}
#[async_trait]
impl ReviewJudge for CountingJudge {
    async fn judge(&self, ask: ReviewAsk<'_>) -> ReviewVerdict {
        if let Ok(mut shown) = self.shown.lock() {
            shown.push(ask.arguments.to_string());
        }
        self.verdict
    }
}

fn blocking_policy() -> ReviewPolicy {
    ReviewPolicy {
        allow_instructions: String::new(),
        block_instructions: "never touch prod".to_string(),
    }
}

fn shell_call(id: &str) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        name: "shell".to_string(),
        arguments: json!({ "command": "ls" }),
    }
}

fn machine_call() -> ToolCall {
    ToolCall {
        id: "m1".to_string(),
        name: USER_MACHINE_SHELL.to_string(),
        arguments: json!({ "command": "uname" }),
    }
}

fn sink_saw_nothing(sink: &FakeSink) -> bool {
    sink.seen
        .lock()
        .map(|seen| seen.is_empty())
        .unwrap_or(false)
}

#[tokio::test]
async fn an_inactive_policy_never_calls_the_judge() {
    let spy = Arc::new(SpyComputer::default());
    let judge = CountingJudge::new(ReviewVerdict::Block);
    let executor = allowing(spy.clone()).with_auto_review(ReviewPolicy::default(), judge.clone());
    let result = executor
        .execute(&context_with_box("box_mine"), &shell_call("c1"))
        .await;
    assert!(result.ok, "{result:?}");
    assert_eq!(judge.calls(), 0, "nothing written ⇒ no judge call");
    assert_eq!(spy.last_box().as_deref(), Some("box_mine"));
}

#[tokio::test]
async fn egress_tunnel_asks_before_computer_use() {
    let spy = Arc::new(SpyComputer::default());
    let executor = allowing(spy.clone())
        .with_screen(true)
        .with_egress_tunnel(true);
    let result = executor
        .execute(
            &context_with_box("box_mine"),
            &call(
                "computer",
                json!({ "action": "click", "coordinate": [120, 40] }),
            ),
        )
        .await;
    assert!(result.awaiting_approval, "{result:?}");
    assert_eq!(result.awaiting_reason, Some(AwaitingReason::AutoReview));
    assert!(result.content.contains("egress tunnel"), "{result:?}");
    assert_eq!(spy.last_box(), None, "must not run before Review an action");
}

/// Looking at the box's own screen sends nothing through the person's network, so the
/// tunnel's card is not raised for it (#165). The issue's own arguments: every field set,
/// `action` says screenshot.
#[tokio::test]
async fn egress_tunnel_lets_a_screenshot_through_without_a_card() {
    let spy = Arc::new(SpyComputer::default());
    let executor = allowing(spy.clone())
        .with_screen(true)
        .with_egress_tunnel(true);
    let result = executor
        .execute(
            &context_with_box("box_mine"),
            &call(
                "computer",
                json!({"to": [0, 0], "key": "", "text": "", "action": "screenshot",
                       "button": 1, "scroll": [0, 0], "coordinate": [0, 0]}),
            ),
        )
        .await;
    assert!(!result.awaiting_approval, "{result:?}");
    assert_eq!(result.awaiting_reason, None);
}

/// Only a screenshot is exempt. Every action that acts can navigate, submit or load (a hover
/// prefetches, a scroll lazy-loads), and anything the gate cannot read as a screenshot asks.
#[tokio::test]
async fn egress_tunnel_still_asks_for_every_screen_action_that_acts() {
    let mut asks: Vec<(&str, Value)> = [
        "click",
        "left_click",
        "right_click",
        "double_click",
        "move",
        "drag",
        "type",
        "key",
        "scroll",
        "zoom",
        "Screenshot",
    ]
    .into_iter()
    .map(|action| {
        (
            "computer",
            json!({ "action": action, "coordinate": [1, 2] }),
        )
    })
    .collect();
    asks.extend([
        ("computer", json!({})),
        ("computer", json!({ "action": ["screenshot"] })),
        ("computer", json!("screenshot")),
        ("open_url", json!({ "url": "https://example.com" })),
        (RUN_RECIPE, json!({ "recipe": "r1" })),
    ]);
    for (tool, arguments) in asks {
        let spy = Arc::new(SpyComputer::default());
        let executor = allowing(spy.clone())
            .with_screen(true)
            .with_egress_tunnel(true);
        let result = executor
            .execute(
                &context_with_box("box_mine"),
                &call(tool, arguments.clone()),
            )
            .await;
        assert!(result.awaiting_approval, "{tool} {arguments}: {result:?}");
        assert_eq!(
            result.awaiting_reason,
            Some(AwaitingReason::AutoReview),
            "{tool} {arguments}"
        );
        assert!(result.content.contains("egress tunnel"), "{result:?}");
        assert_eq!(spy.last_box(), None, "{tool} {arguments}");
    }
}

/// The "waking" frame agrees with the gate: a screenshot the tunnel no longer asks about
/// reaches the box, so it wakes it; a click parks on the card first, so nothing wakes. A
/// held screen still holds a screenshot — a handoff's secret may be on it.
#[tokio::test]
async fn a_screenshot_under_the_tunnel_wakes_the_box() {
    let sleepy = Arc::new(SleepyComputer::new(&["archived"]));
    let executor = allowing(sleepy.clone())
        .with_screen(true)
        .with_egress_tunnel(true);
    let context = context_with_box("box_mine");
    assert!(
        executor
            .box_needs_wake(&context, &call("computer", json!({"action": "screenshot"})))
            .await
    );
    assert!(
        !executor
            .box_needs_wake(
                &context,
                &call("computer", json!({"action": "click", "coordinate": [1, 2]}))
            )
            .await
    );
    let mut holding = context_with_box("box_mine");
    holding.screen_hold = true;
    assert!(
        !executor
            .box_needs_wake(&holding, &call("computer", json!({"action": "screenshot"})))
            .await
    );
    let refused = executor
        .execute(&holding, &call("computer", json!({"action": "screenshot"})))
        .await;
    assert!(!refused.ok && !refused.awaiting_approval, "{refused:?}");
    assert_eq!(sleepy.resumes(), 0);
}

/// The second tunnel ask, after a wake, draws the same line.
#[tokio::test]
async fn after_wake_mode_asks_for_a_click_but_not_a_screenshot() {
    let spy = Arc::new(SpyComputer {
        tunnel_ready: true,
        ..SpyComputer::default()
    });
    let executor = allowing(spy.clone())
        .with_screen(true)
        .with_egress_tunnel_mode(EgressTunnelMode::AskTheBoxAfterWake);
    let context = context_with_box("box_mine");
    let look = executor
        .execute(&context, &call("computer", json!({"action": "screenshot"})))
        .await;
    assert!(!look.awaiting_approval, "{look:?}");
    let click = executor
        .execute(
            &context,
            &call("computer", json!({"action": "click", "coordinate": [1, 2]})),
        )
        .await;
    assert!(click.awaiting_approval, "{click:?}");
    assert_eq!(click.awaiting_reason, Some(AwaitingReason::AutoReview));
}

#[tokio::test]
async fn egress_tunnel_lets_an_approved_computer_call_through() {
    let spy = Arc::new(SpyComputer::default());
    let executor = allowing(spy.clone())
        .with_screen(true)
        .with_egress_tunnel(true)
        .with_review_approved(["call_1".to_string()]);
    let result = executor
        .execute(
            &context_with_box("box_mine"),
            &call("computer", json!({ "action": "screenshot" })),
        )
        .await;
    assert!(!result.awaiting_approval, "{result:?}");
    assert_eq!(result.awaiting_reason, None);
}

/// One yes per run: the card for the page is not followed by a card for the screenshot and
/// another for the click. The resume path marks the run consented from the answered card's
/// own tool; a review yes alone, for some other call, is not consent.
#[tokio::test]
async fn egress_consent_given_once_holds_for_the_rest_of_the_run() {
    let spy = Arc::new(SpyComputer::default());
    let unrelated_yes = allowing(spy.clone())
        .with_screen(true)
        .with_egress_tunnel(true)
        .with_review_approved(["call_page".to_string()]);
    let context = context_with_box("box_mine");
    let asked = unrelated_yes
        .execute(
            &context,
            &ToolCall {
                id: "call_click".to_string(),
                name: "computer".to_string(),
                arguments: json!({ "action": "click", "coordinate": [120, 40] }),
            },
        )
        .await;
    assert!(
        asked.awaiting_approval,
        "a yes for another call is not consent: {asked:?}"
    );

    let executor = allowing(spy.clone())
        .with_screen(true)
        .with_egress_tunnel(true)
        .with_egress_consented(true);
    let context = context_with_box("box_mine");
    let later = ToolCall {
        id: "call_click".to_string(),
        name: "computer".to_string(),
        arguments: json!({ "action": "click", "coordinate": [120, 40] }),
    };
    let result = executor.execute(&context, &later).await;
    assert!(!result.awaiting_approval, "asked again: {result:?}");
    assert_eq!(result.awaiting_reason, None);
}

/// The person answered in advance for this computer: no card, the call runs.
#[tokio::test]
async fn a_standing_always_skips_the_tunnel_card() {
    let spy = Arc::new(SpyComputer::default());
    let executor = allowing(spy.clone())
        .with_screen(true)
        .with_egress_tunnel(true)
        .with_egress_policy(EgressPolicy::Always);
    let result = executor
        .execute(
            &context_with_box("box_mine"),
            &call("computer", json!({ "action": "screenshot" })),
        )
        .await;
    assert!(!result.awaiting_approval, "{result:?}");
    assert_eq!(result.awaiting_reason, None);
    assert!(!result.content.contains("switched off"), "{result:?}");
}

/// `Never` with the tunnel on: the screen tools are not on offer, the prompt is told, and a
/// call that arrives anyway is refused in words without touching the box.
#[tokio::test]
async fn a_standing_never_withholds_the_leave_box_tools() {
    let spy = Arc::new(SpyComputer::default());
    let executor = allowing(spy.clone())
        .with_screen(true)
        .with_egress_tunnel(true)
        .with_egress_policy(EgressPolicy::Never);
    assert!(executor.network_off());
    assert!(!executor.has_screen());
    let offered = executor.tool_names();
    for gone in BROWSER_TOOLS {
        if *gone == REQUEST_USER_FORM {
            continue;
        }
        assert!(!offered.iter().any(|name| name == gone), "{offered:?}");
    }
    assert!(
        offered.iter().any(|name| name == REQUEST_USER_FORM),
        "a collect form does not need the network: {offered:?}"
    );
    assert!(offered.iter().any(|name| name == "shell"), "{offered:?}");
    let result = executor
        .execute(
            &context_with_box("box_mine"),
            &call("open_url", json!({ "url": "https://example.com" })),
        )
        .await;
    assert!(!result.ok, "{result:?}");
    assert!(!result.awaiting_approval, "{result:?}");
    assert!(result.content.contains("switched off"), "{result:?}");
    assert_eq!(spy.last_box(), None, "must not wake or touch the box");
    let login = executor
        .execute(
            &context_with_box("box_mine"),
            &call(
                REQUEST_USER_FORM,
                json!({"title": "Sign in", "fields": [{"id": "p", "label": "Password"}]}),
            ),
        )
        .await;
    assert!(!login.ok, "{login:?}");
    assert!(login.content.contains("switched off"), "{login:?}");
    let collect = executor
        .execute(
            &context_with_box("box_mine"),
            &call(
                REQUEST_USER_FORM,
                json!({
                    "collect": true,
                    "title": "New tax profile",
                    "fields": [{"id": "name", "label": "Name", "value": "Juana Jane"}]
                }),
            ),
        )
        .await;
    assert!(collect.awaiting_approval, "{collect:?}");
    assert_eq!(
        spy.last_box(),
        None,
        "a collect card must not touch the box"
    );
    // The box's own shell is not the person's network: it still runs.
    let shell = executor
        .execute(&context_with_box("box_mine"), &shell_call("c1"))
        .await;
    assert!(shell.ok, "{shell:?}");
}

/// With the tunnel off the policy is moot: `Never` keeps the screen, because nothing would
/// carry the box's traffic through the person's network.
#[tokio::test]
async fn a_standing_never_means_nothing_without_a_tunnel() {
    let spy = Arc::new(SpyComputer::default());
    let executor = allowing(spy.clone())
        .with_screen(true)
        .with_egress_policy(EgressPolicy::Never);
    assert!(!executor.network_off());
    assert!(executor.has_screen());
    let offered = executor.tool_names();
    assert!(offered.iter().any(|name| name == "computer"), "{offered:?}");
    let result = executor
        .execute(
            &context_with_box("box_mine"),
            &call("computer", json!({ "action": "screenshot" })),
        )
        .await;
    assert!(!result.content.contains("switched off"), "{result:?}");
}

/// Host intent with the box asleep is not a tunnel: `never` offers the tools as `ask`
/// would, and only refuses once the woken guest says a tunnel is really attached.
#[tokio::test]
async fn a_standing_never_waits_for_the_guest_when_the_box_was_asleep() {
    let spy = Arc::new(SpyComputer::default());
    let executor = allowing(spy.clone())
        .with_screen(true)
        .with_egress_tunnel_mode(EgressTunnelMode::AskTheBoxAfterWake)
        .with_egress_policy(EgressPolicy::Never);
    assert!(!executor.network_off());
    assert!(executor.has_screen());
    let offered = executor.tool_names();
    assert!(offered.iter().any(|name| name == "computer"), "{offered:?}");
    // The spy's guest advertises no tunnel, so the call goes through.
    let result = executor
        .execute(
            &context_with_box("box_mine"),
            &call("computer", json!({ "action": "screenshot" })),
        )
        .await;
    assert!(!result.awaiting_approval, "{result:?}");
    assert!(!result.content.contains("switched off"), "{result:?}");
}

/// The box was asleep at turn start and the guest, asked after the wake, says a tunnel IS
/// attached: under `never` the browser call is refused in words, where `ask` would raise
/// the card. `run_recipe` is a browser sequence and is caught the same way.
#[tokio::test]
async fn a_standing_never_refuses_once_the_woken_guest_says_the_tunnel_is_there() {
    let spy = Arc::new(SpyComputer {
        tunnel_ready: true,
        ..SpyComputer::default()
    });
    let executor = allowing(spy.clone())
        .with_screen(true)
        .with_egress_tunnel_mode(EgressTunnelMode::AskTheBoxAfterWake)
        .with_egress_policy(EgressPolicy::Never);
    assert!(!executor.network_off(), "not known before the wake");
    assert!(executor.network_off_now("box_mine").await);
    for (tool, args) in [
        ("computer", json!({ "action": "screenshot" })),
        ("open_url", json!({ "url": "https://example.com" })),
        (RUN_RECIPE, json!({ "recipe": "r1" })),
    ] {
        let result = executor
            .execute(&context_with_box("box_mine"), &call(tool, args))
            .await;
        assert!(
            !result.awaiting_approval,
            "{tool}: no card under never: {result:?}"
        );
        assert!(
            result.content.contains("switched off"),
            "{tool}: {result:?}"
        );
    }
    // The same state under `ask` is the card, as before.
    let asking = allowing(spy.clone())
        .with_screen(true)
        .with_egress_tunnel_mode(EgressTunnelMode::AskTheBoxAfterWake);
    let result = asking
        .execute(
            &context_with_box("box_mine"),
            &call(
                "computer",
                json!({ "action": "click", "coordinate": [1, 2] }),
            ),
        )
        .await;
    assert!(result.awaiting_approval, "{result:?}");
}

/// The two hand-off cards are browser tools too: under `never` they are refused before a
/// person could be asked to type a password for a box that cannot browse.
#[tokio::test]
async fn a_standing_never_refuses_the_login_hand_offs_before_they_ask() {
    let spy = Arc::new(SpyComputer::default());
    let executor = allowing(spy.clone())
        .with_screen(true)
        .with_egress_tunnel(true)
        .with_egress_policy(EgressPolicy::Never);
    for tool in [REQUEST_USER_FORM] {
        let result = executor
            .execute(
                &context_with_box("box_mine"),
                &call(
                    tool,
                    json!({ "origin": "https://example.com", "fields": [] }),
                ),
            )
            .await;
        assert!(!result.awaiting_approval, "{tool}: {result:?}");
        assert!(
            result.content.contains("switched off"),
            "{tool}: {result:?}"
        );
    }
}

#[test]
fn egress_policy_words_round_trip_and_unknown_reads_as_ask() {
    for policy in [EgressPolicy::Always, EgressPolicy::Ask, EgressPolicy::Never] {
        assert_eq!(EgressPolicy::from_stored(policy.as_stored()), policy);
        assert!(EgressPolicy::is_valid(policy.as_stored()));
    }
    assert_eq!(EgressPolicy::from_stored("sometimes"), EgressPolicy::Ask);
    assert!(!EgressPolicy::is_valid("sometimes"));
}

#[tokio::test]
async fn egress_tunnel_does_not_ask_for_box_local_shell() {
    let spy = Arc::new(SpyComputer::default());
    let executor = allowing(spy.clone()).with_egress_tunnel(true);
    let result = executor
        .execute(&context_with_box("box_mine"), &shell_call("c1"))
        .await;
    assert!(result.ok, "{result:?}");
    assert!(!result.awaiting_approval);
    assert_eq!(spy.last_box().as_deref(), Some("box_mine"));
}

#[tokio::test]
async fn an_approved_call_is_never_re_judged() {
    let spy = Arc::new(SpyComputer::default());
    let judge = CountingJudge::new(ReviewVerdict::Block);
    let executor = allowing(spy.clone())
        .with_approved(["c1".to_string()])
        .with_auto_review(blocking_policy(), judge.clone());
    let result = executor
        .execute(&context_with_box("box_mine"), &shell_call("c1"))
        .await;
    assert!(result.ok, "{result:?}");
    assert_eq!(judge.calls(), 0, "a person already answered this call");
}

#[tokio::test]
async fn a_review_block_asks_and_touches_no_box() {
    // Settings stores "Ask first" in blockInstructions; a match is a card, not a refuse.
    let spy = Arc::new(SpyComputer::default());
    let judge = CountingJudge::new(ReviewVerdict::Block);
    let executor = allowing(spy.clone()).with_auto_review(blocking_policy(), judge.clone());
    let result = executor
        .execute(&context_with_box("box_mine"), &shell_call("c1"))
        .await;
    assert!(result.awaiting_approval);
    assert_eq!(result.awaiting_reason, Some(AwaitingReason::AutoReview));
    assert!(result.content.contains("never touch prod"), "{result:?}");
    assert_eq!(spy.last_box(), None, "an ask must not reach the box");
    assert_eq!(judge.calls(), 1);
}

#[tokio::test]
async fn a_review_ask_suspends_with_the_auto_review_reason() {
    let spy = Arc::new(SpyComputer::default());
    let judge = CountingJudge::new(ReviewVerdict::Ask);
    let executor = allowing(spy.clone()).with_auto_review(blocking_policy(), judge);
    let result = executor
        .execute(&context_with_box("box_mine"), &shell_call("c1"))
        .await;
    assert!(result.awaiting_approval);
    assert_eq!(result.awaiting_reason, Some(AwaitingReason::AutoReview));
    assert_eq!(spy.last_box(), None);
}

/// The card says WHY the judge did not answer: a capped coworker's judge is refused on
/// every call, and "the reviewer did not answer" told nobody that (#201).
#[tokio::test]
async fn a_judge_outage_asks_rather_than_allows_and_names_its_cause() {
    let spy = Arc::new(SpyComputer::default());
    let judge = CountingJudge::new(ReviewVerdict::Unavailable(JudgeFailure::SpendCap));
    let executor = allowing(spy.clone()).with_auto_review(blocking_policy(), judge);
    let result = executor
        .execute(&context_with_box("box_mine"), &shell_call("c1"))
        .await;
    assert!(result.awaiting_approval);
    assert_eq!(result.awaiting_reason, Some(AwaitingReason::AutoReview));
    assert!(result.content.contains("did not answer"), "{result:?}");
    assert!(result.content.contains("spend limit"), "{result:?}");
    assert!(
        result
            .content
            .strip_prefix("waiting for approval: ")
            .is_some_and(review::is_unavailable_reason),
        "{result:?}"
    );
    assert_eq!(spy.last_box(), None);
}

/// Every cause reads differently, and none of them reads as an allow.
#[test]
fn every_judge_failure_has_its_own_words() {
    let causes = [
        JudgeFailure::SpendCap,
        JudgeFailure::Held,
        JudgeFailure::Refused(404),
        JudgeFailure::Unreachable,
        JudgeFailure::StreamBroke,
        JudgeFailure::TimedOut,
        JudgeFailure::Unparseable,
    ];
    let reasons: std::collections::BTreeSet<String> = causes
        .iter()
        .map(|cause| review::unavailable_reason(*cause))
        .collect();
    assert_eq!(reasons.len(), causes.len(), "{reasons:?}");
    assert!(review::unavailable_reason(JudgeFailure::Refused(404)).contains("404"));
    for reason in &reasons {
        assert!(review::is_unavailable_reason(reason), "{reason}");
        assert!(reason.contains("asked rather than allowed"), "{reason}");
    }
    assert!(!review::is_unavailable_reason(review::REVIEW_ASK_REASON));
    assert!(!review::is_unavailable_reason(EGRESS_TUNNEL_ASK_REASON));
}

/// A run whose judge failed `JUDGE_DOWN_AFTER` times in a row stops asking it: the next
/// reviewed call is refused in words the model can pass on, the judge is not called (a
/// capped key is not billed another attempt), and nothing reaches the box. A real verdict
/// in between starts the count again.
#[tokio::test]
async fn a_judge_that_keeps_failing_is_not_asked_again_this_run() {
    let spy = Arc::new(SpyComputer::default());
    let judge = CountingJudge::new(ReviewVerdict::Unavailable(JudgeFailure::TimedOut));
    let executor = allowing(spy.clone()).with_auto_review(blocking_policy(), judge.clone());
    let context = context_with_box("box_mine");
    for id in ["c1", "c2", "c3"] {
        let asked = executor.execute(&context, &shell_call(id)).await;
        assert!(asked.awaiting_approval, "{id}: {asked:?}");
        assert!(asked.content.contains("timed out"), "{asked:?}");
    }
    assert_eq!(judge.calls(), 3);
    let refused = executor.execute(&context, &shell_call("c4")).await;
    assert!(!refused.ok && !refused.awaiting_approval, "{refused:?}");
    assert!(refused.content.contains("reviewer is down"), "{refused:?}");
    assert_eq!(judge.calls(), 3, "the judge is not asked once it is down");
    assert_eq!(spy.last_box(), None);

    // Seeded from a resumed run's journal: already down, refused at once.
    let resumed = allowing(spy.clone())
        .with_auto_review(blocking_policy(), judge.clone())
        .with_judge_failures(review::JUDGE_DOWN_AFTER);
    let refused = resumed.execute(&context, &shell_call("c5")).await;
    assert!(refused.content.contains("reviewer is down"), "{refused:?}");
    assert_eq!(judge.calls(), 3);

    // Two failures and then an answer: the count starts again.
    let flaky = CountingJudge::new(ReviewVerdict::Ask);
    let executor = allowing(spy.clone())
        .with_auto_review(blocking_policy(), flaky.clone())
        .with_judge_failures(review::JUDGE_DOWN_AFTER - 1);
    let asked = executor.execute(&context, &shell_call("c6")).await;
    assert!(asked.awaiting_approval, "{asked:?}");
    assert_eq!(
        executor
            .judge_failures
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
}

#[tokio::test]
async fn a_denied_machine_gate_calls_neither_judge_nor_sink() {
    let sink = FakeSink::new(UserMachineReply::Refused("channel off".into()));
    let judge = CountingJudge::new(ReviewVerdict::Block);
    let executor = allowing(Arc::new(SpyComputer::default()))
        .with_user_machine(sink.clone())
        .with_auto_review(blocking_policy(), judge.clone());
    let result = executor.execute(&no_box_context(), &machine_call()).await;
    assert!(!result.ok);
    assert!(result.content.contains("channel off"), "{result:?}");
    assert_eq!(
        judge.calls(),
        0,
        "no spend on a command the channel refused"
    );
    assert!(sink_saw_nothing(&sink));
}

#[tokio::test]
async fn a_machine_ask_plus_review_ask_is_one_card_the_exec_one() {
    let sink = FakeSink::new(UserMachineReply::NeedsApproval);
    let judge = CountingJudge::new(ReviewVerdict::Ask);
    let executor = allowing(Arc::new(SpyComputer::default()))
        .with_user_machine(sink.clone())
        .with_auto_review(blocking_policy(), judge);
    let result = executor.execute(&no_box_context(), &machine_call()).await;
    assert!(result.awaiting_approval);
    assert_eq!(result.awaiting_reason, Some(AwaitingReason::ExecConsent));
    // The sink IS consulted (it records the ask and answers NeedsApproval); it dispatches
    // nothing — that is the sink's contract, and the fake's reply is fixed.
    assert_eq!(sink.seen.lock().map(|seen| seen.len()).unwrap_or(0), 1);
}

#[tokio::test]
async fn a_machine_ask_plus_review_ask_first_is_one_card_the_exec_one() {
    let sink = FakeSink::new(UserMachineReply::NeedsApproval);
    let judge = CountingJudge::new(ReviewVerdict::Block);
    let executor = allowing(Arc::new(SpyComputer::default()))
        .with_user_machine(sink.clone())
        .with_auto_review(blocking_policy(), judge);
    let result = executor.execute(&no_box_context(), &machine_call()).await;
    assert!(result.awaiting_approval);
    assert_eq!(result.awaiting_reason, Some(AwaitingReason::ExecConsent));
    assert_eq!(sink.seen.lock().map(|seen| seen.len()).unwrap_or(0), 1);
}

#[tokio::test]
async fn a_machine_allow_plus_review_ask_raises_the_review_card() {
    let sink = FakeSink::new(UserMachineReply::Ran("exit 0".into()));
    let judge = CountingJudge::new(ReviewVerdict::Ask);
    let executor = allowing(Arc::new(SpyComputer::default()))
        .with_user_machine(sink.clone())
        .with_auto_review(blocking_policy(), judge);
    let result = executor.execute(&no_box_context(), &machine_call()).await;
    assert!(result.awaiting_approval);
    assert_eq!(result.awaiting_reason, Some(AwaitingReason::AutoReview));
    assert!(sink_saw_nothing(&sink));
}

#[tokio::test]
async fn the_judge_sees_redacted_arguments() {
    let judge = CountingJudge::new(ReviewVerdict::Allow);
    let executor = allowing(Arc::new(SpyComputer::default()))
        .with_auto_review(blocking_policy(), judge.clone());
    let call = ToolCall {
        id: "c1".to_string(),
        name: "shell".to_string(),
        arguments: json!({ "command": "ls", "coworker_id": "cw_evil", "api_key": "sk-abc" }),
    };
    let result = executor.execute(&context_with_box("box_mine"), &call).await;
    assert!(result.ok, "{result:?}");
    let shown = judge.first_shown();
    assert!(shown.contains("ls"), "{shown}");
    assert!(!shown.contains("cw_evil"), "{shown}");
    assert!(!shown.contains("sk-abc"), "{shown}");
}

/// Ask-first covers every offered tool: each call suspends for a card and
/// none of them touches a machine on the way.
#[tokio::test]
async fn nothing_offered_escapes_an_ask_first_judge() {
    let spy = Arc::new(SpyComputer::default());
    let sink = FakeSink::new(UserMachineReply::Ran("exit 0".into()));
    let judge = CountingJudge::new(ReviewVerdict::Block);
    let executor = allowing(spy.clone())
        .with_user_machine(sink.clone())
        .with_auto_review(blocking_policy(), judge);
    let context = context_with_box("box_mine");
    for name in executor.tool_names() {
        let result = executor
            .execute(
                &context,
                &call(
                    &name,
                    json!({
                        "command": "ls",
                        "path": "/tmp/a",
                        "content": "x",
                        "origin": "accounts.google.com"
                    }),
                ),
            )
            .await;
        assert!(
            result.awaiting_approval,
            "{name} ran without a card: {result:?}"
        );
    }
    assert_eq!(spy.last_box(), None);
    assert!(sink_saw_nothing(&sink));
}

/// A review-card approval skips the judge and NOTHING else: if the machine now asks for its
/// owner's consent, the sink is not told the call was approved.
#[tokio::test]
async fn a_review_approval_never_releases_the_machines_own_consent() {
    let sink = FakeSink::new(UserMachineReply::NeedsApproval);
    let judge = CountingJudge::new(ReviewVerdict::Block);
    let executor = allowing(Arc::new(SpyComputer::default()))
        .with_user_machine(sink.clone())
        .with_review_approved(["m1".to_string()])
        .with_auto_review(blocking_policy(), judge.clone());
    let result = executor.execute(&no_box_context(), &machine_call()).await;
    assert_eq!(judge.calls(), 0, "the review answer stands");
    assert!(result.awaiting_approval);
    assert_eq!(result.awaiting_reason, Some(AwaitingReason::ExecConsent));
    let flags = sink
        .approved_flags
        .lock()
        .map(|f| f.clone())
        .unwrap_or_default();
    assert_eq!(
        flags,
        vec![false],
        "the sink must not be told the owner consented"
    );
}

fn computer_args(value: Value) -> Result<Option<CuaAction>, String> {
    serde_json::from_value::<ComputerArgs>(value)
        .map_err(|error| error.to_string())?
        .into_action()
}

#[test]
fn computer_args_map_onto_the_boxs_actions() {
    assert_eq!(computer_args(json!({"action": "screenshot"})), Ok(None));
    assert_eq!(
        computer_args(json!({"action": "click", "coordinate": [17, 781]})),
        Ok(Some(CuaAction::Click {
            x: 17,
            y: 781,
            button: None
        }))
    );
    assert_eq!(
        computer_args(json!({"action": "right_click", "coordinate": [1, 2]})),
        Ok(Some(CuaAction::Click {
            x: 1,
            y: 2,
            button: Some(3)
        }))
    );
    assert_eq!(
        computer_args(json!({"action": "drag", "coordinate": [1, 2], "to": [3, 4]})),
        Ok(Some(CuaAction::Drag {
            x1: 1,
            y1: 2,
            x2: 3,
            y2: 4
        }))
    );
    assert_eq!(
        computer_args(json!({"action": "type", "text": "uname -a\n"})),
        Ok(Some(CuaAction::Type {
            text: "uname -a\n".into()
        }))
    );
    assert_eq!(
        computer_args(json!({"action": "scroll", "coordinate": [5, 6], "scroll": [0, -3]})),
        Ok(Some(CuaAction::Scroll {
            x: 5,
            y: 6,
            dx: 0,
            dy: -3
        }))
    );
}

/// The error names the field, so the model's next call can be right.
#[test]
fn a_computer_call_missing_its_field_is_told_which() {
    let error = computer_args(json!({"action": "click"})).unwrap_err();
    assert!(error.contains("`coordinate`"), "{error}");
    let error = computer_args(json!({"action": "type"})).unwrap_err();
    assert!(error.contains("`text`"), "{error}");
    let error = computer_args(json!({"action": "teleport"})).unwrap_err();
    assert!(error.contains("unknown action `teleport`"), "{error}");
}

/// The screen tools are offered only where there is a screen — the offered set and the
/// executed set stay equal either way.
#[test]
fn screen_tools_are_offered_only_with_a_screen() {
    let spy = Arc::new(SpyComputer::default());
    let headless = allowing(spy.clone());
    let names = headless.tool_names();
    assert!(!names.iter().any(|name| name == "computer"), "{names:?}");
    assert!(!names.iter().any(|name| name == "open_url"), "{names:?}");
    assert!(
        names.iter().any(|name| name == REQUEST_USER_FORM),
        "the form tool does not need a display: {names:?}"
    );
    assert!(!headless.has_screen());

    let with_screen = allowing(spy).with_screen(true);
    let names = with_screen.tool_names();
    assert!(names.iter().any(|name| name == "computer"), "{names:?}");
    assert!(names.iter().any(|name| name == "open_url"), "{names:?}");
    assert!(with_screen.has_screen());
}

/// A provider without a desktop refuses in words the model can act on, and nothing panics.
#[tokio::test]
async fn a_screen_action_on_a_headless_box_is_refused() {
    let executor = allowing(Arc::new(SpyComputer::default())).with_screen(true);
    let context = context_with_box("box_mine");

    let result = executor
        .execute(
            &context,
            &call("computer", json!({"action": "click", "coordinate": [1, 1]})),
        )
        .await;
    assert!(!result.ok, "{result:?}");
    assert!(result.content.contains("no screen"), "{result:?}");
    assert!(result.image.is_none());
}

/// A room's shared computer is a second target for the same tools, chosen per call.
#[tokio::test]
async fn machine_group_aims_a_call_at_the_rooms_box() {
    let spy = Arc::new(SpyComputer::default());
    let mut executor = allowing(spy.clone());
    executor.set_group_box_name("Finance desk");
    let mut context = context_with_box("box_mine");
    context.group_box = Some(GroupBox {
        box_id: BoxId::from_stored("box_room"),
        name: "Finance desk".into(),
    });

    let result = executor
        .execute(
            &context,
            &call("shell", json!({"command": "ls", "machine": "group"})),
        )
        .await;
    assert!(result.ok, "{result:?}");
    assert_eq!(spy.last_box().as_deref(), Some("box_room"));

    // Without `machine`, the coworker's own box — the default nobody has to spell.
    let result = executor
        .execute(&context, &call("shell", json!({"command": "ls"})))
        .await;
    assert!(result.ok, "{result:?}");
    assert_eq!(spy.last_box().as_deref(), Some("box_mine"));
}

#[tokio::test]
async fn machine_group_without_a_room_box_is_refused_in_words() {
    let executor = allowing(Arc::new(SpyComputer::default()));
    let context = context_with_box("box_mine");
    let result = executor
        .execute(
            &context,
            &call("shell", json!({"command": "ls", "machine": "group"})),
        )
        .await;
    assert!(!result.ok);
    assert!(
        result.content.contains("no shared group computer"),
        "{result:?}"
    );
}

/// The schema says `machine` exists only when there is a room box to aim at.
#[test]
fn the_box_tools_offer_machine_only_in_a_room_with_a_box() {
    let account = AccountId::from_stored("acct_1");
    let coworker = CoworkerId::from_stored("cw_1");
    let plain = allowing(Arc::new(SpyComputer::default()));
    let shell = plain
        .tool_schemas(&account, &coworker)
        .into_iter()
        .find(|schema| schema["function"]["name"] == "shell")
        .unwrap();
    assert!(shell["function"]["parameters"]["properties"]["machine"].is_null());

    let mut in_room = allowing(Arc::new(SpyComputer::default()));
    in_room.set_group_box_name("Finance desk");
    let shell = in_room
        .tool_schemas(&account, &coworker)
        .into_iter()
        .find(|schema| schema["function"]["name"] == "shell")
        .unwrap();
    let machine = &shell["function"]["parameters"]["properties"]["machine"];
    assert_eq!(machine["enum"], json!(["mine", "group"]));
    assert!(
        machine["description"]
            .as_str()
            .unwrap()
            .contains("Finance desk")
    );
}

/// The registry as the executor sees it: steps out, runs written down.
#[derive(Default)]
struct SpyRecipes {
    runs: Mutex<Vec<(String, i32, bool)>>,
    /// Which recipe was asked for, and with which values — the point of the precedence test.
    asked: Mutex<Vec<(String, opengrok_recipes::Values)>>,
    /// A recipe whose share was taken back after the turn's offers were read.
    withdrawn: Option<&'static str>,
    /// What `claim_run` answers: `None` keeps no leases, `Ok(id)` claims under `id`, `Err(why)`
    /// is another run playing on the bot's screen.
    claim: Option<Result<&'static str, &'static str>>,
    /// The claimed id each `record_run` finished, in order.
    finished: Mutex<Vec<Option<String>>>,
}

#[async_trait]
impl RecipeSource for SpyRecipes {
    async fn recipe_request(
        &self,
        recipe_id: &str,
        values: &opengrok_recipes::Values,
    ) -> Result<(i32, Value), String> {
        if let Ok(mut asked) = self.asked.lock() {
            asked.push((recipe_id.to_string(), values.clone()));
        }
        match recipe_id {
            "rcp_gmail" => Ok((
                2,
                json!({"name": "Open Gmail", "steps": [], "stop_on_error": true, "screenshot": "end"}),
            )),
            "rcp_stops" => Ok((
                3,
                json!({"name": "stops", "steps": [], "stop_on_error": true, "screenshot": "end"}),
            )),
            "rcp_search" => Ok((
                1,
                json!({"name": "Search", "steps": [], "stop_on_error": true, "screenshot": "end"}),
            )),
            other => Err(format!("recipe `{other}` is gone")),
        }
    }
    async fn record_run(
        &self,
        recipe_id: &str,
        version: i32,
        _by: &CoworkerId,
        receipt: &RecipeReceipt,
        claimed: Option<&str>,
    ) -> Option<String> {
        if let Ok(mut runs) = self.runs.lock() {
            runs.push((recipe_id.to_string(), version, receipt.ok));
        }
        if let Ok(mut finished) = self.finished.lock() {
            finished.push(claimed.map(str::to_string));
        }
        Some(format!("rrun_spy_{recipe_id}"))
    }
    async fn claim_run(
        &self,
        _recipe_id: &str,
        _version: i32,
        _by: &CoworkerId,
    ) -> Result<Option<RecipeClaim>, String> {
        match self.claim {
            None => Ok(None),
            Some(Ok(id)) => Ok(Some(RecipeClaim {
                id: id.to_string(),
                hold: Box::new(()),
            })),
            Some(Err(why)) => Err(why.to_string()),
        }
    }
    async fn still_granted(&self, recipe_id: &str, _by: &CoworkerId) -> Result<(), String> {
        if self.withdrawn == Some(recipe_id) {
            return Err(format!("recipe `{recipe_id}` was taken back"));
        }
        Ok(())
    }
}

fn offers() -> Vec<RecipeOffer> {
    vec![
        RecipeOffer {
            id: "rcp_gmail".into(),
            name: "Open Gmail".into(),
            description: "Open Gmail in Chrome and land on the inbox".into(),
            parameters: vec![],
        },
        RecipeOffer {
            id: "rcp_stops".into(),
            name: "Stops".into(),
            description: "a recipe whose second step fails".into(),
            parameters: vec![],
        },
        RecipeOffer {
            id: "rcp_search".into(),
            name: "Search".into(),
            description: "Search for a term on a website".into(),
            parameters: vec![opengrok_recipes::Parameter {
                name: "search_term".into(),
                description: "What to search for".into(),
                required: true,
                kind: opengrok_recipes::ParameterKind::Text,
                default: None,
                values: None,
            }],
        },
    ]
}

#[test]
fn run_recipe_is_offered_only_with_a_screen_and_a_grant() {
    let spy = Arc::new(SpyComputer::default());
    let source: Arc<dyn RecipeSource> = Arc::new(SpyRecipes::default());

    let no_grants = allowing(spy.clone()).with_screen(true);
    assert!(!no_grants.tool_names().iter().any(|n| n == RUN_RECIPE));
    assert!(!no_grants.has_recipes());

    let headless = allowing(spy.clone()).with_recipes(offers(), source.clone());
    assert!(
        !headless.tool_names().iter().any(|n| n == RUN_RECIPE),
        "no screen ⇒ nothing to run on"
    );
    assert!(!headless.has_recipes());

    let granted = allowing(spy)
        .with_screen(true)
        .with_recipes(offers(), source);
    assert!(granted.tool_names().iter().any(|n| n == RUN_RECIPE));
    assert!(granted.has_recipes());
    // The schema lists the granted recipes by id and in words, and pins the id to the grant.
    let schema = serde_json::to_string(&granted.tool_schemas(
        &AccountId::from_stored("acct_1"),
        &CoworkerId::from_stored("cw_1"),
    ))
    .unwrap();
    assert!(schema.contains("rcp_gmail"), "{schema}");
    assert!(
        schema.contains("Open Gmail in Chrome and land on the inbox"),
        "{schema}"
    );
    assert!(schema.contains("\"enum\""), "{schema}");
}

#[tokio::test]
async fn a_recipe_run_carries_the_receipt_and_the_screenshot() {
    let spy = Arc::new(SpyComputer::default());
    let recipes = Arc::new(SpyRecipes::default());
    let executor = allowing(spy.clone())
        .with_screen(true)
        // Nobody asked the box to watch, so this is the result a model has been reading since
        // before observation existed, and it must not have grown a word.
        .with_observe(Observe::Off)
        .with_recipes(offers(), recipes.clone());
    let context = context_with_box("box_mine");

    let result = executor
        .execute(&context, &call(RUN_RECIPE, json!({"recipe": "rcp_gmail"})))
        .await;
    assert!(result.ok, "{result:?}");
    assert!(result.content.contains("Open Gmail"), "{result:?}");
    assert!(result.content.contains("v2"), "{result:?}");
    assert!(!result.content.contains("the box"), "{result:?}");
    assert!(
        result.image.is_some(),
        "the end screenshot rides the result"
    );
    assert_eq!(spy.last_box().as_deref(), Some("box_mine"));
    assert_eq!(
        recipes.runs.lock().unwrap().as_slice(),
        &[("rcp_gmail".to_string(), 2, true)],
        "the run is written down"
    );
}

/// The offers were read when the turn began; the grant is read again when the recipe plays.
#[tokio::test]
async fn a_recipe_taken_back_mid_turn_is_refused_before_the_box_is_touched() {
    let spy = Arc::new(SpyComputer::default());
    let recipes = Arc::new(SpyRecipes {
        withdrawn: Some("rcp_gmail"),
        ..SpyRecipes::default()
    });
    let executor = allowing(spy.clone())
        .with_screen(true)
        .with_recipes(offers(), recipes.clone());
    let result = executor
        .execute(
            &context_with_box("box_mine"),
            &call(RUN_RECIPE, json!({"recipe": "rcp_gmail"})),
        )
        .await;
    assert!(!result.ok, "{result:?}");
    assert!(result.content.contains("taken back"), "{result:?}");
    assert!(spy.last_box().is_none(), "nothing was played");
    assert!(recipes.runs.lock().unwrap().is_empty());
    assert!(recipes.asked.lock().unwrap().is_empty());
}

/// #227: a chat play is refused while a run started from the page holds the bot's screen, and
/// the box is never asked: two recipes clicking on one screen is neither's recipe.
#[tokio::test]
async fn a_recipe_is_refused_while_another_run_holds_the_bots_screen() {
    let spy = Arc::new(SpyComputer::default());
    let recipes = Arc::new(SpyRecipes {
        claim: Some(Err(
            "this bot is already playing a recipe; wait for that run to finish",
        )),
        ..SpyRecipes::default()
    });
    let executor = allowing(spy.clone())
        .with_screen(true)
        .with_recipes(offers(), recipes.clone());
    let result = executor
        .execute(
            &context_with_box("box_mine"),
            &call(RUN_RECIPE, json!({"recipe": "rcp_gmail"})),
        )
        .await;
    assert!(!result.ok, "{result:?}");
    assert!(result.content.contains("already playing"), "{result:?}");
    assert!(spy.last_box().is_none(), "nothing was played");
    assert!(recipes.runs.lock().unwrap().is_empty());
}

/// The claimed row is the one finished, played or failed, so no second row appears beside it
/// and a failed box does not leave the claim to lapse into "interrupted".
#[tokio::test]
async fn a_claimed_run_is_finished_under_its_claim_whether_it_played_or_failed() {
    for fail_with in [None, Some(BoxError::Unreachable("gone".to_string()))] {
        let failing = fail_with.is_some();
        let spy = Arc::new(SpyComputer {
            fail_with,
            ..SpyComputer::default()
        });
        let recipes = Arc::new(SpyRecipes {
            claim: Some(Ok("rrun_claimed")),
            ..SpyRecipes::default()
        });
        let executor = allowing(spy.clone())
            .with_screen(true)
            .with_recipes(offers(), recipes.clone());
        let result = executor
            .execute(
                &context_with_box("box_mine"),
                &call(RUN_RECIPE, json!({"recipe": "rcp_gmail"})),
            )
            .await;
        assert_eq!(result.ok, !failing, "{result:?}");
        assert_eq!(
            recipes.finished.lock().unwrap().as_slice(),
            &[Some("rrun_claimed".to_string())],
            "failing: {failing}"
        );
        assert_eq!(
            recipes.runs.lock().unwrap().as_slice(),
            &[("rcp_gmail".to_string(), 2, !failing)]
        );
    }
}

#[tokio::test]
async fn what_the_person_typed_beats_what_the_model_guessed() {
    let spy = Arc::new(SpyComputer::default());
    let recipes = Arc::new(SpyRecipes::default());
    let executor = allowing(spy.clone())
        .with_screen(true)
        .with_observe(Observe::Off)
        .with_recipes(offers(), recipes.clone())
        // The person picked this recipe and typed "mundo" into its field.
        .with_chosen_recipe(
            "rcp_gmail",
            opengrok_recipes::Values::from([("q".to_string(), "mundo".to_string())]),
        );
    let context = context_with_box("box_mine");

    // The model calls it with a term of its own — read off the sentence, or invented. The
    // person's beats it: theirs was typed into a named field, the model's was inferred.
    let result = executor
        .execute(
            &context,
            &call(
                RUN_RECIPE,
                json!({"recipe": "rcp_gmail", "values": {"q": "kabisado"}}),
            ),
        )
        .await;
    assert!(result.ok, "{result:?}");
    let asked = recipes.asked.lock().unwrap().clone();
    assert_eq!(
        asked
            .last()
            .and_then(|(_, values)| values.get("q"))
            .map(String::as_str),
        Some("mundo"),
        "the value that reached the recipe is the one the person typed"
    );

    // A recipe the person did NOT choose is untouched by their values.
    let result = executor
        .execute(
            &context,
            &call(
                RUN_RECIPE,
                json!({"recipe": "rcp_stops", "values": {"q": "kabisado"}}),
            ),
        )
        .await;
    assert!(!result.ok, "{result:?}");
    let asked = recipes.asked.lock().unwrap().clone();
    assert_eq!(
        asked
            .last()
            .and_then(|(_, values)| values.get("q"))
            .map(String::as_str),
        Some("kabisado"),
        "another recipe keeps the model's own values"
    );
}

/// A recipe whose connection dropped after the POST may have played: it is a part-way
/// refusal, so the loop's once-per-request rule holds (#120). One that never reached the
/// box played nothing, and a corrected retry is new work.
#[tokio::test]
async fn a_recipe_cut_off_mid_play_counts_as_played() {
    for (error, played) in [
        (
            BoxError::Interrupted("box-exec: client error (SendRequest)".to_string()),
            true,
        ),
        (
            BoxError::Unreachable("box-exec: client error (Connect)".to_string()),
            false,
        ),
    ] {
        let spy = Arc::new(SpyComputer {
            fail_with: Some(error),
            ..SpyComputer::default()
        });
        let executor = allowing(spy)
            .with_screen(true)
            .with_recipes(offers(), Arc::new(SpyRecipes::default()));
        let result = executor
            .execute(
                &context_with_box("box_mine"),
                &call(RUN_RECIPE, json!({"recipe": "rcp_gmail"})),
            )
            .await;
        assert!(!result.ok, "{result:?}");
        assert_eq!(result.stopped_part_way, played, "{result:?}");
        assert_eq!(
            result.content.contains("may have played part way"),
            played,
            "{result:?}"
        );
    }
}

#[tokio::test]
async fn a_recipe_that_stops_is_a_refusal_in_words() {
    let spy = Arc::new(SpyComputer::default());
    let recipes = Arc::new(SpyRecipes::default());
    let executor = allowing(spy)
        .with_screen(true)
        .with_observe(Observe::Input)
        .with_recipes(offers(), recipes.clone());
    let context = context_with_box("box_mine");

    let result = executor
        .execute(&context, &call(RUN_RECIPE, json!({"recipe": "rcp_stops"})))
        .await;
    assert!(!result.ok, "{result:?}");
    assert!(result.content.contains("stopped at step 1"), "{result:?}");
    assert!(result.content.contains("nothing at (5, 5)"), "{result:?}");
    // WHERE IT STOPPED IS MOST OF THE STORY AND WHAT WAS UNDER IT IS THE REST. A step that
    // failed at a coordinate is far easier to act on when the result also says which window,
    // if any, was there.
    assert!(
        result
            .content
            .contains("pointer steps landed on chromium.Chromium \"Inbox\""),
        "{result:?}"
    );
    assert!(
        result
            .content
            .contains("no window the box could name was under the pointer (at step 1)"),
        "{result:?}"
    );
    assert_eq!(
        recipes.runs.lock().unwrap().last().map(|r| r.2),
        Some(false)
    );
    assert!(result.stopped_part_way, "the box played it: {result:?}");

    // A recipe that was never granted is refused before anything runs.
    let result = executor
        .execute(&context, &call(RUN_RECIPE, json!({"recipe": "rcp_other"})))
        .await;
    assert!(!result.ok, "{result:?}");
    assert!(!result.stopped_part_way, "nothing played: {result:?}");
    assert!(
        result.content.contains("not granted") || result.content.contains("no recipe"),
        "{result:?}"
    );
}

/// THE RESULT THE TWENTY-FIVE-RUN INCIDENT DID NOT HAVE. A model that gets back "8 steps, ok"
/// has been told that nothing threw, which was true on all twenty-five of them. This is the
/// same result with what the box saw appended: which window each click landed on, and a
/// `type` whose keys reached nothing at all.
///
/// The server still says none of what that MEANS. It cannot: it does not know what the recipe
/// was for. The words are readings a model can act on, and every one of them is a reading.
#[tokio::test]
async fn a_run_that_was_watched_says_what_the_box_saw_and_judges_none_of_it() {
    let spy = Arc::new(SpyComputer::default());
    let recipes = Arc::new(SpyRecipes::default());
    let executor = allowing(spy.clone())
        .with_screen(true)
        .with_observe(Observe::Input)
        .with_recipes(offers(), recipes.clone());
    let context = context_with_box("box_mine");

    let result = executor
        .execute(&context, &call(RUN_RECIPE, json!({"recipe": "rcp_gmail"})))
        .await;
    // A run that played every step is still a success. The observation is what the model
    // reads next, not a reason to refuse.
    assert!(result.ok, "{result:?}");
    assert!(
        result.content.contains("ran recipe \"Open Gmail\""),
        "{result:?}"
    );
    assert!(
        result
            .content
            .contains("pointer steps landed on xterm.XTerm \"Terminal\" (at step 0)"),
        "{result:?}"
    );
    assert!(
        result
            .content
            .contains("keys had nowhere to go: no window held the focus (at step 1)"),
        "{result:?}"
    );
    assert!(
        result.content.contains("the looking cost 14ms"),
        "{result:?}"
    );
    for verdict in ["failed", "did not work", "wrong window", "should"] {
        assert!(
            !result.content.contains(verdict),
            "the server reports, it does not judge: {result:?}"
        );
    }
    // And the level actually reached the box, rather than being a summary of nothing.
    let asked = spy
        .ran_on
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, what)| what.starts_with("recipe:"))
        .map(|(_, what)| what.clone())
        .collect::<Vec<_>>();
    assert!(
        asked
            .last()
            .is_some_and(|body| body.contains("\"observe\":\"input\"")),
        "{asked:?}"
    );
}

/// A recipe's parameters appear in the schema so a model can see what it needs to provide.
#[test]
fn a_recipe_with_parameters_lists_them_in_the_schema() {
    let executor = allowing(Arc::new(SpyComputer::default()))
        .with_screen(true)
        .with_recipes(offers(), Arc::new(SpyRecipes::default()));
    let schemas = executor.tool_schemas(
        &AccountId::from_stored("acct_1"),
        &CoworkerId::from_stored("cw_1"),
    );
    let run_recipe_schema = schemas
        .iter()
        .find(|schema| schema["function"]["name"] == RUN_RECIPE)
        .unwrap();

    // The enum includes all recipe ids.
    let recipe_enum = &run_recipe_schema["function"]["parameters"]["properties"]["recipe"]["enum"];
    let recipe_ids: Vec<&str> = recipe_enum
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        recipe_ids.contains(&"rcp_search"),
        "rcp_search must be in enum"
    );

    // The schema mentions parameters for recipes that have them.
    let description = run_recipe_schema["function"]["description"]
        .as_str()
        .unwrap();
    assert!(
        description.contains("rcp_search"),
        "recipe id must be in description"
    );
    assert!(
        description.contains("search_term"),
        "parameter name must be in description"
    );
    assert!(
        description.contains("text"),
        "parameter type must be in description"
    );
}

/// A run whose missing required value comes back as a refusal the model can act on.
#[tokio::test]
async fn a_recipe_with_missing_required_parameter_is_refused() {
    let spy = Arc::new(SpyComputer::default());
    let recipes = Arc::new(SpyRecipes::default());
    let executor = allowing(spy.clone())
        .with_screen(true)
        .with_recipes(offers(), recipes);
    let context = context_with_box("box_mine");

    // Call the recipe without providing the required search_term parameter.
    let result = executor
        .execute(&context, &call(RUN_RECIPE, json!({"recipe": "rcp_search"})))
        .await;

    // The refusal names the parameter, so the model can try again with it.
    assert!(!result.ok, "{result:?}");
    assert!(result.content.contains("refused:"), "{result:?}");
    assert!(
        result.content.contains("search_term"),
        "must name the missing parameter: {result:?}"
    );
    assert!(
        result.content.contains("required"),
        "must explain it is required: {result:?}"
    );

    // Nothing reached the box — binding is a gate.
    assert_eq!(spy.last_box(), None);
}

/// A run with the required parameter supplied succeeds without error.
#[tokio::test]
async fn a_recipe_with_supplied_parameter_values_runs() {
    let spy = Arc::new(SpyComputer::default());
    let recipes = Arc::new(SpyRecipes::default());
    let executor = allowing(spy.clone())
        .with_screen(true)
        .with_recipes(offers(), recipes);
    let context = context_with_box("box_mine");

    let result = executor
        .execute(
            &context,
            &call(
                RUN_RECIPE,
                json!({"recipe": "rcp_search", "values": {"search_term": "hello"}}),
            ),
        )
        .await;

    assert!(result.ok, "{result:?}");
    assert!(
        result.content.contains("Search"),
        "should mention recipe name"
    );
    assert_eq!(spy.last_box().as_deref(), Some("box_mine"));
}

#[tokio::test]
async fn request_user_form_keeps_id_parsing_and_hold_precedence() {
    let spy = Arc::new(SpyComputer::default());
    let executor = allowing(spy.clone());
    let result = executor.execute(&context_with_box("box_mine"), &call(REQUEST_USER_FORM, json!({
        "collect": true,
        "fields": [{"id": "name"}, {"id": "Name"}, {"id": ""}, {"id": ""}, {"id": 1}, {"id": 1}, {}, {}]
    }))).await;
    assert_eq!(
        result.awaiting_reason,
        Some(AwaitingReason::UserForm),
        "{result:?}"
    );
    let mut context = context_with_box("box_mine");
    context.screen_hold = true;
    let held = executor
        .execute(
            &context,
            &call(
                REQUEST_USER_FORM,
                json!({
                    "collect": true, "fields": [{"id": "x"}, {"id": "x"}]
                }),
            ),
        )
        .await;
    assert!(held.content.contains("already open"), "{held:?}");
    assert!(!held.content.contains("Field IDs"), "{held:?}");
    assert!(!held.awaiting_approval);
    assert_eq!(spy.last_box(), None);
}

#[tokio::test]
async fn request_user_form_refuses_duplicate_ids_before_waiting() {
    let spy = Arc::new(SpyComputer::default());
    let executor = allowing(spy.clone());
    for collect in [false, true] {
        for first in [
            json!({"id": "x", "type": "password", "value": "private-value"}),
            json!({"id": "x", "type": "otp", "value": "private-value"}),
            json!({"id": "x", "secret": true, "value": "private-value"}),
            json!({"id": "x", "type": "text", "value": "private-value"}),
        ] {
            for reversed in [false, true] {
                let mut fields = vec![first.clone(), json!({"id": "x", "type": "text"})];
                if reversed {
                    fields.reverse();
                }
                let form = json!({"title": "Form", "collect": collect, "fields": fields});
                for arguments in [
                    form.clone(),
                    json!({"formRequest": form.clone()}),
                    json!({"message": {"formRequest": form}}),
                ] {
                    let result = executor
                        .execute(
                            &context_with_box("box_mine"),
                            &call(REQUEST_USER_FORM, arguments),
                        )
                        .await;
                    assert!(!result.ok, "{result:?}");
                    assert!(!result.awaiting_approval, "{result:?}");
                    assert!(result.awaiting_reason.is_none(), "{result:?}");
                    assert!(result.content.contains("Field IDs must be unique. Give each field a different id and call request_user_form again."), "{result:?}");
                    assert!(!result.content.contains("private-value"), "{result:?}");
                    assert_eq!(spy.last_box(), None);
                }
            }
        }
    }
}

#[tokio::test]
async fn request_user_form_awaits_and_does_not_type() {
    let spy = Arc::new(SpyComputer::default());
    let executor = allowing(spy.clone());
    let context = context_with_box("box_mine");
    let result = executor
        .execute(
            &context,
            &call(
                REQUEST_USER_FORM,
                json!({
                    "title": "Sign in",
                    "fields": [{
                        "id": "password",
                        "label": "Password",
                        "type": "password",
                        "required": true
                    }],
                    "values": { "password": "s3cret" }
                }),
            ),
        )
        .await;
    assert!(result.awaiting_approval, "{result:?}");
    assert_eq!(result.awaiting_reason, Some(AwaitingReason::UserForm));
    assert!(result.content.contains("Waiting for you"), "{result:?}");
    assert!(result.image.is_none(), "{result:?}");
    assert_eq!(spy.last_box(), None, "await must not type");
    assert!(!result.content.contains("s3cret"), "{result:?}");
}

#[tokio::test]
async fn a_form_open_refuses_computer_type_without_a_png() {
    let spy = Arc::new(SpyComputer::default());
    let executor = allowing(spy.clone()).with_screen(true);
    let mut context = context_with_box("box_mine");
    context.screen_hold = true;
    let result = executor
        .execute(
            &context,
            &call("computer", json!({ "action": "type", "text": "s3cret" })),
        )
        .await;
    assert!(!result.ok, "{result:?}");
    assert!(result.content.contains("is open"), "{result:?}");
    assert!(result.image.is_none(), "must not PNG a secret: {result:?}");
    assert!(!result.content.contains("s3cret"), "{result:?}");
    assert_eq!(spy.last_box(), None);
}

/// #188. The hold is the coworker's one computer, not one conversation's, so a turn in
/// another conversation is refused too — and told which conversation holds it.
#[tokio::test]
async fn a_held_screen_names_the_conversation_holding_it() {
    let spy = Arc::new(SpyComputer::default());
    let executor = allowing(spy.clone()).with_screen(true);
    let mut context = context_with_box("box_mine");
    context.screen_hold = true;
    context.screen_held_in = Some("thr-sign-in".to_string());
    for name in ["computer", REQUEST_USER_FORM] {
        let args = json!({"action": "screenshot", "fields": [{"id": "x"}]});
        let result = executor.execute(&context, &call(name, args)).await;
        assert!(result.content.contains("thr-sign-in"), "{result:?}");
    }
    assert_eq!(spy.last_box(), None);
}
