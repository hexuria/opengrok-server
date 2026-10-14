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
pub use opengrok_forms as user_form;
pub use opengrok_forms::{FormRequest, FormResolution, HAND_BACK_TOOL_RESULT, REQUEST_USER_FORM};
pub use review::{
    AwaitingReason, EGRESS_TUNNEL_ASK_REASON, Gate, JudgeFailure, Outcome, REDACTED,
    REVIEW_ASK_REASON, ReviewAsk, ReviewJudge, ReviewOutcome, ReviewPolicy, ReviewVerdict,
    ask_first_reason, combine, looks_like_a_secret, redact_arguments,
};
pub mod cards;
pub mod credential;
pub use credential::OFFER_SAVE;
pub mod mcp;
pub mod message_bot;

pub use mcp::{Endpoint, McpError, McpTool, openai_safe_tool_name};
pub use opengrok_recipes::observe;
pub use opengrok_recipes::{Observe, Seen};
pub mod computer_desk;
pub mod office_desk;
pub mod plugin_desk;
pub mod routine;
pub mod skill;
pub mod workflow;
pub use workflow::Workflow;

use std::collections::BTreeMap;
use std::sync::Arc;

use opengrok_box::{BoxError, Computer, CuaAction, Screen, Screenshot};
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
    /// An unresolved `user-form` **or** a live box handoff is open on this coworker's computer.
    /// Screen tools must not run: typing or clicking would race the person (and `computer` type
    /// would PNG a secret). Escalate settles the form but must **keep** this hold until hand-back
    /// or decline. `request_user_form` itself is not a screen action and is not held — except a
    /// second raise while this is still true, which would stack cards.
    ///
    /// PER COMPUTER, NOT PER CONVERSATION, on purpose: a coworker has one box, and a turn in
    /// another conversation clicking on the page a person is signing in on is the race this
    /// exists to stop. Do not narrow it to the conversation that raised the form.
    pub screen_hold: bool,
    /// The conversation whose form or handoff holds the screen, when the hold is tied to a run.
    /// Named in the refusal: a turn held by another conversation cannot see why otherwise.
    pub screen_held_in: Option<String>,
    /// Whether the client that sent this turn draws form-mode cards (`forwardedProps.formMode`,
    /// #220); `None` when no client sent it (a routine, a run resumed after a card). Without a
    /// yes a `request_user_form` with `formMode` is refused in words: a card nobody can draw
    /// would show the person nothing while the model is told the form is open.
    pub form_mode: Option<bool>,
    /// Which of its computer's screens this Bot uses (#376): its own on a computer it shares, the
    /// one screen on a computer of its own or a group's. One cell for every copy of the context a
    /// turn makes.
    pub screen: Arc<std::sync::Mutex<Screen>>,
    /// The thread this turn is spoken in — an office export's artifact is attached under it so
    /// the transcript can draw the file the bot delivered. `None` where no thread scopes the
    /// call (a desk answering a route, a test).
    pub thread_id: Option<String>,
    /// The run this turn is, for the artifact's `run_id`. Same caveat as `thread_id`.
    pub run_id: Option<String>,
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
            screen_hold: false,
            screen_held_in: None,
            form_mode: None,
            screen: Arc::default(),
            thread_id: None,
            run_id: None,
        }
    }

    /// The screen this Bot's screen actions go to now.
    #[must_use]
    pub fn screen(&self) -> Screen {
        self.screen.lock().map_or_else(
            |poisoned| poisoned.into_inner().clone(),
            |screen| screen.clone(),
        )
    }

    /// Point this turn's screen actions at `screen` from now on.
    pub fn set_screen(&self, screen: Screen) {
        match self.screen.lock() {
            Ok(mut cell) => *cell = screen,
            Err(poisoned) => *poisoned.into_inner() = screen,
        }
    }

    /// Where the hold is, in words the model can pass on.
    fn held_where(&self) -> String {
        match &self.screen_held_in {
            Some(thread) => format!("in conversation {thread}"),
            None => "on this coworker's computer".to_string(),
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
    /// shown it as an image. Whether a client must persist it as a chat event is
    /// [`ToolImage::visibility`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ToolImage>,
    /// A refusal that came after the call had already done part of its work: a recipe the box
    /// played until a step failed. Every other refusal came before anything ran, so a corrected
    /// call is new work and not a replay; the loop's once-per-request rule for recipes counted
    /// a missing parameter as a play and then refused the fixed call (#120). The loop's, not
    /// the wire's: never serialized.
    #[serde(skip)]
    pub stopped_part_way: bool,
    /// CUSTOM frames a tool's answer should put on the stream beside the result — the office
    /// tools' `opengrok.officeDoc` document-changed notices. The loop's, not the model's: never
    /// serialized into the result the model reads.
    #[serde(skip)]
    pub customs: Vec<(String, Value)>,
}

/// Where a tool-result image may be shown. Rides `TOOL_CALL_RESULT.image.visibility`.
///
/// Computer-step shots default to [`Agent`]: the model sees them, the Computer pane may
/// paint the live SSE, and they are **not** first-class chat events a client must store.
/// Promote to [`Transcript`] / [`Failure`] / [`End`] when the person should keep the PNG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ImageVisibility {
    /// Model context + Computer pane. Not a transcript chat event.
    #[default]
    Agent,
    /// Persist in the transcript (explicit observe / Open the screen).
    Transcript,
    /// Hard-failure pin.
    Failure,
    /// End-of-turn success pin.
    End,
}

