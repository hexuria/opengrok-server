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

pub mod mcp;

pub use mcp::{Endpoint, McpError, McpTool};

use std::collections::BTreeMap;
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

/// The name of the reverse-exec tool: a shell command on the USER'S OWN machine, not the bot's box.
pub const USER_MACHINE_SHELL: &str = "user_machine_shell";

/// What the reverse-exec sink hands back for one command. The gate + machine selection + audit all
/// live behind the sink (the server); the tool only forwards a command and renders the reply.
#[derive(Debug, Clone)]
pub enum UserMachineReply {
    /// The command ran on the user's machine; here is the rendered outcome.
    Ran(String),
    /// The gate refused it (channel off, a deny rule, or no daemon connected). Never ran.
    Refused(String),
    /// The user must approve this command. The run SUSPENDS, exactly like a policy `NeedsApproval`.
    NeedsApproval,
}

/// The bridge from the `user_machine_shell` tool to the reverse-exec channel. The server implements
/// it over its enqueue path; `opengrok-tools` only defines the seam so it need not depend on the
/// server. Attached to an `Executor` ONLY when the account has an enrolled, enabled machine — so the
/// tool is advertised exactly when there is a live machine to reach, never as a dead end.
#[async_trait::async_trait]
pub trait UserMachineSink: Send + Sync {
    /// Enqueue `command` on this account holder's own machine through the gate, and wait for the
    /// outcome. The server picks the machine, runs the gate, and writes the audit row. `call_id` is
    /// the tool call id — the stable approval id the inline card uses; `approved` is true on resume
    /// (the card said yes), so the Ask gate dispatches instead of suspending again.
    async fn run(
        &self,
        account_id: &AccountId,
        command: &str,
        call_id: &str,
        approved: bool,
    ) -> UserMachineReply;
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
    /// Calls a person has already said yes to.
    ///
    /// PER CALL, NEVER PER TOOL. Approving `shell` once must not approve every later `shell`: the
    /// person approved *that command*, and a set of call ids is the only shape that says so. A
    /// resumed run carries exactly the id that was answered, so nothing else slips through with it.
    approved_calls: std::collections::BTreeSet<String>,
    /// Live sessions with the MCP servers this coworker's plugins bring, keyed by
    /// `<plugin>.<server>`.
    ///
    /// Connected once per request rather than per call: a turn that reaches for three tools on one
    /// server should not hand-shake three times.
    sessions: BTreeMap<String, Arc<crate::mcp::Session>>,
    /// Every plugin tool on offer, in the order a model is told about them.
    plugin_tools: Vec<crate::mcp::McpTool>,
    /// The reverse-exec bridge, present ONLY when this account has an enrolled, enabled machine.
    /// Its presence is what advertises `user_machine_shell` — the tool exists iff a machine can
    /// actually be reached.
    user_machine: Option<Arc<dyn UserMachineSink>>,
}

impl Executor {
    /// An executor that allows nothing. The default is deliberately useless: an executor built
    /// without policy should refuse everything rather than quietly permit it.
    pub fn new(computer: Arc<dyn Computer>) -> Self {
        Self {
            computer,
            policy: opengrok_policy::Context::default(),
            approved_calls: std::collections::BTreeSet::new(),
            sessions: BTreeMap::new(),
            plugin_tools: Vec::new(),
            user_machine: None,
        }
    }

    /// The executor a real request builds: a computer, and what this principal may do with it.
    pub fn with_policy(computer: Arc<dyn Computer>, policy: opengrok_policy::Context) -> Self {
        Self {
            computer,
            policy,
            approved_calls: std::collections::BTreeSet::new(),
            sessions: BTreeMap::new(),
            plugin_tools: Vec::new(),
            user_machine: None,
        }
    }

    /// Carry the calls a person has already answered yes to.
    #[must_use]
    pub fn with_approved(mut self, approved: impl IntoIterator<Item = String>) -> Self {
        self.approved_calls = approved.into_iter().collect();
        self
    }

    /// Attach the reverse-exec bridge — the server does this only when the account has an enrolled,
    /// enabled machine, which is precisely when `user_machine_shell` should be offered.
    #[must_use]
    pub fn with_user_machine(mut self, sink: Arc<dyn UserMachineSink>) -> Self {
        self.user_machine = Some(sink);
        self
    }

    /// Attach live MCP sessions and the tools they offer.
    ///
    /// Taken together because a tool nobody can reach is worse than a tool nobody was offered: the
    /// model would call it, wait, and be refused for a reason it cannot fix.
    #[must_use]
    pub fn with_plugin_tools(
        mut self,
        sessions: BTreeMap<String, Arc<crate::mcp::Session>>,
        tools: Vec<crate::mcp::McpTool>,
    ) -> Self {
        self.sessions = sessions;
        self.plugin_tools = tools;
        self
    }

