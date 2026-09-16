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

pub mod review;
pub use review::{
    AwaitingReason, Gate, Outcome, ReviewAsk, ReviewJudge, ReviewOutcome, ReviewPolicy,
    ReviewVerdict, ask_first_reason, combine, redact_arguments,
};
pub mod mcp;

pub use mcp::{Endpoint, McpError, McpTool};

use std::collections::BTreeMap;
use std::sync::Arc;

use opengrok_box::{BoxError, Computer, CuaAction, Screenshot};
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
    /// A room's shared computer, when this turn is spoken in a group that has one. Reached by
    /// passing `machine: "group"`; without it every call goes to the coworker's own box.
    pub group_box: Option<GroupBox>,
}

/// The shared computer of the group a turn is spoken in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupBox {
    pub box_id: BoxId,
    /// The room's name, for the tool descriptions.
    pub name: String,
}

impl ToolContext {
    /// Build the context from the coworker's own row. The only supported way to make one, so a
    /// caller cannot assemble a context out of request fields by accident.
    pub fn from_coworker(account_id: AccountId, id: CoworkerId, coworker: &Coworker) -> Self {
        Self {
            account_id,
            coworker_id: id,
            box_id: coworker.computer().cloned(),
            group_box: None,
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
    /// WHY it is waiting — which card to raise, and which verb may answer it. Two different cards
    /// can come from the same tool, so the tool name no longer says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub awaiting_reason: Option<AwaitingReason>,
    /// A picture that goes with the words: the screen after a `computer` action. The model is
    /// shown it as an image, the client paints it, the journal keeps it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ToolImage>,
}

/// An image a tool hands back, base64 so it rides JSON as-is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolImage {
    pub mime: String,
    pub base64: String,
    pub width: u32,
    pub height: u32,
}

impl From<Screenshot> for ToolImage {
    fn from(shot: Screenshot) -> Self {
        Self {
            mime: shot.mime,
            base64: shot.png_base64,
            width: shot.width,
            height: shot.height,
        }
    }
}

impl ToolResult {
    pub fn ok(call_id: &str, content: impl Into<String>) -> Self {
        Self {
            call_id: call_id.to_string(),
            ok: true,
            content: content.into(),
            awaiting_approval: false,
            awaiting_reason: None,
            image: None,
        }
    }

    #[must_use]
    pub fn with_image(mut self, image: ToolImage) -> Self {
        self.image = Some(image);
        self
    }

    /// Waiting on a person. `ok` is false because nothing ran — treating a pending approval as
    /// success is how a model concludes its command already worked.
    pub fn awaiting(call_id: &str, reason: AwaitingReason, why: impl Into<String>) -> Self {
        Self {
            call_id: call_id.to_string(),
            ok: false,
            content: format!("waiting for approval: {}", why.into()),
            awaiting_approval: true,
            awaiting_reason: Some(reason),
            image: None,
        }
    }