impl ImageVisibility {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Transcript => "transcript",
            Self::Failure => "failure",
            Self::End => "end",
        }
    }
}

/// An image a tool hands back, base64 so it rides JSON as-is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolImage {
    pub mime: String,
    pub base64: String,
    pub width: u32,
    pub height: u32,
    /// Absent on rows written before this field existed: treat as `transcript` on the
    /// wire (legacy clients already stored those PNGs). New computer-step shots set
    /// [`ImageVisibility::Agent`] explicitly.
    #[serde(default)]
    pub visibility: ImageVisibility,
}

impl From<Screenshot> for ToolImage {
    fn from(shot: Screenshot) -> Self {
        Self {
            mime: shot.mime,
            base64: shot.png_base64,
            width: shot.width,
            height: shot.height,
            // Step shots are the model's eyes, not a chat event the client must journal.
            visibility: ImageVisibility::Agent,
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
            stopped_part_way: false,
            customs: Vec::new(),
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
            stopped_part_way: false,
            customs: Vec::new(),
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
            stopped_part_way: false,
            customs: Vec::new(),
        }
    }

    /// This refusal came after the box had already acted.
    #[must_use]
    pub fn part_way(mut self) -> Self {
        self.stopped_part_way = true;
        self
    }
}

/// The name of the reverse-exec tool: a shell command on the USER'S OWN machine, not the bot's box.
pub const USER_MACHINE_SHELL: &str = "user_machine_shell";
/// Why a call to it is refused when the coworker's ceiling has the machine switched off (#268).
const MACHINE_SWITCHED_OFF: &str =
    "this coworker's tool ceiling does not let it use your machine; its owner can switch that on";

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

/// `grok-box:local` ships a desktop and Chromium. It does not ship these binaries,
/// and the box has no Rust toolchain to build them. Checked by running the image.
const BIR_NOT_ON_THE_BOX: &str =
    "gpui-agent and bir-headless are not on this computer. Use user_machine_shell.";

/// A box `shell` token that would execute a BIR binary, including a path
/// (`/usr/local/bin/gpui-agent`) and a download whose last segment is that name.
/// An env assignment (`GPUI_AGENT_ADDR=...`) is not a command.
fn command_targets_absent_bir_binary(command: &str) -> bool {
    command
        .split(|ch: char| ch.is_whitespace() || matches!(ch, ';' | '|' | '&'))
        .any(token_is_absent_bir_binary)
}

