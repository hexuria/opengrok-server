//! What a coworker can do, and where it happens.
//!
//! IDENTITY ARGUMENTS ARE OVERWRITTEN, NOT VALIDATED (CLAUDE.md #7). The model proposes a tool
//! call; before it runs, the session's own identity replaces whatever the arguments said. Not
//! checked — *replaced*. A validating executor still has to be right about every field on every
//! tool forever; an overwriting one cannot be wrong, because the model's value is discarded before
//! anything reads it. A model that asks to run a command on `box_of_someone_else` runs it on its
//! own box and never learns the other box exists.
//!
//! THE BOX COMES FROM THE COWORKER'S ROW, NEVER FROM THE CALL. That is the same rule seen from the
//! other side: there is no argument a model could set that would move its work onto another
//! machine, because the machine is not an argument.
//!
//! A REFUSAL IS A RESULT, NOT AN ERROR (CLAUDE.md #8). When a tool is denied, the model is told so
//! in a form it can reason about and recover from — a refusal that killed the run would turn every
//! policy decision into an outage.

use std::sync::Arc;

use opengrok_box::{BoxError, Computer};
use opengrok_core::coworker::Coworker;
use opengrok_core::id::{AccountId, BoxId, CoworkerId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Who is running a tool. Assembled by the server from the session, never from a payload.
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// Whose session this is. Policy is about the principal, not the coworker, so it has to be
    /// carried here rather than inferred from the coworker's row.
    pub account_id: AccountId,
    pub coworker_id: CoworkerId,
    /// The coworker's own machine. `None` means one has not been assigned yet.
    pub box_id: Option<BoxId>,
}

impl ToolContext {
    /// Build the context from the coworker's own row. The only supported way to make one, so a
    /// caller cannot assemble a context out of request fields by accident.
    pub fn from_coworker(account_id: AccountId, id: CoworkerId, coworker: &Coworker) -> Self {
        Self {
            account_id,
            coworker_id: id,
            box_id: coworker.computer().cloned(),
        }
    }
}

/// What a model asked for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Arguments as the model wrote them — untrusted, and partly overwritten before use.
    pub arguments: Value,
}

/// What the model is told back. Always a result: a refusal is content, not a thrown error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub ok: bool,
    pub content: String,
    /// Set when the call is waiting on a person. The run SUSPENDS on this rather than continuing:
    /// a refusal ends a turn, an approval pauses one that can still be finished tomorrow.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub awaiting_approval: bool,
}

impl ToolResult {
    pub fn ok(call_id: &str, content: impl Into<String>) -> Self {
        Self {
            call_id: call_id.to_string(),
            ok: true,
            content: content.into(),
            awaiting_approval: false,
        }
    }

    /// Waiting on a person. `ok` is false because nothing ran — treating a pending approval as
    /// success is how a model concludes its command already worked.
    pub fn awaiting(call_id: &str, why: impl Into<String>) -> Self {
        Self {
            call_id: call_id.to_string(),
            ok: false,
            content: format!("waiting for approval: {}", why.into()),
            awaiting_approval: true,
        }
    }

    /// A refusal the model can reason about, phrased so it knows what to do differently.
    pub fn refused(call_id: &str, why: impl Into<String>) -> Self {
        Self {
            call_id: call_id.to_string(),
            ok: false,
            content: format!("refused: {}", why.into()),
            awaiting_approval: false,
        }
    }
}

/// The arguments `shell` accepts. `box_id` is deliberately absent — see the module note.
#[derive(Debug, Clone, Deserialize)]
pub struct ShellArgs {
    pub command: String,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u32,
}

fn default_timeout() -> u32 {
    30
}