    /// A refusal the model can reason about, phrased so it knows what to do differently.
    pub fn refused(call_id: &str, why: impl Into<String>) -> Self {
        Self {
            call_id: call_id.to_string(),
            ok: false,
            content: format!("refused: {}", why.into()),
            awaiting_approval: false,
            awaiting_reason: None,
            image: None,
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
/// The gate's verdict for a command, WITHOUT dispatching it. Auto-review must judge before a
/// command runs, and the gate must judge first — so they are two questions, not one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserMachineVerdict {
    Allow,
    Ask,
    Deny(String),
}

#[async_trait::async_trait]
pub trait UserMachineSink: Send + Sync {
    /// Judge `command` against the machine's policy (mode + rules) and say what `run` would do —
    /// allow, ask, or deny — without queueing anything. `run` judges again when it runs; this is
    /// the executor's chance to consult auto-review in between.
    async fn decide(&self, account_id: &AccountId, command: &str) -> UserMachineVerdict;

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

/// The arguments `open_url` accepts.
/// A recipe a bot may run: what the tool lists, by id and in words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipeOffer {
    pub id: String,
    pub name: String,
    pub description: String,
}

/// What a recipe run leaves behind: the box's receipt, read for the parts a result needs.
#[derive(Debug, Clone)]
pub struct RecipeReceipt {
    pub ok: bool,
    pub ran: usize,
    pub stopped_at: Option<usize>,
    pub error: Option<String>,
    pub image: Option<ToolImage>,
    pub raw: Value,
}

impl RecipeReceipt {
    pub fn from_value(raw: Value) -> Self {
        let ok = raw.get("ok").and_then(Value::as_bool).unwrap_or(false);
        let ran = raw.get("ran").and_then(Value::as_u64).unwrap_or(0) as usize;
        let stopped_at = raw
            .get("stopped_at")
            .and_then(Value::as_u64)
            .map(|n| n as usize);
        let error = raw
            .get("steps")
            .and_then(Value::as_array)
            .and_then(|steps| steps.iter().find_map(|step| step.get("error")?.as_str()))
            .map(str::to_string);
        let image = raw.get("screenshot").and_then(|shot| {
            Some(ToolImage {
                mime: shot.get("mime")?.as_str()?.to_string(),
                base64: shot.get("png_base64")?.as_str()?.to_string(),
                width: shot.get("width")?.as_u64()? as u32,
                height: shot.get("height")?.as_u64()? as u32,
            })
        });
        Self {
            ok,
            ran,
            stopped_at,
            error,
            image,
            raw,
        }
    }
}

/// Where the executor gets a recipe's steps from, and tells what a run did — the server
/// implements it over the store, so this crate stays free of Postgres.
#[async_trait::async_trait]
pub trait RecipeSource: Send + Sync {
    /// The runnable version of a recipe: `(version, the box's request body)`.
    async fn recipe_request(&self, recipe_id: &str) -> Result<(i32, Value), String>;
    /// Write the run down.
    async fn record_run(
        &self,
        recipe_id: &str,
        version: i32,
        coworker_id: &CoworkerId,
        receipt: &RecipeReceipt,
    );
}

/// The arguments `run_recipe` accepts.
#[derive(Debug, Clone, Deserialize)]
pub struct RunRecipeArgs {
    pub recipe: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenUrlArgs {
    pub url: String,
}

/// The arguments `computer` accepts: one action, with the fields that action needs. Modelled
/// on the shape models already know from computer-use APIs, so one tool covers the screen.
#[derive(Debug, Clone, Deserialize)]
pub struct ComputerArgs {
    pub action: String,
    #[serde(default)]
    pub coordinate: Option<[i32; 2]>,
    #[serde(default)]
    pub to: Option<[i32; 2]>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub scroll: Option<[i32; 2]>,
    #[serde(default)]
    pub button: Option<u8>,
}

impl ComputerArgs {
    /// `Ok(None)` is a screenshot; `Ok(Some(action))` acts first and then looks. The error
    /// names the missing field, so a model can fix its call instead of guessing.
    pub fn into_action(self) -> Result<Option<CuaAction>, String> {
        let at = |coordinate: Option<[i32; 2]>, what: &str| {
            coordinate.ok_or_else(|| format!("{what} needs `coordinate`: [x, y]"))
        };
        let action = match self.action.as_str() {
            "screenshot" => return Ok(None),
            "click" | "left_click" => {
                let [x, y] = at(self.coordinate, "click")?;
                CuaAction::Click {
                    x,
                    y,
                    button: self.button,
                }
            }
            "right_click" => {
                let [x, y] = at(self.coordinate, "right_click")?;
                CuaAction::Click {
                    x,
                    y,
                    button: Some(3),
                }
            }
            "double_click" => {
                let [x, y] = at(self.coordinate, "double_click")?;
                CuaAction::DoubleClick { x, y }
            }
            "move" | "mouse_move" => {
                let [x, y] = at(self.coordinate, "move")?;
                CuaAction::Move { x, y }
            }
            "drag" | "left_click_drag" => {
                let [x1, y1] = at(self.coordinate, "drag")?;
                let [x2, y2] = self.to.ok_or("drag needs `to`: [x, y]")?;
                CuaAction::Drag { x1, y1, x2, y2 }
            }
            "type" => CuaAction::Type {
                text: self.text.ok_or("type needs `text`")?,
            },
            "key" => CuaAction::Key {
                key: self.key.ok_or("key needs `key`, e.g. Return or ctrl+l")?,
            },
            "scroll" => {
                let [x, y] = at(self.coordinate, "scroll")?;
                let [dx, dy] = self.scroll.ok_or("scroll needs `scroll`: [dx, dy]")?;
                CuaAction::Scroll { x, y, dx, dy }
            }
            other => {
                return Err(format!(
                    "unknown action `{other}`; one of screenshot, click, right_click, \
                     double_click, move, drag, type, key, scroll"
                ));
            }
        };
        Ok(Some(action))
    }
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
    /// Calls a person answered on an AUTO-REVIEW card. Kept apart from `approved_calls` on
    /// purpose: a review approval skips the judge and nothing else. It must never release the
    /// machine's own consent gate — if the machine's mode flipped to `ask` between the card and
    /// the answer, the owner still gets their own card.
    review_approved_calls: std::collections::BTreeSet<String>,
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
    /// Auto-review, when the run's effective policy is on: the instruction texts (resolved once
    /// per run by the server) and the judge that reads them. `None` is the cheapest short-circuit.
    auto_review: Option<AutoReview>,
    /// The box has a display. Only then are `open_url` and `computer` offered: a headless box
    /// would refuse every call, and a tool that always refuses is a dead end the model retries.
    screen: bool,
    /// The name of the group whose shared computer this turn may use, when there is one; it
    /// puts `machine` on the box tools' schemas.
    group_box_name: Option<String>,
    /// The taught recipes this bot may run; `run_recipe` is offered only when there are any.
    recipes: Vec<RecipeOffer>,
    recipe_source: Option<Arc<dyn RecipeSource>>,
}

/// The built-ins that need a display.
const SCREEN_TOOLS: &[&str] = &["open_url", "computer"];
/// The recipe tool's name; offered next to the screen tools, gated the same way.
pub const RUN_RECIPE: &str = "run_recipe";

/// The auto-review pair a run carries.
struct AutoReview {
    policy: ReviewPolicy,
    judge: Arc<dyn ReviewJudge>,
}

impl AutoReview {
    /// One question, one word back, rendered into what the ladder needs. The judge sees the
    /// arguments AFTER identity overwrite and redaction, never as the model wrote them.
    async fn judge(&self, tool: &str, arguments: &Value) -> ReviewOutcome {
        let redacted = redact_arguments(arguments);
        let verdict = self
            .judge
            .judge(ReviewAsk {
                tool,
                arguments: &redacted,
                allow_instructions: &self.policy.allow_instructions,
                block_instructions: &self.policy.block_instructions,
            })
            .await;
        match verdict {
            ReviewVerdict::Allow => ReviewOutcome::Allow,
            // The Settings UI labels this list "Ask first" and stores it as
            // blockInstructions. A match must raise a card, not refuse.
            ReviewVerdict::Block => {
                ReviewOutcome::Ask(ask_first_reason(&self.policy.block_instructions))
            }
            ReviewVerdict::Ask => ReviewOutcome::Ask(review::REVIEW_ASK_REASON.to_string()),
            ReviewVerdict::Unavailable => {
                ReviewOutcome::Ask(review::REVIEW_UNAVAILABLE_REASON.to_string())
            }
        }
    }
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
            auto_review: None,
            review_approved_calls: std::collections::BTreeSet::new(),
            screen: false,
            group_box_name: None,
            recipes: Vec::new(),
            recipe_source: None,
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
            auto_review: None,
            review_approved_calls: std::collections::BTreeSet::new(),
            screen: false,
            group_box_name: None,
            recipes: Vec::new(),
            recipe_source: None,
        }
    }

    /// Say the box has a display, so the screen tools are offered and run.
    #[must_use]
    pub fn with_screen(mut self, screen: bool) -> Self {
        self.screen = screen;
        self
    }

    /// Whether the screen tools are on offer — the prompt must say the same thing the offering does.
    pub fn has_screen(&self) -> bool {
        self.screen
    }

    /// The recipes this bot was granted, and where their steps come from.
    #[must_use]
    pub fn with_recipes(
        mut self,
        recipes: Vec<RecipeOffer>,
        source: Arc<dyn RecipeSource>,
    ) -> Self {
        self.recipes = recipes;
        self.recipe_source = Some(source);
        self
    }

    pub fn has_recipes(&self) -> bool {
        self.screen && !self.recipes.is_empty()
    }

    /// Offer `machine: "group"` on the box tools, naming the room.
    pub fn set_group_box_name(&mut self, name: &str) {
        self.group_box_name = Some(name.to_string());
    }

    /// Carry the calls a person has already answered yes to.
    #[must_use]
    pub fn with_approved(mut self, approved: impl IntoIterator<Item = String>) -> Self {
        self.approved_calls = approved.into_iter().collect();
        self
    }

    /// Carry the calls a person approved on an auto-review card: the judge is skipped for them,
    /// and NOTHING else is released (see `review_approved_calls`).
    #[must_use]
    pub fn with_review_approved(mut self, approved: impl IntoIterator<Item = String>) -> Self {
        self.review_approved_calls = approved.into_iter().collect();
        self
    }

    /// Attach the reverse-exec bridge — the server does this only when the account has an enrolled,
    /// enabled machine, which is precisely when `user_machine_shell` should be offered.
    #[must_use]
    pub fn with_user_machine(mut self, sink: Arc<dyn UserMachineSink>) -> Self {
        self.user_machine = Some(sink);
        self
    }

    /// Attach auto-review for this run: the effective instruction texts and the judge. The server
    /// resolves the texts once per run (`docs/AUTO-REVIEW.md` §3) and attaches only when the
    /// policy is on; an inactive policy attached here is still a no-op per call.
    #[must_use]
    pub fn with_auto_review(mut self, policy: ReviewPolicy, judge: Arc<dyn ReviewJudge>) -> Self {
        self.auto_review = Some(AutoReview { policy, judge });
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

    /// The tools that need no plugin. `open_url` and `computer` are in the default grant but are
    /// OFFERED only when the box has a display (`with_screen`).
    pub fn builtin_tool_names() -> &'static [&'static str] {
        &["shell", "read_file", "write_file", "open_url", "computer"]
    }

    /// The built-ins this executor can actually run right now.
    fn offered_builtins(&self) -> impl Iterator<Item = &'static str> + '_ {
        Self::builtin_tool_names()
            .iter()
            .copied()
            .filter(move |name| self.screen || !SCREEN_TOOLS.contains(name))
    }

    /// EVERY tool a model is offered on THIS request — built-ins plus whatever this coworker's
    /// plugins brought.
    ///
    /// An instance method now rather than a constant, because the answer depends on which plugins
    /// this coworker has. The invariant it protects is unchanged and load-bearing: the offered set
    /// must equal the executed set, or the model is told about a dead end and keeps trying it.
    pub fn tool_names(&self) -> Vec<String> {
        self.offered_builtins()
            .map(str::to_string)
            .chain(self.has_recipes().then(|| RUN_RECIPE.to_string()))
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
        for name in self.offered_builtins() {
            if permitted(name)
                && let Some((description, mut parameters)) = builtin_tool_spec(name)
            {
                if let Some(group) = &self.group_box_name
                    && let Some(properties) = parameters
                        .get_mut("properties")
                        .and_then(Value::as_object_mut)
                {
                    properties.insert(
                        "machine".to_string(),
                        serde_json::json!({
                            "type": "string",
                            "enum": ["mine", "group"],
                            "description": format!(
                                "Which computer: \"mine\" (your own box, the default) or \"group\" (the shared computer of the {group} group, which every member can see and use)."
                            ),
                        }),
                    );
                }
                schemas.push(serde_json::json!({
                    "type": "function",
                    "function": { "name": name, "description": description, "parameters": parameters },
                }));
            }
        }
        // Taught recipes: one tool whose description is the list, so the model reads what each
        // one does and picks by id. Gated by the grant like the other box tools, and only where
        // there is a screen to run them on.
        if self.has_recipes() && permitted(RUN_RECIPE) {
            let listing = self
                .recipes
                .iter()
                .map(|recipe| format!("`{}` — {}: {}", recipe.id, recipe.name, recipe.description))
                .collect::<Vec<_>>()
                .join("\n");
            let ids: Vec<&str> = self
                .recipes
                .iter()
                .map(|recipe| recipe.id.as_str())
                .collect();
            schemas.push(serde_json::json!({
                "type": "function",
                "function": {
                    "name": RUN_RECIPE,
                    "description": format!(
                        "Run a task a person taught on THIS BOT'S OWN computer, as one step, instead of \
                         looking and clicking your way through it. Use one when the request matches its \
                         description, and say which you used. Recipes you may run:\n{listing}"
                    ),
                    "parameters": {
                        "type": "object",
                        "properties": { "recipe": { "type": "string", "enum": ids, "description": "The recipe's id." } },
                        "required": ["recipe"],
                    },
                },
            }));
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
    ///
    /// THE ORDER IS THE SAFETY ARGUMENT. Identity first, for every tool. Then the PRIMARY gate for
    /// the tool — the machine's own policy for `user_machine_shell`, the coworker's grant for
    /// everything else. Then, once, the auto-review judge — skipped when the gate already denied
    /// (no spend on a refused command), when a person already answered this call (a resumed call
    /// is never re-judged), or when no active policy is attached. Then the ladder in
    /// `review::combine`, which is pure and table-tested. Only `Outcome::Run` reaches a dispatch.
    pub async fn execute(&self, context: &ToolContext, call: &ToolCall) -> ToolResult {
        // The identity rule, applied once, before anything reads an argument — including the judge
        // and the card. Whatever the model wrote for these keys is discarded rather than checked.
        let arguments = overwrite_identity(&call.arguments, context);
        // Two different yeses. The gate's approval (the machine owner's or the policy's card)
        // releases the gate's ask AND skips the judge; a review approval skips only the judge.
        let gate_approved = self.approved_calls.contains(&call.id);
        let review_approved = gate_approved || self.review_approved_calls.contains(&call.id);

        // The reverse-exec tool is authorized by the LOCAL-EXEC policy (its sink judges the
        // command against never/ask/bypass + the standing rules), NOT the per-coworker tool grant —
        // which would Deny it for any coworker whose grant lists only the box tools. It runs on
        // the USER'S machine, so it needs no box.
        let user_machine_command = if call.name == USER_MACHINE_SHELL {
            match serde_json::from_value::<ShellArgs>(arguments.clone()) {
                Ok(args) => Some(args.command),
                Err(error) => {
                    // Say what would work: a model that sent nothing needs the shape, not
                    // just the complaint, or it sends nothing again.
                    return ToolResult::refused(
                        &call.id,
                        format!(
                            "bad arguments: {error}; call again as {{\"command\": \"<the shell command>\"}}"
                        ),
                    );
                }
            }
        } else {
            None
        };
        let gate = if let Some(command) = user_machine_command.as_deref() {
            let Some(sink) = self.user_machine.as_ref() else {
                return ToolResult::refused(
                    &call.id,
                    "no machine of yours is connected, so nothing can run there",
                );
            };
            match sink.decide(&context.account_id, command).await {
                UserMachineVerdict::Allow => Gate::Allow,
                UserMachineVerdict::Ask => Gate::Ask(
                    AwaitingReason::ExecConsent,
                    "your machine's owner must approve this command".to_string(),
                ),
                UserMachineVerdict::Deny(why) => Gate::Deny(why),
            }
        } else {
            // POLICY BEFORE ANYTHING RUNS, AND BEFORE ANYTHING IS EVEN LOOKED UP. A refusal
            // reaches the model as a result it can reason about rather than an exception that
            // kills the run (CLAUDE.md #8), and it names the rule so a person can fix it.
            let decision = opengrok_policy::decide(
                &context.account_id,
                &context.coworker_id,
                opengrok_policy::Action::RunTool(&call.name),
                &self.policy,
            );
            if decision.needs_approval() {
                Gate::Ask(
                    AwaitingReason::PolicyApproval,
                    decision.reason().unwrap_or("a human yes").to_string(),
                )
            } else if let Some(reason) = decision.reason() {
                Gate::Deny(reason.to_string())
            } else {
                Gate::Allow
            }
        };

        // ONE judge call site, for every tool.
        let review = match (&gate, review_approved, self.auto_review.as_ref()) {
            (Gate::Deny(_), _, _) | (_, true, _) | (_, _, None) => None,
            (_, false, Some(review)) if !review.policy.is_active() => None,
            (_, false, Some(review)) => Some(review.judge(&call.name, &arguments).await),
        };

        match combine(gate, review, gate_approved) {
            Outcome::Refuse(why) => return ToolResult::refused(&call.id, why),
            // The machine's own ask is answered by the sink: `run` re-judges, writes the audit
            // row every other verdict gets, and replies NeedsApproval without dispatching. Going
            // through it keeps "one audit row per command that touched the channel" true.
            Outcome::Ask(AwaitingReason::ExecConsent, _) if user_machine_command.is_some() => {}
            Outcome::Ask(reason, why) => return ToolResult::awaiting(&call.id, reason, why),
            Outcome::Run => {}
        }

        if let Some(command) = user_machine_command.as_deref() {
            let Some(sink) = self.user_machine.as_ref() else {
                return ToolResult::refused(
                    &call.id,
                    "no machine of yours is connected, so nothing can run there",
                );
            };
            // The sink judges again as it runs (the gate is the single choke point on the way to
            // a machine). If the policy changed underneath us and it now asks, honour that — a
            // review approval must never stand in for the machine owner's consent.
            return match sink
                .run(&context.account_id, command, &call.id, gate_approved)
                .await
            {
                UserMachineReply::Ran(text) => ToolResult::ok(&call.id, text),
                UserMachineReply::Refused(why) => ToolResult::refused(&call.id, why),
                UserMachineReply::NeedsApproval => ToolResult::awaiting(
                    &call.id,
                    AwaitingReason::ExecConsent,
                    "your machine's owner must approve this command",
                ),
            };
        }

        // `machine: "group"` aims the call at the room's shared computer; anything else is the
        // coworker's own box. The model chooses this one, so it is read from the arguments
        // rather than stamped by `overwrite_identity`.
        let on_group = arguments.get("machine").and_then(Value::as_str) == Some("group");
        let box_id = if on_group {
            match context.group_box.as_ref() {
                Some(group) => &group.box_id,
                None => {
                    return ToolResult::refused(
                        &call.id,
                        "this conversation has no shared group computer; leave `machine` out to use your own",
                    );
                }
            }
        } else {
            let Some(box_id) = context.box_id.as_ref() else {
                return ToolResult::refused(
                    &call.id,
                    "this coworker has no computer yet, so nothing can be run",
                );
            };
            box_id
        };

        match call.name.as_str() {
            RUN_RECIPE => match serde_json::from_value::<RunRecipeArgs>(arguments) {
                Ok(args) => {
                    self.run_recipe(box_id, context, &call.id, &args.recipe)
                        .await
                }
                Err(error) => ToolResult::refused(&call.id, format!("bad arguments: {error}")),
            },
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
            "open_url" => match serde_json::from_value::<OpenUrlArgs>(arguments) {
                Ok(args) => self.open_url(box_id, &call.id, args).await,
                Err(error) => ToolResult::refused(&call.id, format!("bad arguments: {error}")),
            },
            "computer" => match serde_json::from_value::<ComputerArgs>(arguments) {
                Ok(args) => self.computer_use(box_id, &call.id, args).await,
                Err(error) => ToolResult::refused(&call.id, format!("bad arguments: {error}")),
            },
            // A plugin's tool. Reached only AFTER the policy check above, so a connector is
            // governed exactly like `shell` — the grant, the ceiling and the approval all apply
            // before a single byte leaves this process.
            other => self.call_plugin_tool(&call.id, other, arguments).await,
        }
    }

    /// A taught recipe, run as one box call. The receipt's end screenshot rides the result so
    /// the model sees where the screen ended up; a stopped run is a refusal with the step.
    async fn run_recipe(
        &self,
        box_id: &BoxId,
        context: &ToolContext,
        call_id: &str,
        recipe_id: &str,
    ) -> ToolResult {
        let Some(offer) = self.recipes.iter().find(|recipe| recipe.id == recipe_id) else {
            return ToolResult::refused(
                call_id,
                format!("no recipe `{recipe_id}` is granted to this coworker"),
            );
        };
        let Some(source) = self.recipe_source.as_ref() else {
            return ToolResult::refused(call_id, "recipes are not available on this server");
        };
        let (version, request) = match source.recipe_request(recipe_id).await {
            Ok(found) => found,
            Err(why) => return ToolResult::refused(call_id, why),
        };
        let receipt = match self.computer.run_recipe(box_id.as_str(), &request).await {
            Ok(raw) => RecipeReceipt::from_value(raw),
            Err(error) => return ToolResult::refused(call_id, describe(error)),
        };
        source
            .record_run(recipe_id, version, &context.coworker_id, &receipt)
            .await;
        let mut result = if receipt.ok {
            ToolResult::ok(
                call_id,
                format!(
                    "ran recipe \"{}\" (v{version}): {} steps; screenshot of the screen afterwards attached",
                    offer.name, receipt.ran
                ),
            )
        } else {
            ToolResult::refused(
                call_id,
                format!(
                    "recipe \"{}\" (v{version}) stopped at step {}: {}",
                    offer.name,
                    receipt.stopped_at.unwrap_or(receipt.ran),
                    receipt.error.as_deref().unwrap_or("a step failed")
                ),
            )
        };
        if let Some(image) = receipt.image.clone() {
            result = result.with_image(image);
        }
        result
    }

    async fn open_url(&self, box_id: &BoxId, call_id: &str, args: OpenUrlArgs) -> ToolResult {
        if !self.screen {
            return ToolResult::refused(call_id, "this computer has no screen");
        }
        match self.computer.open_url(box_id.as_str(), &args.url).await {
            Ok(()) => ToolResult::ok(
                call_id,
                format!(
                    "opened {} in the browser on your box; take a screenshot with `computer` to see it",
                    args.url
                ),
            ),
            Err(error) => ToolResult::refused(call_id, describe(error)),
        }
    }

    /// Act, then look: every action answers with a fresh screenshot, so the model sees what it
    /// did without a second call. A plain `screenshot` just looks.
    async fn computer_use(&self, box_id: &BoxId, call_id: &str, args: ComputerArgs) -> ToolResult {
        if !self.screen {
            return ToolResult::refused(call_id, "this computer has no screen");
        }
        let action = match args.into_action() {
            Ok(action) => action,
            Err(why) => return ToolResult::refused(call_id, format!("bad arguments: {why}")),
        };
        let mut said = String::new();
        if let Some(action) = &action {
            if let Err(error) = self.computer.act(box_id.as_str(), action).await {
                return ToolResult::refused(call_id, describe(error));
            }
            said = format!("{}; ", action.describe());
            // The display needs a moment to repaint after input before it is worth looking.
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        }
        match self.computer.screenshot(box_id.as_str()).await {
            Ok(shot) => ToolResult::ok(
                call_id,
                format!(
                    "{said}screenshot of the {}x{} screen attached",
                    shot.width, shot.height
                ),
            )
            .with_image(ToolImage::from(shot)),
            Err(error) if action.is_some() => ToolResult::ok(
                call_id,
                format!(
                    "{said}but the screen could not be captured: {}",
                    describe(error)
                ),
            ),
            Err(error) => ToolResult::refused(call_id, describe(error)),
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
            "Run a shell command on THIS BOT'S OWN computer — a sandboxed box on the server, \
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
        "open_url" => Some((
            "Open a web page in the browser on THIS BOT'S OWN computer — its sandboxed box, which \
             has a screen. The page appears on that display; use `computer` with \
             action=screenshot to see it, then click and type on it.",
            serde_json::json!({
                "type": "object",
                "properties": { "url": { "type": "string", "description": "The page to open, with its scheme (https://…)." } },
                "required": ["url"],
            }),
        )),
        "computer" => Some((
            "Use the screen of THIS BOT'S OWN computer: a 1280x800 display with a desktop, a dock \
             (Terminal, Chromium, Files) and whatever windows are open. `screenshot` returns the \
             display as an image. `click`, `right_click`, `double_click`, `move`, `drag`, `type`, \
             `key` and `scroll` act at pixel coordinates and return a fresh screenshot. Look before \
             you act, do one step at a time, and check each result before the next.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["screenshot", "click", "right_click", "double_click", "move", "drag", "type", "key", "scroll"] },
                    "coordinate": { "type": "array", "items": { "type": "integer" }, "minItems": 2, "maxItems": 2, "description": "[x, y] in display pixels, 0,0 top-left." },
                    "to": { "type": "array", "items": { "type": "integer" }, "minItems": 2, "maxItems": 2, "description": "For drag: where to drop, [x, y]." },
                    "text": { "type": "string", "description": "For type: the text to type." },
                    "key": { "type": "string", "description": "For key: a key or chord, e.g. Return, Escape, ctrl+l, alt+F4." },
                    "scroll": { "type": "array", "items": { "type": "integer" }, "minItems": 2, "maxItems": 2, "description": "For scroll: [dx, dy]; positive dy scrolls down." },
                    "button": { "type": "integer", "description": "For click: 1 left (default), 2 middle, 3 right." }
                },
                "required": ["action"],
            }),
        )),
        USER_MACHINE_SHELL => Some((
            "Run a shell command on the USER'S OWN machine — the real computer they enrolled,              NOT this bot's sandboxed box. It runs only with the user's consent under their              reverse-exec policy: a command may run, be refused, or be held for the user to approve              (in which case you should wait rather than retry). Use this ONLY when the task is about              the user's own machine; for your own work use `shell`.",
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

    // Strip EVERY identity alias the model may have written — both spellings of account/coworker/
    // box — BEFORE inserting ours. Overwriting only `coworker_id`/`box_id` left the camelCase
    // twins (`coworkerId`/`boxId`) to ride through untouched, and for a plugin tool the whole
    // argument map is forwarded to a remote server: an attacker-chosen `coworkerId` reaching a
    // connector is exactly what "overwrite, never validate" (CLAUDE.md #7) forbids. The alias list
    // is the same one the judge redacts (`review::IDENTITY_KEYS`), so the two cannot drift.
    for key in crate::review::IDENTITY_KEYS {
        object.remove(*key);
    }

    object.insert(
        "coworker_id".to_string(),
        Value::String(context.coworker_id.to_string()),
    );
    // A box is inserted when the session has one; when it does not, the key stays removed rather
    // than carrying whatever the model wrote (the strip above already took every spelling).
    if let Some(box_id) = &context.box_id {
        object.insert("box_id".to_string(), Value::String(box_id.to_string()));
    }

    Value::Object(object)
}

/// Remove the identity keys before handing arguments to a remote server.
///
/// They exist so a LOCAL tool cannot be aimed elsewhere. A remote MCP server never sees them: it
/// did not ask for them, its schema does not have them, and a strict server would reject the call
/// over a field the model never wrote. Every alias `overwrite_identity` guards against is removed
/// here too — a value the model must not choose must not leave the process under any spelling.
fn strip_identity(arguments: Value) -> Value {
    match arguments {
        Value::Object(mut map) => {
            for key in crate::review::IDENTITY_KEYS {
                map.remove(*key);
            }
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
        async fn run_recipe(&self, box_id: &str, request: &Value) -> BoxResult<Value> {
            if let Ok(mut calls) = self.ran_on.lock() {
                calls.push((box_id.to_string(), format!("recipe:{request}")));
            }
            let name = request.get("name").and_then(Value::as_str).unwrap_or("");
            if name == "stops" {
                return Ok(json!({
                    "ok": false, "ran": 1, "stopped_at": 1,
                    "steps": [{"ok": true}, {"ok": false, "error": "nothing at (5, 5)"}],
                }));
            }
            Ok(json!({
                "ok": true, "ran": 2, "stopped_at": null, "steps": [{"ok": true}, {"ok": true}],
                "screenshot": {"mime": "image/png", "png_base64": "iVBORw0KGgo=", "width": 1280, "height": 800},
            }))
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
        let executor =
            allowing(spy.clone()).with_auto_review(ReviewPolicy::default(), judge.clone());
        let result = executor
            .execute(&context_with_box("box_mine"), &shell_call("c1"))
            .await;
        assert!(result.ok, "{result:?}");
        assert_eq!(judge.calls(), 0, "nothing written ⇒ no judge call");
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

    #[tokio::test]
    async fn a_judge_outage_asks_rather_than_allows() {
        let spy = Arc::new(SpyComputer::default());
        let judge = CountingJudge::new(ReviewVerdict::Unavailable);
        let executor = allowing(spy.clone()).with_auto_review(blocking_policy(), judge);
        let result = executor
            .execute(&context_with_box("box_mine"), &shell_call("c1"))
            .await;
        assert!(result.awaiting_approval);
        assert_eq!(result.awaiting_reason, Some(AwaitingReason::AutoReview));
        assert!(result.content.contains("did not answer"), "{result:?}");
        assert_eq!(spy.last_box(), None);
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
                        json!({"command": "ls", "path": "/tmp/a", "content": "x"}),
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
    }

    #[async_trait]
    impl RecipeSource for SpyRecipes {
        async fn recipe_request(&self, recipe_id: &str) -> Result<(i32, Value), String> {
            match recipe_id {
                "rcp_gmail" => Ok((
                    2,
                    json!({"name": "Open Gmail", "steps": [], "stop_on_error": true, "screenshot": "end"}),
                )),
                "rcp_stops" => Ok((
                    3,
                    json!({"name": "stops", "steps": [], "stop_on_error": true, "screenshot": "end"}),
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
        ) {
            if let Ok(mut runs) = self.runs.lock() {
                runs.push((recipe_id.to_string(), version, receipt.ok));
            }
        }
    }

    fn offers() -> Vec<RecipeOffer> {
        vec![
            RecipeOffer {
                id: "rcp_gmail".into(),
                name: "Open Gmail".into(),
                description: "Open Gmail in Chrome and land on the inbox".into(),
            },
            RecipeOffer {
                id: "rcp_stops".into(),
                name: "Stops".into(),
                description: "a recipe whose second step fails".into(),
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
            .with_recipes(offers(), recipes.clone());
        let context = context_with_box("box_mine");

        let result = executor
            .execute(&context, &call(RUN_RECIPE, json!({"recipe": "rcp_gmail"})))
            .await;
        assert!(result.ok, "{result:?}");
        assert!(result.content.contains("Open Gmail"), "{result:?}");
        assert!(result.content.contains("v2"), "{result:?}");
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

    #[tokio::test]
    async fn a_recipe_that_stops_is_a_refusal_in_words() {
        let spy = Arc::new(SpyComputer::default());
        let recipes = Arc::new(SpyRecipes::default());
        let executor = allowing(spy)
            .with_screen(true)
            .with_recipes(offers(), recipes.clone());
        let context = context_with_box("box_mine");

        let result = executor
            .execute(&context, &call(RUN_RECIPE, json!({"recipe": "rcp_stops"})))
            .await;
        assert!(!result.ok, "{result:?}");
        assert!(result.content.contains("stopped at step 1"), "{result:?}");
        assert!(result.content.contains("nothing at (5, 5)"), "{result:?}");
        assert_eq!(
            recipes.runs.lock().unwrap().last().map(|r| r.2),
            Some(false)
        );

        // A recipe that was never granted is refused before anything runs.
        let result = executor
            .execute(&context, &call(RUN_RECIPE, json!({"recipe": "rcp_other"})))
            .await;
        assert!(!result.ok, "{result:?}");
        assert!(
            result.content.contains("not granted") || result.content.contains("no recipe"),
            "{result:?}"
        );
    }
}