    /// The tools that need no plugin: always present, always executable.
    pub fn builtin_tool_names() -> &'static [&'static str] {
        &["shell", "read_file", "write_file"]
    }

    /// EVERY tool a model is offered on THIS request — built-ins plus whatever this coworker's
    /// plugins brought.
    ///
    /// An instance method now rather than a constant, because the answer depends on which plugins
    /// this coworker has. The invariant it protects is unchanged and load-bearing: the offered set
    /// must equal the executed set, or the model is told about a dead end and keeps trying it.
    pub fn tool_names(&self) -> Vec<String> {
        Self::builtin_tool_names()
            .iter()
            .map(|name| (*name).to_string())
            .chain(
                self.user_machine
                    .is_some()
                    .then(|| USER_MACHINE_SHELL.to_string()),
            )
            .chain(
                self.plugin_tools
                    .iter()
                    .map(|tool| tool.qualified_name.clone()),
            )
            .collect()
    }

    /// What the model is told each tool does, so it can choose between them.
    pub fn tool_descriptions(&self) -> Vec<(String, Option<String>)> {
        self.plugin_tools
            .iter()
            .map(|tool| (tool.qualified_name.clone(), tool.description.clone()))
            .collect()
    }

    /// The OpenAI function-calling tool definitions this coworker is OFFERED for a request: built-ins
    /// the policy permits (Allow or NeedsApproval — a `Deny` is never advertised, so the model is not
    /// told about a dead end it would keep trying) plus its plugin tools. This is the OFFERING half
    /// that must pair with `run`; without it the model is never told the tools exist and answers "I
    /// can't run commands" even with a computer attached. Empty when the coworker may run nothing.
    pub fn tool_schemas(&self, account_id: &AccountId, coworker_id: &CoworkerId) -> Vec<Value> {
        let permitted = |name: &str| {
            !matches!(
                opengrok_policy::decide(
                    account_id,
                    coworker_id,
                    opengrok_policy::Action::RunTool(name),
                    &self.policy,
                ),
                opengrok_policy::Decision::Deny(_)
            )
        };
        let mut schemas = Vec::new();
        for name in Self::builtin_tool_names() {
            if permitted(name)
                && let Some((description, parameters)) = builtin_tool_spec(name)
            {
                schemas.push(serde_json::json!({
                    "type": "function",
                    "function": { "name": name, "description": description, "parameters": parameters },
                }));
            }
        }
        // The reverse-exec tool is NOT gated by the per-coworker tool grant: its authorization is
        // the account's local-exec policy (enrolled machine + never/ask/bypass) and the machine's
        // own consent, applied per command inside the sink. Gating it behind the grant would deny
        // every existing coworker (whose grant lists only the box tools) a capability the account
        // explicitly enabled. Offered whenever a machine is attached.
        if self.user_machine.is_some()
            && let Some((description, parameters)) = builtin_tool_spec(USER_MACHINE_SHELL)
        {
            schemas.push(serde_json::json!({
                "type": "function",
                "function": { "name": USER_MACHINE_SHELL, "description": description, "parameters": parameters },
            }));
        }
        for tool in &self.plugin_tools {
            if permitted(&tool.qualified_name) {
                schemas.push(serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": tool.qualified_name,
                        "description": tool.description.clone().unwrap_or_default(),
                        // The MCP server validates the real arguments; we advertise an open object so
                        // the model can call it, rather than a schema we do not have here.
                        "parameters": { "type": "object" },
                    },
                }));
            }
        }
        schemas
    }

    /// Run one call. Never returns `Err` for a *tool* failure: the model gets a result either way,
    /// and only the caller's own bugs propagate.
    pub async fn execute(&self, context: &ToolContext, call: &ToolCall) -> ToolResult {
        // The reverse-exec tool is authorized by the LOCAL-EXEC policy (its sink runs its own
        // never/ask/bypass gate on the command), NOT the per-coworker tool grant — so it is handled
        // before the opengrok_policy gate, which would otherwise Deny it for any coworker whose
        // grant lists only the box tools. It runs on the USER'S machine, so it needs no box.
        if call.name == USER_MACHINE_SHELL {
            let Some(sink) = self.user_machine.as_ref() else {
                return ToolResult::refused(
                    &call.id,
                    "no machine of yours is connected, so nothing can run there",
                );
            };
            let approved = self.approved_calls.contains(&call.id);
            return match serde_json::from_value::<ShellArgs>(call.arguments.clone()) {
                Ok(args) => match sink.run(&context.account_id, &args.command, &call.id, approved).await {
                    UserMachineReply::Ran(text) => ToolResult::ok(&call.id, text),
                    UserMachineReply::Refused(why) => ToolResult::refused(&call.id, why),
                    UserMachineReply::NeedsApproval => ToolResult::awaiting(
                        &call.id,
                        "your machine's owner must approve this command",
                    ),
                },
                Err(error) => ToolResult::refused(&call.id, format!("bad arguments: {error}")),
            };
        }

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
        // A person may already have said yes — to THIS call, by id. An approval is about one
        // command, so matching on the tool name here would let the next `shell` ride in on the last
        // one's yes. Note this releases only `NeedsApproval`: a denial stays a denial, because
        // approving something policy forbids is not a thing a person can do.
        let answered_yes = decision.needs_approval() && self.approved_calls.contains(&call.id);

        if !answered_yes {
            // Waiting is not permission and not refusal: the run suspends, and a person decides.
            if decision.needs_approval() {
                return ToolResult::awaiting(&call.id, decision.reason().unwrap_or("a human yes"));
            }
            if let Some(reason) = decision.reason() {
                return ToolResult::refused(&call.id, reason);
            }
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
            // A plugin's tool. Reached only AFTER the policy check above, so a connector is
            // governed exactly like `shell` — the grant, the ceiling and the approval all apply
            // before a single byte leaves this process.
            other => self.call_plugin_tool(&call.id, other, arguments).await,
        }
    }

    /// Route a qualified name to the session that owns it.
    async fn call_plugin_tool(
        &self,
        call_id: &str,
        name: &str,
        arguments: serde_json::Value,
    ) -> ToolResult {
        let Some((plugin, server, remote)) = crate::mcp::split_qualified(name) else {
            return ToolResult::refused(call_id, format!("there is no tool called {name}"));
        };

        let key = format!("{plugin}.{server}");
        let Some(session) = self.sessions.get(&key) else {
            // Named precisely: "no such tool" and "that plugin is not connected right now" send a
            // person to different places.
            return ToolResult::refused(
                call_id,
                format!("{plugin} is not connected on this run, so {name} cannot run"),
            );
        };

        // The identity-overwritten arguments go out, minus the keys that are ours rather than the
        // tool's — a remote server rejecting an unexpected `coworker_id` would fail the call for a
        // reason the model cannot act on.
        let arguments = strip_identity(arguments);

        match session.call(&remote, arguments).await {
            Ok(content) => ToolResult::ok(call_id, content),
            Err(error) => ToolResult::refused(call_id, error.to_string()),
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
/// The description and JSON-Schema parameters a builtin tool is advertised to the model with, or
/// `None` if the name is not a builtin. `box_id` is deliberately ABSENT from every schema — the
/// computer is not the model's to choose; `overwrite_identity` injects it server-side.
fn builtin_tool_spec(name: &str) -> Option<(&'static str, Value)> {
    // Every description names the target unambiguously: THIS BOT'S OWN sandboxed box on the server,
    // which is NOT the user's own machine. A bot that runs `write_file` has written to its box, and
    // must never describe that as touching the user's computer. (When a reverse channel to the user's
    // machine exists, "my computer" will name two real machines; this wording keeps them apart.)
    match name {
        "shell" => Some((
            "Run a shell command on THIS BOT'S OWN computer — a sandboxed Linux box on the server, \
             not the user's own machine — and return its stdout, stderr and exit code.",
            serde_json::json!({
                "type": "object",
                "properties": { "command": { "type": "string", "description": "The shell command to run on the bot's own box." } },
                "required": ["command"],
            }),
        )),
        "read_file" => Some((
            "Read a file from THIS BOT'S OWN computer (the sandboxed box on the server, not the \
             user's machine) and return its contents.",
            serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string", "description": "Absolute path on the bot's own box." } },
                "required": ["path"],
            }),
        )),
        "write_file" => Some((
            "Create or overwrite a file on THIS BOT'S OWN computer (the sandboxed box on the server, \
             not the user's machine).",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute path on the bot's own box." },
                    "content": { "type": "string", "description": "The file's full new contents." },
                },
                "required": ["path", "content"],
            }),
        )),
        USER_MACHINE_SHELL => Some((
            "Run a shell command on the USER'S OWN machine — their real computer (for example their              Mac), NOT this bot's sandboxed box. It runs only with the user's consent under their              reverse-exec policy: a command may run, be refused, or be held for the user to approve              (in which case you should wait rather than retry). Use this ONLY when the task is about              the user's own machine; for your own work use `shell`.",
            serde_json::json!({
                "type": "object",
                "properties": { "command": { "type": "string", "description": "The shell command to run on the USER's own machine." } },
                "required": ["command"],
            }),
        )),
        _ => None,
    }
}

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