/// The arguments the file tools accept.
#[derive(Debug, Clone, Deserialize)]
pub struct ReadFileArgs {
    pub path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WriteFileArgs {
    pub path: String,
    pub content: String,
}

/// Runs tool calls on the caller's own computer, if policy allows.
pub struct Executor {
    computer: Arc<dyn Computer>,
    /// What this principal may make this coworker do. Consulted before EVERY call, never once at
    /// the start: a grant revoked mid-conversation must stop the next tool, not the next session
    /// (CLAUDE.md #6).
    policy: opengrok_policy::Context,
}

impl Executor {
    /// An executor that allows nothing. The default is deliberately useless: an executor built
    /// without policy should refuse everything rather than quietly permit it.
    pub fn new(computer: Arc<dyn Computer>) -> Self {
        Self {
            computer,
            policy: opengrok_policy::Context::default(),
        }
    }

    /// The executor a real request builds: a computer, and what this principal may do with it.
    pub fn with_policy(computer: Arc<dyn Computer>, policy: opengrok_policy::Context) -> Self {
        Self { computer, policy }
    }

    /// The tools a model is offered. Kept here so the offered set and the executed set cannot
    /// drift — a tool the model is told about but that `execute` does not know is a dead end it
    /// will keep trying.
    pub fn tool_names() -> &'static [&'static str] {
        &["shell", "read_file", "write_file"]
    }

    /// Run one call. Never returns `Err` for a *tool* failure: the model gets a result either way,
    /// and only the caller's own bugs propagate.
    pub async fn execute(&self, context: &ToolContext, call: &ToolCall) -> ToolResult {
        // The identity rule, applied once, before anything reads an argument. Whatever the model
        // wrote for these keys is discarded rather than checked.
        let arguments = overwrite_identity(&call.arguments, context);

        // POLICY BEFORE ANYTHING RUNS, AND BEFORE ANYTHING IS EVEN LOOKED UP. A refusal reaches
        // the model as a result it can reason about rather than an exception that kills the run
        // (CLAUDE.md #8), and it names the rule so a person can fix it.
        let decision = opengrok_policy::decide(
            &context.account_id,
            &context.coworker_id,
            opengrok_policy::Action::RunTool(&call.name),
            &self.policy,
        );
        // Waiting is not permission and not refusal: the run suspends, and a person decides.
        if decision.needs_approval() {
            return ToolResult::awaiting(&call.id, decision.reason().unwrap_or("a human yes"));
        }
        if let Some(reason) = decision.reason() {
            return ToolResult::refused(&call.id, reason);
        }

        let Some(box_id) = context.box_id.as_ref() else {
            return ToolResult::refused(
                &call.id,
                "this coworker has no computer yet, so nothing can be run",
            );
        };

        match call.name.as_str() {
            "shell" => match serde_json::from_value::<ShellArgs>(arguments) {
                Ok(args) => self.shell(box_id, &call.id, args).await,
                Err(error) => ToolResult::refused(&call.id, format!("bad arguments: {error}")),
            },
            "read_file" => match serde_json::from_value::<ReadFileArgs>(arguments) {
                Ok(args) => match self.computer.read_file(box_id.as_str(), &args.path).await {
                    Ok(content) => ToolResult::ok(&call.id, content),
                    Err(error) => ToolResult::refused(&call.id, describe(error)),
                },
                Err(error) => ToolResult::refused(&call.id, format!("bad arguments: {error}")),
            },
            "write_file" => match serde_json::from_value::<WriteFileArgs>(arguments) {
                Ok(args) => match self
                    .computer
                    .write_file(box_id.as_str(), &args.path, &args.content)
                    .await
                {
                    Ok(()) => ToolResult::ok(&call.id, format!("wrote {}", args.path)),
                    Err(error) => ToolResult::refused(&call.id, describe(error)),
                },
                Err(error) => ToolResult::refused(&call.id, format!("bad arguments: {error}")),
            },
            other => ToolResult::refused(&call.id, format!("there is no tool called {other}")),
        }
    }