fn token_is_absent_bir_binary(token: &str) -> bool {
    let token = token.trim_matches(|ch| matches!(ch, '"' | '\'' | '(' | ')'));
    if token.is_empty() || token.contains('=') {
        return false;
    }
    matches!(
        token.rsplit('/').next(),
        Some("gpui-agent" | "bir-headless")
    )
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecipeOffer {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub parameters: Vec<opengrok_recipes::Parameter>,
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
                visibility: ImageVisibility::Agent,
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

/// A run written down before the box is touched, holding the bot's screen until it is dropped.
/// `hold` is whatever keeps the claim alive (the server's lease renewal); dropping the claim
/// stops it, and a run that never lands then reads as interrupted once its lease lapses.
pub struct RecipeClaim {
    pub id: String,
    pub hold: Box<dyn Send + Sync>,
}

/// Where the executor gets a recipe's steps from, and tells what a run did — the server
/// implements it over the store, so this crate stays free of Postgres.
#[async_trait::async_trait]
pub trait RecipeSource: Send + Sync {
    /// The runnable version of a recipe: `(version, the box's request body)`.
    /// Values bind parameters to their supplied values; pass an empty map for recipes with no parameters.
    async fn recipe_request(
        &self,
        recipe_id: &str,
        values: &opengrok_recipes::Values,
    ) -> Result<(i32, Value), String>;
    /// Write the run down, and say what id it was written under — what a workflow's trail points
    /// at, so a person reading a branch can open the recipe run that branch actually made.
    /// `None` from an implementation that keeps no history to point at. `claimed` is the id
    /// `claim_run` wrote, finished in place; without one a new row is written.
    async fn record_run(
        &self,
        recipe_id: &str,
        version: i32,
        coworker_id: &CoworkerId,
        receipt: &RecipeReceipt,
        claimed: Option<&str>,
    ) -> Option<String>;
    /// Take the bot's screen for one run before the box is touched, or the sentence saying why
    /// not (another run is playing on it). `None`: a source that keeps no leases.
    async fn claim_run(
        &self,
        _recipe_id: &str,
        _version: i32,
        _coworker_id: &CoworkerId,
    ) -> Result<Option<RecipeClaim>, String> {
        Ok(None)
    }
    /// Whether this bot may play the recipe NOW, or the sentence saying why not.
    ///
    /// The offers a turn carries were read when the turn began, and a turn can outlast the share
    /// that granted them: taken back mid-turn, the recipe must stop at the next play, not the next
    /// turn. Default yes, for a source that keeps no grants to re-read.
    async fn still_granted(
        &self,
        _recipe_id: &str,
        _coworker_id: &CoworkerId,
    ) -> Result<(), String> {
        Ok(())
    }
}

/// The arguments `run_recipe` accepts.
#[derive(Debug, Clone, Deserialize)]
pub struct RunRecipeArgs {
    pub recipe: String,
    #[serde(default)]
    pub values: Option<opengrok_recipes::Values>,
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

/// What a box-bound tool answers when the box cannot be brought up. The model relays it in its
/// own words; the person is the one who can look at the computer.
pub const COMPUTER_DOWN: &str = "my computer is down; ask them to check it";

/// What a box-bound tool answers when the box is still on its way up after the patience — a
/// box.ascii.dev restore can take longer than a short patience — so the caller retries rather
/// than telling the person the computer is broken.
pub const COMPUTER_STARTING: &str = "my computer is still starting; try again in a moment";

/// Told the box id after an executor woke a box (or first found it running), so the server can
/// stamp it as in use for the idle sweep.
pub type OnWoken = Arc<dyn Fn(&str) + Send + Sync>;

pub use opengrok_policy::egress::{EgressPolicy, EgressTunnelMode, NETWORK_OFF};

/// The wait for a sleeping box, when nobody said otherwise. The server passes its own.
const DEFAULT_WAKE_PATIENCE: std::time::Duration = std::time::Duration::from_secs(90);

/// The tools whose action can leave the box for the network. By tool name, for what must hold
/// whatever the action: a form's screen hold and a standing `never`. A screenshot taken while
/// a sign-in handoff is open can capture the secret being typed, so it stays held.
pub fn leaves_the_box(tool_name: &str) -> bool {
    matches!(tool_name, "computer" | "open_url" | RUN_RECIPE)
}

/// The calls the egress tunnel's card is about: a leave-box call whose action can reach the
/// network. A `computer` screenshot only reads the box's own display and sends nothing through
/// the person's network, yet it raised the card like a click (#165). It is the one exemption:
/// `move` can prefetch, `scroll` can lazy-load, `key` can submit, so every other action asks,
/// and so does anything not spelt exactly `"action": "screenshot"` — missing, misspelt, another
/// case, not a string. One predicate for the ask, the "waking" frame and the resume paths, or
/// the frame says the box is waking for a call that parks on a card.
pub fn needs_egress_consent(tool_name: &str, arguments: &Value) -> bool {
    leaves_the_box(tool_name)
        && !(tool_name == "computer"
            && arguments.get("action").and_then(Value::as_str) == Some("screenshot"))
}

/// The tools that run on the box (as opposed to plugin tools and the person's own machine).
fn needs_the_box(tool_name: &str) -> bool {
    matches!(
        tool_name,
        RUN_RECIPE | "shell" | "read_file" | "write_file" | "open_url" | "computer"
    ) || (office_desk::is_office_tool(tool_name) && tool_name != office_desk::OFFICE_CLOSE)
}

/// Which box a call targets: the room's shared one when `machine` is `group`, else the
/// coworker's own. `None` when that box does not exist.
fn target_box<'a>(context: &'a ToolContext, arguments: &Value) -> Option<&'a BoxId> {
    if arguments.get("machine").and_then(Value::as_str) == Some("group") {
        context.group_box.as_ref().map(|group| &group.box_id)
    } else {
        context.box_id.as_ref()
    }
}

mod executor;

pub use executor::*;

#[cfg(test)]
#[path = "../tests/unit/lib_tests.rs"]
#[allow(clippy::unwrap_used)]
mod tests;