/// Remove the identity keys we added before handing arguments to a remote server.
///
/// They exist so a LOCAL tool cannot be aimed elsewhere. A remote MCP server never sees them: it
/// did not ask for them, its schema does not have them, and a strict server would reject the call
/// over a field the model never wrote.
fn strip_identity(arguments: Value) -> Value {
    match arguments {
        Value::Object(mut map) => {
            map.remove("coworker_id");
            map.remove("box_id");
            Value::Object(map)
        }
        other => other,
    }
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

    /// The invariant, now that the set is dynamic: what a model is offered is what it can call.
    #[tokio::test]
    async fn plugin_tools_join_the_offered_set() {
        let executor = allowing(Arc::new(SpyComputer::default())).with_plugin_tools(
            BTreeMap::new(),
            vec![crate::mcp::McpTool {
                qualified_name: "gmail.api.send".to_string(),
                remote_name: "send".to_string(),
                description: Some("Send a message".to_string()),
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

    // ---- The reverse-exec tool (slice 6) ------------------------------------------------------

    /// A sink whose reply is fixed, and which records what command it was asked to run.
    struct FakeSink {
        reply: UserMachineReply,
        seen: std::sync::Mutex<Vec<String>>,
    }
    impl FakeSink {
        fn new(reply: UserMachineReply) -> Arc<Self> {
            Arc::new(Self { reply, seen: std::sync::Mutex::new(Vec::new()) })
        }
    }
    #[async_trait]
    impl UserMachineSink for FakeSink {
        async fn run(
            &self,
            _account_id: &AccountId,
            command: &str,
            _call_id: &str,
            _approved: bool,
        ) -> UserMachineReply {
            if let Ok(mut seen) = self.seen.lock() {
                seen.push(command.to_string());
            }
            self.reply.clone()
        }
    }

    fn no_box_context() -> ToolContext {
        ToolContext {
            account_id: AccountId::from_stored("acct_1"),
            coworker_id: CoworkerId::from_stored("cw_1"),
            box_id: None,
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
        assert!(!schemas.iter().any(|s| s["function"]["name"] == USER_MACHINE_SHELL));

        let with = allowing(Arc::new(SpyComputer::default()))
            .with_user_machine(FakeSink::new(UserMachineReply::Ran("exit 0".into())));
        assert!(with.tool_names().iter().any(|n| n == USER_MACHINE_SHELL));
        let schemas = with.tool_schemas(
            &AccountId::from_stored("acct_1"),
            &CoworkerId::from_stored("cw_1"),
        );
        assert!(schemas.iter().any(|s| s["function"]["name"] == USER_MACHINE_SHELL));
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
            schemas.iter().any(|s| s["function"]["name"] == USER_MACHINE_SHELL),
            "reverse-exec tool must be offered even when the grant lists only box tools"
        );
        // And it RUNS (routes to the sink) rather than being refused by the grant.
        let result = executor
            .execute(&no_box_context(), &call(USER_MACHINE_SHELL, json!({"command": "mkdir ~/Code/x"})))
            .await;
        assert!(result.ok, "{result:?}");
        assert_eq!(sink.seen.lock().unwrap().as_slice(), &["mkdir ~/Code/x".to_string()]);
    }

    #[tokio::test]
    async fn user_machine_shell_routes_the_command_to_the_sink_without_a_box() {
        let sink = FakeSink::new(UserMachineReply::Ran("exit 0\n--- stdout ---\nuriah\n".into()));
        let executor = allowing(Arc::new(SpyComputer::default())).with_user_machine(sink.clone());
        // No box on the context — the reverse-exec tool must not need one.
        let result = executor
            .execute(&no_box_context(), &call(USER_MACHINE_SHELL, json!({"command": "whoami"})))
            .await;
        assert!(result.ok, "{result:?}");
        assert!(result.content.contains("uriah"));
        assert_eq!(sink.seen.lock().unwrap().as_slice(), &["whoami".to_string()]);
    }

    #[tokio::test]
    async fn user_machine_shell_suspends_the_run_when_the_owner_must_approve() {
        let executor = allowing(Arc::new(SpyComputer::default()))
            .with_user_machine(FakeSink::new(UserMachineReply::NeedsApproval));
        let result = executor
            .execute(&no_box_context(), &call(USER_MACHINE_SHELL, json!({"command": "rm -rf x"})))
            .await;
        assert!(!result.ok);
        assert!(result.awaiting_approval, "an Ask must suspend the run");
    }

    #[tokio::test]
    async fn user_machine_shell_relays_a_refusal_from_the_gate() {
        let executor = allowing(Arc::new(SpyComputer::default()))
            .with_user_machine(FakeSink::new(UserMachineReply::Refused("a deny rule matched".into())));
        let result = executor
            .execute(&no_box_context(), &call(USER_MACHINE_SHELL, json!({"command": "rm -rf /"})))
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
            .execute(&no_box_context(), &call(USER_MACHINE_SHELL, json!({"command": "whoami"})))
            .await;
        assert!(!result.ok);
        assert!(!result.awaiting_approval);
    }
}