    async fn shell(&self, box_id: &BoxId, call_id: &str, args: ShellArgs) -> ToolResult {
        match self
            .computer
            .run(box_id.as_str(), &args.command, args.timeout_seconds)
            .await
        {
            Ok(output) => {
                let mut content = String::new();
                if !output.stdout.is_empty() {
                    content.push_str(&output.stdout);
                }
                if !output.stderr.is_empty() {
                    content.push_str("\n[stderr]\n");
                    content.push_str(&output.stderr);
                }
                // Truncation is stated, never implied. A coworker reasoning over a silently
                // clipped log reaches confident wrong conclusions.
                if output.stdout_truncated || output.stderr_truncated {
                    content.push_str("\n[output was truncated by the box]");
                }
                if output.timed_out {
                    content.push_str(&format!(
                        "\n[timed out after {}s — it may still be running]",
                        args.timeout_seconds
                    ));
                }
                // A non-zero exit is a *result*, not a refusal: the command ran, and the model
                // needs to see what it said in order to fix it.
                if output.exit_code != 0 {
                    content.push_str(&format!("\n[exit code {}]", output.exit_code));
                }
                ToolResult::ok(call_id, content)
            }
            Err(error) => ToolResult::refused(call_id, describe(error)),
        }
    }
}

/// Replace every identity-bearing argument with the session's own.
///
/// Additive on purpose: a key the model did not send is still set, so a tool cannot be reached
/// with an absent identity either. The list is small and explicit — a new identity-bearing
/// argument must be added here, and the test below is what notices when one is not.
pub fn overwrite_identity(arguments: &Value, context: &ToolContext) -> Value {
    let mut object = match arguments {
        Value::Object(map) => map.clone(),
        // A non-object argument carries no identity to overwrite, but must still not be able to
        // smuggle one — it is replaced by an object with only ours.
        _ => serde_json::Map::new(),
    };

    object.insert(
        "coworker_id".to_string(),
        Value::String(context.coworker_id.to_string()),
    );
    match &context.box_id {
        Some(box_id) => object.insert("box_id".to_string(), Value::String(box_id.to_string())),
        // Removed rather than left as the model wrote it.
        None => object.remove("box_id"),
    };

    Value::Object(object)
}

/// A box failure, in words a model can act on.
fn describe(error: BoxError) -> String {
    match error {
        BoxError::NoSuchBox => "that computer no longer exists".to_string(),
        BoxError::Unreachable(detail) => format!("the computer is unreachable: {detail}"),
        BoxError::Refused { status, body } => {
            format!("the computer refused the request ({status}): {body}")
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use opengrok_box::{BoxResult, CommandOutput, StartedCommand};
    use opengrok_core::coworker::{BoxMode, CoworkerCommand};
    use serde_json::json;
    use std::sync::Mutex;

    /// Records which box it was asked to act on, which is the assertion that matters here.
    #[derive(Default)]
    struct SpyComputer {
        ran_on: Mutex<Vec<(String, String)>>,
        fail_with: Option<BoxError>,
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
        async fn create(&self, _ttl: Option<u64>) -> BoxResult<String> {
            Ok("box_new".to_string())
        }
        async fn run(&self, box_id: &str, command: &str, _t: u32) -> BoxResult<CommandOutput> {
            if let Ok(mut calls) = self.ran_on.lock() {
                calls.push((box_id.to_string(), command.to_string()));
            }
            if let Some(error) = &self.fail_with {
                return Err(match error {
                    BoxError::NoSuchBox => BoxError::NoSuchBox,
                    BoxError::Unreachable(detail) => BoxError::Unreachable(detail.clone()),
                    BoxError::Refused { status, body } => BoxError::Refused {
                        status: *status,
                        body: body.clone(),
                    },
                });
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

    /// With no computer assigned, a model's `box_id` must not become the one used.
    #[test]
    fn without_a_computer_the_models_box_id_is_removed_not_kept() {
        let context = ToolContext {
            account_id: AccountId::from_stored("acct_1"),
            coworker_id: CoworkerId::from_stored("cw_1"),
            box_id: None,
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
        for name in Executor::tool_names() {
            let result = executor
                .execute(
                    &context,
                    &call(
                        name,
                        json!({"command": "ls", "path": "/tmp/a", "content": "x"}),
                    ),
                )
                .await;
            assert!(
                !result.content.contains("there is no tool called"),
                "{name} is offered but not implemented"
            );
        }
    }
}
