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
    /// Which of its computer's screens this Bot uses (#376): the shared one, or its own when it
    /// was told to use one. One cell for every copy of the context a turn makes, so a switch
    /// (`use_own_screen`) is where the turn's next screen action lands.
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

/// Whether leave-box tools (`computer`, `open_url`, `run_recipe`) raise the Review-an-action card
/// for the egress tunnel before they run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EgressTunnelMode {
    /// No tunnel: nothing to ask about.
    Off,
    /// The box's guest said the tunnel is up with a client attached: ask before the first leave-box
    /// tool.
    On,
    /// The host wants the tunnel but the box was asleep when the turn started, so the guest could
    /// not be asked. Ask it right after the first leave-box tool wakes the box, and raise the card
    /// only if it says the tunnel is really there — a card that asserts a tunnel that is not
    /// attached would be a lie.
    AskTheBoxAfterWake,
}

/// The person's standing answer, per computer, to the tunnel's Review-an-action card. Stored as
/// the reverse-exec channel's words (`bypass` | `ask` | `never`) so one vocabulary serves both.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum EgressPolicy {
    /// Never raise the tunnel card for this computer: every leave-box action may use the
    /// person's network.
    Always,
    /// One card per run, as before. What an unset policy means.
    #[default]
    Ask,
    /// This computer may not use the person's network: while the tunnel is on for it, the
    /// leave-box tools are not offered at all, and one that arrives anyway is refused.
    Never,
}

impl EgressPolicy {
    /// The stored word; anything unknown reads as `Ask`, which is what no row means too.
    pub fn from_stored(mode: &str) -> Self {
        match mode {
            "bypass" => Self::Always,
            "never" => Self::Never,
            _ => Self::Ask,
        }
    }

    pub fn as_stored(self) -> &'static str {
        match self {
            Self::Always => "bypass",
            Self::Ask => "ask",
            Self::Never => "never",
        }
    }

    pub fn is_valid(mode: &str) -> bool {
        matches!(mode, "bypass" | "ask" | "never")
    }
}

/// What a leave-box tool answers when the computer's network use is switched off.
pub const NETWORK_OFF: &str =
    "this computer's use of the person's network is switched off, so it cannot reach the web";

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

/// Runs tool calls on the caller's own computer, if policy allows.
pub struct Executor {
    computer: Arc<dyn Computer>,
    /// How long the first box-bound call of a turn waits for a sleeping box to come up. A turn
    /// no longer waits before the model is asked; it waits here, once, when a tool needs the box.
    wake_patience: std::time::Duration,
    /// What this turn learnt about each box it needed: `Ok` once found running (or woken), `Err`
    /// with the sentence to answer once it would not come up. Asked once per box per turn, not
    /// once per call — a box that is down costs one wait, not one per call.
    woken: std::sync::Mutex<std::collections::BTreeMap<String, Result<(), String>>>,
    /// Told the box id after this executor woke a box, so the server can stamp it as in use.
    on_woken: Option<OnWoken>,
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
    /// Connected once per request rather than per call — and pooled across requests by
    /// `mcp::Pool` — so a turn that reaches for three tools on one server hand-shakes at most once.
    sessions: BTreeMap<String, Arc<crate::mcp::Session>>,
    /// Every plugin tool on offer, in the order a model is told about them.
    plugin_tools: Vec<crate::mcp::McpTool>,
    /// Plugin servers that should have been reached this request and were not, with the reason.
    unavailable_plugins: BTreeMap<String, String>,
    /// The reverse-exec bridge, present ONLY when this account has an enrolled, enabled machine.
    /// Its presence, with a ceiling that allows it, is what advertises `user_machine_shell` — the
    /// tool exists iff a machine can actually be reached.
    user_machine: Option<Arc<dyn UserMachineSink>>,
    /// Auto-review, when the run's effective policy is on: the instruction texts (resolved once
    /// per run by the server) and the judge that reads them. `None` is the cheapest short-circuit.
    auto_review: Option<AutoReview>,
    /// The judge's failures in a row in this run. Seeded by the server from the run's journal
    /// on a resume: every failure parks the run on a card and the executor is rebuilt, so a
    /// count that began at zero here would never pass one.
    judge_failures: std::sync::atomic::AtomicU32,
    /// The box has a display. Only then are `open_url` and `computer` offered: a headless box
    /// would refuse every call, and a tool that always refuses is a dead end the model retries.
    screen: bool,
    /// The name of the group whose shared computer this turn may use, when there is one; it
    /// puts `machine` on the box tools' schemas.
    group_box_name: Option<String>,
    /// The taught recipes this bot may run; `run_recipe` is offered only when there are any.
    recipes: Vec<RecipeOffer>,
    recipe_source: Option<Arc<dyn RecipeSource>>,
    /// The recipe the PERSON chose in the composer this turn, and the values they typed for it.
    /// Kept apart from whatever the model passes, because when the two disagree the person wins.
    chosen_recipe: Option<(String, opengrok_recipes::Values)>,
    /// How much of the desktop a recipe run asks the box to report back. Held here rather than
    /// read at the call, so the level a turn runs at is decided once when the turn is built and
    /// cannot change between two calls of the same conversation.
    observe: crate::observe::Observe,
    /// Prod user-network path: the box agent's egress tunnel. Docker host-network is not this.
    /// When on, leave-box screen tools raise the Review-an-action card unless a standing
    /// auto-review allow is already attached.
    egress_tunnel: EgressTunnelMode,
    /// Boxes whose guest, asked once a tool woke it, said the tunnel is there (the
    /// `AskTheBoxAfterWake` mode). Only a yes is kept: right after a wake the guest is usually
    /// not answering yet, and a remembered "no" would skip the consent card for the whole run.
    egress_after_wake: std::sync::Mutex<std::collections::BTreeSet<String>>,
    /// The person already said yes, in this run, to a leave-box action — the tunnel's card, or a
    /// judge's card on the same kind of action — so the tunnel is not asked about again.
    egress_consented: bool,
    /// The person's standing answer for this computer. `Always` is consent given in advance;
    /// `Never`, with the tunnel on, takes the leave-box tools off the offer (see `has_screen`).
    egress_policy: EgressPolicy,
    /// The policy could not be read this turn and `Never` is the fail-closed stand-in, not the
    /// person's word: the prompt must not say they chose it.
    egress_policy_unconfirmed: bool,
    /// The person's routines (#316), and whether this run may only list them: a run a routine
    /// started may not make one, or a routine could breed routines. `None` offers none.
    routines: Option<(Arc<dyn routine::RoutineDesk>, bool)>,
    /// The person's plugins and accounts (#359), through the desk the Plugins routes use. `None`
    /// offers none.
    plugin_desk: Option<Arc<dyn plugin_desk::PluginDesk>>,
    /// This Bot's own computer (7 Oct 2026), through the desk the Computer pane's routes use.
    /// `None` offers none.
    computer_desk: Option<Arc<dyn computer_desk::ComputerDesk>>,
    /// The office documents on it, through the desk `doc_session` rows and box bytes answer.
    /// `None` offers none.
    office_desk: Option<Arc<dyn office_desk::OfficeDesk>>,
    /// A switched-on plugin's tools the person switched off for this Bot (`with_switched_off`).
    switched_off: Vec<crate::mcp::McpTool>,
    /// A Bot with no computer that still has plugin tools (`without_a_computer`): no built-in is
    /// offered, and one called anyway is refused in words rather than sent to a box that is not
    /// there.
    boxless: bool,
}

/// The box a Bot with no computer is given so its plugin tools can run: every box call is
/// refused, and none is ever offered (`Executor::without_a_computer`).
struct NoComputer;

#[async_trait::async_trait]
impl Computer for NoComputer {
    async fn create(&self, _ttl: Option<u64>) -> opengrok_box::BoxResult<String> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn run(
        &self,
        _box_id: &str,
        _command: &str,
        _timeout: u32,
    ) -> opengrok_box::BoxResult<opengrok_box::CommandOutput> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn start(
        &self,
        _box_id: &str,
        _command: &str,
    ) -> opengrok_box::BoxResult<opengrok_box::StartedCommand> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn watch(
        &self,
        _box_id: &str,
        _process: &str,
    ) -> opengrok_box::BoxResult<opengrok_box::StartedCommand> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn read_file(&self, _box_id: &str, _path: &str) -> opengrok_box::BoxResult<String> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn write_file(
        &self,
        _box_id: &str,
        _path: &str,
        _content: &str,
    ) -> opengrok_box::BoxResult<()> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn expose_port(
        &self,
        _box_id: &str,
        _port: u16,
        _title: &str,
    ) -> opengrok_box::BoxResult<String> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn stop(&self, _box_id: &str) -> opengrok_box::BoxResult<()> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn resume(&self, _box_id: &str) -> opengrok_box::BoxResult<()> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn destroy(&self, _box_id: &str) -> opengrok_box::BoxResult<()> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn state(&self, _box_id: &str) -> opengrok_box::BoxResult<String> {
        Ok("absent".to_string())
    }
}

/// What a Bot with no computer is told when it calls a computer tool anyway.
pub const NO_COMPUTER: &str =
    "This Bot has no computer, so it cannot run computer tools. Its plugins' tools still work.";

/// The built-ins that need a display.
const SCREEN_TOOLS: &[&str] = &["open_url", "computer"];
/// The built-ins that exist to work a web page in the box's browser: the screen tools, and the
/// one that hands a page's login to the person. Withheld together when the computer's use of the
/// person's network is switched off, so none of them is an advertised dead end.
const BROWSER_TOOLS: &[&str] = &["open_url", "computer", RUN_RECIPE, REQUEST_USER_FORM];
/// The recipe tool's name; offered next to the screen tools, gated the same way.
pub const RUN_RECIPE: &str = "run_recipe";
/// What `run_recipe` does; its schema lists the recipes this bot may run after these words.
const RUN_RECIPE_DESCRIPTION: &str = "Run a task a person taught on THIS BOT'S OWN computer, \
     as one step, instead of looking and clicking your way through it. Use one when the request \
     matches its description, and say which you used.";

/// The auto-review pair a run carries.
struct AutoReview {
    policy: ReviewPolicy,
    judge: Arc<dyn ReviewJudge>,
}

impl AutoReview {
    /// One question, one word back, rendered into what the ladder needs. The judge sees the
    /// arguments AFTER identity overwrite and redaction, never as the model wrote them.
    ///
    /// `failures` is the run's count of judge failures in a row. Once it reaches
    /// `JUDGE_DOWN_AFTER` the judge is not asked again this run: the call is refused in words,
    /// which the model passes on once, instead of one more card and one more billed attempt.
    async fn judge(
        &self,
        tool: &str,
        call_id: &str,
        arguments: &Value,
        failures: &std::sync::atomic::AtomicU32,
    ) -> ReviewOutcome {
        use std::sync::atomic::Ordering;
        let streak = failures.load(Ordering::Relaxed);
        if streak >= review::JUDGE_DOWN_AFTER {
            tracing::warn!(
                call = call_id,
                tool,
                failures = streak,
                "auto-review judge is down for this run; refusing the call without asking it"
            );
            return ReviewOutcome::Block(review::judge_down_reason(streak));
        }
        let redacted = redact_arguments(arguments);
        let verdict = self
            .judge
            .judge(ReviewAsk {
                call_id,
                tool,
                arguments: &redacted,
                allow_instructions: &self.policy.allow_instructions,
                ask_instructions: &self.policy.ask_instructions,
                block_instructions: &self.policy.block_instructions,
            })
            .await;
        if matches!(verdict, ReviewVerdict::Unavailable(_)) {
            failures.fetch_add(1, Ordering::Relaxed);
        } else {
            failures.store(0, Ordering::Relaxed);
        }
        let (ask, block) = (
            &self.policy.ask_instructions,
            &self.policy.block_instructions,
        );
        match verdict {
            ReviewVerdict::Allow => ReviewOutcome::Allow,
            // A refusal names a rule the person wrote. A "block" or "ask" with no such list
            // behind it is the judge's guess, and a guess asks; it never refuses.
            ReviewVerdict::Block if !block.trim().is_empty() => {
                ReviewOutcome::Block(review::block_refusal(block))
            }
            ReviewVerdict::Ask if !ask.trim().is_empty() => {
                ReviewOutcome::Ask(ask_first_reason(ask))
            }
            ReviewVerdict::Block | ReviewVerdict::Ask | ReviewVerdict::Unsure => {
                ReviewOutcome::Ask(review::REVIEW_ASK_REASON.to_string())
            }
            ReviewVerdict::Unavailable(cause) => {
                ReviewOutcome::Ask(review::unavailable_reason(cause))
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
            wake_patience: DEFAULT_WAKE_PATIENCE,
            woken: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            on_woken: None,
            approved_calls: std::collections::BTreeSet::new(),
            sessions: BTreeMap::new(),
            plugin_tools: Vec::new(),
            unavailable_plugins: BTreeMap::new(),
            user_machine: None,
            auto_review: None,
            judge_failures: std::sync::atomic::AtomicU32::new(0),
            review_approved_calls: std::collections::BTreeSet::new(),
            screen: false,
            group_box_name: None,
            recipes: Vec::new(),
            recipe_source: None,
            chosen_recipe: None,
            observe: crate::observe::wanted(),
            egress_tunnel: EgressTunnelMode::Off,
            egress_after_wake: std::sync::Mutex::new(std::collections::BTreeSet::new()),
            egress_consented: false,
            egress_policy: EgressPolicy::default(),
            egress_policy_unconfirmed: false,
            routines: None,
            plugin_desk: None,
            computer_desk: None,
            office_desk: None,
            switched_off: Vec::new(),
            boxless: false,
        }
    }

    /// The executor a real request builds: a computer, and what this principal may do with it.
    pub fn with_policy(computer: Arc<dyn Computer>, policy: opengrok_policy::Context) -> Self {
        Self {
            policy,
            ..Self::new(computer)
        }
    }

    /// An executor for a Bot with no computer: only what is attached to it (its plugins) is
    /// offered. A plugin is the person's account at a service, not something that runs on a box,
    /// so a Bot without one still uses it (#359).
    pub fn without_a_computer(policy: opengrok_policy::Context) -> Self {
        Self {
            boxless: true,
            ..Self::with_policy(Arc::new(NoComputer), policy)
        }
    }

    /// Whether a box stands behind this executor.
    pub fn has_computer(&self) -> bool {
        !self.boxless
    }

    /// Offer the routine tools over `desk`, where the ceiling and the grant allow each (#316).
    #[must_use]
    pub fn with_routines(mut self, desk: Arc<dyn routine::RoutineDesk>) -> Self {
        self.routines = Some((desk, false));
        self
    }

    /// Offer the plugin tools over `desk`, where the ceiling and the grant allow each (#359).
    #[must_use]
    pub fn with_plugin_desk(mut self, desk: Arc<dyn plugin_desk::PluginDesk>) -> Self {
        self.plugin_desk = Some(desk);
        self
    }

    /// Offer the computer tools over `desk`, where the ceiling and the grant allow each.
    #[must_use]
    pub fn with_computer_desk(mut self, desk: Arc<dyn computer_desk::ComputerDesk>) -> Self {
        self.computer_desk = Some(desk);
        self
    }

    /// Offer the office tools over `desk` — only ever attached where a box is, since every
    /// verb but `office_close` reads or writes box bytes.
    #[must_use]
    pub fn with_office_desk(mut self, desk: Arc<dyn office_desk::OfficeDesk>) -> Self {
        self.office_desk = Some(desk);
        self
    }

    /// The office tools this run is offered: all of them with a desk, none without.
    fn office_desk_tools(&self) -> impl Iterator<Item = &'static str> + '_ {
        let take = if self.office_desk.is_some() {
            office_desk::TOOLS.len()
        } else {
            0
        };
        office_desk::TOOLS.into_iter().take(take)
    }

    /// The computer tools this run is offered: all of them with a desk, none without.
    fn computer_desk_tools(&self) -> impl Iterator<Item = &'static str> + '_ {
        let take = if self.computer_desk.is_some() {
            computer_desk::TOOLS.len()
        } else {
            0
        };
        computer_desk::TOOLS.into_iter().take(take)
    }

    /// The plugin tools this run is offered: all of them with a desk, none without.
    fn plugin_desk_tools(&self) -> impl Iterator<Item = &'static str> + '_ {
        let take = if self.plugin_desk.is_some() {
            plugin_desk::TOOLS.len()
        } else {
            0
        };
        plugin_desk::TOOLS.into_iter().take(take)
    }

    /// Only `list_routines`, for a run a routine started: it may not make or change one.
    #[must_use]
    pub fn with_routines_listing_only(mut self) -> Self {
        self.routines = self.routines.map(|(desk, _)| (desk, true));
        // Nor any plugin tool (#359): nobody is watching such a run to answer a card or finish a
        // sign-in, and a routine must not install or switch on what its person never chose.
        self.plugin_desk = None;
        // Nor a computer tool: nobody is watching to answer a reset's card.
        self.computer_desk = None;
        self.office_desk = None;
        self
    }

    /// The routine tools this run is offered: none without a desk, only the listing for a run a
    /// routine started.
    fn routine_tools(&self) -> impl Iterator<Item = &'static str> + '_ {
        let listing = self.routines.as_ref().map(|(_, listing)| *listing);
        let take = listing.map_or(0, |listing| if listing { 1 } else { routine::TOOLS.len() });
        routine::TOOLS.into_iter().take(take)
    }

    /// Say the box has a display, so the screen tools are offered and run.
    #[must_use]
    pub fn with_screen(mut self, screen: bool) -> Self {
        self.screen = screen;
        self
    }

    /// Whether the screen tools are on offer — the prompt must say the same thing the offering does.
    /// A box whose network use is switched off has its screen tools withheld too: every one of
    /// them leaves the box for the web, and a tool the model is told about but that always
    /// refuses is a dead end it keeps trying.
    pub fn has_screen(&self) -> bool {
        self.screen && !self.network_off()
    }

    /// The computer may not use the person's network, and the box's guest says a tunnel is
    /// attached that would carry it: the browser tools are withheld and the prompt says why.
    /// Host intent alone (`AskTheBoxAfterWake`, the box asleep) is not enough — `ask` raises
    /// no card then either; a `never` that withheld more than `ask` asks about would say the
    /// person switched off something that was not carrying anything. The after-wake check
    /// refuses in words instead, once the guest has been asked.
    pub fn network_off(&self) -> bool {
        self.egress_policy == EgressPolicy::Never && self.egress_tunnel == EgressTunnelMode::On
    }

    /// `network_off`, asked once the box is awake: under a standing `never` with the tunnel
    /// mode undecided at turn start (`AskTheBoxAfterWake`), the woken guest's word settles it.
    /// For the paths that wake a box and act on it outside `execute` (the user-form fill).
    pub async fn network_off_now(&self, box_id: &str) -> bool {
        self.network_off()
            || (self.egress_policy == EgressPolicy::Never
                && self.egress_tunnel == EgressTunnelMode::AskTheBoxAfterWake
                && self.tunnel_is_there(box_id).await)
    }

    /// The person's standing answer for this computer to the tunnel's card.
    #[must_use]
    pub fn with_egress_policy(mut self, policy: EgressPolicy) -> Self {
        self.egress_policy = policy;
        self
    }

    /// The policy is a fail-closed stand-in this turn, not something the person chose.
    #[must_use]
    pub fn with_egress_policy_unconfirmed(mut self, unconfirmed: bool) -> Self {
        self.egress_policy_unconfirmed = unconfirmed;
        self
    }

    /// The browser is withheld this turn because the policy could not be read, not because
    /// the person said no — the prompt says so in those words.
    pub fn network_unconfirmed(&self) -> bool {
        self.egress_policy_unconfirmed && self.network_off()
    }

    /// How long the first box-bound tool call of a turn waits for a sleeping box.
    #[must_use]
    pub fn with_wake_patience(mut self, patience: std::time::Duration) -> Self {
        self.wake_patience = patience;
        self
    }

    /// Whether running `call` would first have to wake the box it targets: the call is box-bound,
    /// the box is known, and it is not running yet as far as this turn has seen. The harness asks
    /// before running a round so it can say "waking the computer" on the stream.
    pub async fn box_needs_wake(&self, context: &ToolContext, call: &ToolCall) -> bool {
        let tool_name = self.internal_tool_name(&call.name);
        if self.boxless {
            return false;
        }
        if !needs_the_box(&tool_name) || !self.would_reach_the_box(context, call, &tool_name) {
            return false;
        }
        let Some(box_id) = target_box(context, &call.arguments) else {
            return false;
        };
        if self.box_outcome(box_id.as_str()).is_some() {
            return false;
        }
        match self.computer.state(box_id.as_str()).await {
            Ok(state) if state == "running" => {
                self.remember_box(box_id.as_str(), Ok(()));
                false
            }
            Ok(state) if state == "absent" => false,
            _ => true,
        }
    }

    /// The cheap half of `execute`'s admission, for the frame that says "waking": a call the
    /// policy denies or parks on a card, a screen tool while a form holds the screen, or a screen
    /// tool the egress tunnel will ask about first, never reaches the box, so nothing wakes.
    fn would_reach_the_box(&self, context: &ToolContext, call: &ToolCall, tool_name: &str) -> bool {
        // A routine is the server's (#316), and so is a plugin tool (#359): neither touches the
        // box, so nothing wakes.
        if routine::is_routine_tool(tool_name)
            || plugin_desk::is_plugin_desk_tool(tool_name)
            || computer_desk::is_computer_tool(tool_name)
        {
            return false;
        }
        let decision = opengrok_policy::decide(
            &context.account_id,
            &context.coworker_id,
            opengrok_policy::Action::RunTool(tool_name),
            &self.policy,
        );
        let gate_approved = self.approved_calls.contains(&call.id);
        let review_approved = gate_approved || self.review_approved_calls.contains(&call.id);
        // A deny is a deny, approved or not; an ask is released by the gate's own yes.
        if decision.reason().is_some() && !decision.needs_approval() {
            return false;
        }
        if decision.needs_approval() && !gate_approved {
            return false;
        }
        let screen_tool = leaves_the_box(tool_name);
        if context.screen_hold && screen_tool {
            return false;
        }
        if self.network_off() && screen_tool {
            return false;
        }
        let review_inactive = self
            .auto_review
            .as_ref()
            .is_none_or(|review| !review.policy.is_active());
        // In the after-wake mode the box is woken before the tunnel is asked about, so the frame
        // is right either way.
        if self.egress_tunnel == EgressTunnelMode::On
            && needs_egress_consent(tool_name, &call.arguments)
            && review_inactive
            && !review_approved
            && !self.egress_consented()
        {
            return false;
        }
        true
    }

    /// Consent to leave through the tunnel for this run: given on a card in this run, or given
    /// in advance for this computer (`EgressPolicy::Always`).
    fn egress_consented(&self) -> bool {
        self.egress_consented || self.egress_policy == EgressPolicy::Always
    }

    fn box_outcome(&self, box_id: &str) -> Option<Result<(), String>> {
        self.woken
            .lock()
            .ok()
            .and_then(|woken| woken.get(box_id).cloned())
    }

    fn remember_box(&self, box_id: &str, outcome: Result<(), String>) {
        if let Ok(mut woken) = self.woken.lock() {
            woken.insert(box_id.to_string(), outcome);
        }
    }

    /// The box was seen running, or woken, by the time this returns `Ok` — once per box per
    /// turn; the answer is remembered either way, so a box that is down costs one wait and every
    /// later call in the turn gets the same sentence at once. `Err` is what the model is told and
    /// relays: the computer is down (only the person can look at it), or still starting (retry).
    async fn ensure_awake(&self, box_id: &str) -> Result<(), String> {
        if let Some(outcome) = self.box_outcome(box_id) {
            return outcome;
        }
        let (outcome, woke) = self.wake_now(box_id).await;
        // "Still starting" invites a retry, so it is the one answer not remembered: the next
        // call asks the provider again and finds the box up, instead of being told the same
        // sentence at once and giving up on it.
        if !outcome
            .as_ref()
            .is_err_and(|why| why.starts_with(COMPUTER_STARTING))
        {
            self.remember_box(box_id, outcome.clone());
        }
        if woke && let Some(on_woken) = &self.on_woken {
            on_woken(box_id);
        }
        outcome
    }

    /// The outcome, and whether this call actually brought the box up (as opposed to finding it
    /// running), which is what the in-use stamp is for.
    async fn wake_now(&self, box_id: &str) -> (Result<(), String>, bool) {
        let state = match self.computer.state(box_id).await {
            Ok(state) => state,
            Err(error) => return (Err(format!("{COMPUTER_DOWN} ({error})")), false),
        };
        if state == "running" {
            return (Ok(()), false);
        }
        let reached = match self.computer.wake(box_id, self.wake_patience).await {
            Ok(reached) => reached,
            Err(error) => return (Err(format!("{COMPUTER_DOWN} ({error})")), false),
        };
        if reached == "running" {
            (Ok(()), true)
        } else if opengrok_box::is_starting(&reached) {
            (Err(format!("{COMPUTER_STARTING} (it is {reached})")), false)
        } else {
            (Err(format!("{COMPUTER_DOWN} (it is {reached})")), false)
        }
    }

    /// Bring the coworker's own box up for something that types into it outside `execute` — the
    /// user-form fill — through the same memo and in-use stamp as a tool call.
    pub async fn wake_own_box(&self, context: &ToolContext) -> Result<(), String> {
        let Some(box_id) = context.box_id.as_ref() else {
            return Err("this coworker has no computer yet".to_string());
        };
        self.ensure_awake(box_id.as_str()).await
    }

    /// Whether the (now awake) box's guest reports the egress tunnel up with a client attached.
    /// A yes is remembered for the turn; a no is asked again next time, because right after a
    /// wake the guest and the laptop client are usually still coming back.
    async fn tunnel_is_there(&self, box_id: &str) -> bool {
        if self
            .egress_after_wake
            .lock()
            .is_ok_and(|known| known.contains(box_id))
        {
            return true;
        }
        let there =
            opengrok_box::EgressTunnel::advertised(true, self.computer.egress_tunnel(box_id).await);
        if there && let Ok(mut known) = self.egress_after_wake.lock() {
            known.insert(box_id.to_string());
        }
        there
    }

    /// Called with the box id after this executor brought a box up (or found it up), so the
    /// server can stamp it as in use for the idle sweep.
    #[must_use]
    pub fn with_on_woken(mut self, on_woken: OnWoken) -> Self {
        self.on_woken = Some(on_woken);
        self
    }

    /// The box this executor talks to. Fill (`user-form`) types here outside `computer_use`.
    #[must_use]
    pub fn computer(&self) -> Arc<dyn Computer> {
        self.computer.clone()
    }

    /// How much of the desktop a recipe run asks the box to report back, when it should not be
    /// the deployment's own setting. The level is a cost paid in playback time, so it is
    /// settable rather than fixed.
    #[must_use]
    pub fn with_observe(mut self, observe: crate::observe::Observe) -> Self {
        self.observe = observe;
        self
    }

    /// What the person picked in the composer: a recipe, and the values they filled in for it.
    ///
    /// A model that is told a recipe was chosen still has to call it, and it may supply values of
    /// its own — read off the sentence, or guessed. These are neither: they were typed into named
    /// fields, so they override.
    #[must_use]
    pub fn with_chosen_recipe(
        mut self,
        recipe_id: impl Into<String>,
        values: opengrok_recipes::Values,
    ) -> Self {
        self.chosen_recipe = Some((recipe_id.into(), values));
        self
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
        self.has_screen() && !self.recipes.is_empty()
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
    /// enabled machine; `user_machine_shell` is then offered where the coworker's ceiling allows.
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

    /// Prod traffic reroute: leave-box tools (`computer`, `open_url`, `run_recipe`) raise the
    /// Review-an-action card before they run. Docker host-network is not this path.
    #[must_use]
    pub fn with_egress_tunnel(mut self, on: bool) -> Self {
        self.egress_tunnel = if on {
            EgressTunnelMode::On
        } else {
            EgressTunnelMode::Off
        };
        self
    }

    /// See [`EgressTunnelMode`].
    #[must_use]
    pub fn with_egress_tunnel_mode(mut self, mode: EgressTunnelMode) -> Self {
        self.egress_tunnel = mode;
        self
    }

    /// The person already said yes, in this run, to a leave-box action; the tunnel is not asked
    /// about again. Set by the resume paths from the answered card's own tool, not inferred.
    #[must_use]
    pub fn with_egress_consented(mut self, consented: bool) -> Self {
        self.egress_consented = consented;
        self
    }

    /// The judge's failures in a row so far in this run, read back from its journal by the
    /// resume paths. See `judge_failures`.
    #[must_use]
    pub fn with_judge_failures(self, failures: u32) -> Self {
        self.judge_failures
            .store(failures, std::sync::atomic::Ordering::Relaxed);
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

    /// Whether the person chose Always allow for `tool`, which otherwise asks first by rule (a
    /// delete, an uninstall, a removal): `+<tool>` in this Bot's ceiling. It admits nothing by
    /// itself; the tool must still be allowed.
    pub fn asks_unasked(&self, tool: &str) -> bool {
        match self.policy.ceiling.as_ref().map(|ceiling| &ceiling.tools) {
            Some(opengrok_policy::ToolSet::Only(names)) => names.contains(&format!("+{tool}")),
            _ => false,
        }
    }

    /// The tools that ask first by rule, whatever the grant says, unless the person chose Always
    /// allow (`asks_unasked`).
    pub const ASK_BY_RULE: [&'static str; 6] = [
        routine::DELETE_ROUTINE,
        plugin_desk::UNINSTALL_PLUGIN,
        plugin_desk::REMOVE_PLUGIN_ACCOUNT,
        computer_desk::RESET_COMPUTER,
        computer_desk::UPDATE_COMPUTER,
        computer_desk::SET_NETWORK,
    ];

    /// The label a decided tool gets: an Allow still asks first on the ask-by-rule list unless
    /// the person chose Always allow for it.
    fn mode_label(&self, decision: &opengrok_policy::Decision, tool: &str) -> &'static str {
        if decision.needs_approval() {
            "ask"
        } else if !decision.is_allowed() {
            "never"
        } else if Self::ASK_BY_RULE.contains(&tool) && !self.asks_unasked(tool) {
            "ask"
        } else {
            "always"
        }
    }

    /// A tool's choice for this Bot as a person reads it: `ask` (a card first), `always`, or
    /// `never` (not offered). `tool` is the dotted name for a plugin's tool.
    pub fn mode_of(&self, account: &AccountId, coworker: &CoworkerId, tool: &str) -> &'static str {
        let decision = opengrok_policy::decide(
            account,
            coworker,
            opengrok_policy::Action::RunTool(tool),
            &self.policy,
        );
        self.mode_label(&decision, tool)
    }

    /// Whether `group`'s own switch is off for this Bot (7 Oct 2026): switched off as a whole,
    /// or none of its tools in the ceiling at all.
    pub fn group_off(&self, group: &str) -> bool {
        let Some(ceiling) = self.policy.ceiling.as_ref() else {
            return true;
        };
        ceiling.tools.group_off(group)
            || opengrok_policy::TOOL_GROUPS
                .iter()
                .find(|(g, _)| *g == group)
                .is_some_and(|(_, members)| {
                    !members.iter().any(|t| ceiling.tools.allows_alone(t))
                        && !members.iter().any(|t| ceiling.tools.names_never(t))
                })
    }

    /// A tool's choice for the Bot as its own entries say it, whatever its group's switch: what a
    /// group switched off keeps for each of its tools, and shows (7 Oct 2026).
    pub fn mode_of_alone(
        &self,
        account: &AccountId,
        coworker: &CoworkerId,
        tool: &str,
    ) -> &'static str {
        let off = opengrok_policy::group_of(tool).filter(|group| {
            self.policy
                .ceiling
                .as_ref()
                .is_some_and(|ceiling| ceiling.tools.group_off(group))
        });
        let Some(group) = off else {
            // A group left out entirely keeps no choices of its own: switching it on gives each
            // tool its default, which is what it shows meanwhile.
            if let Some(group) = opengrok_policy::group_of(tool)
                && self.group_off(group)
                && !self
                    .policy
                    .ceiling
                    .as_ref()
                    .is_some_and(|c| c.tools.names_never(tool))
            {
                return if Self::ASK_BY_RULE.contains(&tool) {
                    "ask"
                } else {
                    "always"
                };
            }
            return self.mode_of(account, coworker, tool);
        };
        // The switch is written into both layers (the ceiling and the owner's grant), so both
        // are read as if it were on.
        let mut policy = self.policy.clone();
        if let Some(ceiling) = policy.ceiling.as_mut() {
            ceiling.tools = ceiling.tools.with_group_on(group);
        }
        if let Some(grant) = policy.grant.as_mut() {
            grant.profile = grant.profile.with_group_on(group);
        }
        let decision = opengrok_policy::decide(
            account,
            coworker,
            opengrok_policy::Action::RunTool(tool),
            &policy,
        );
        self.mode_label(&decision, tool)
    }

    /// The tools of a switched-on plugin that the person switched off for this Bot (Never allow):
    /// never offered, kept so a listing can show them and they can be switched back on.
    #[must_use]
    pub fn with_switched_off(mut self, tools: Vec<crate::mcp::McpTool>) -> Self {
        self.switched_off = tools;
        self
    }

    pub fn switched_off(&self) -> &[crate::mcp::McpTool] {
        &self.switched_off
    }

    /// Attach what a dial produced: the sessions, the tools, and the servers that did not answer.
    #[must_use]
    pub fn with_plugins(mut self, dialled: crate::mcp::Dialled) -> Self {
        self.unavailable_plugins = dialled.unavailable;
        self.with_plugin_tools(dialled.sessions, dialled.tools)
    }

    /// One sentence for the system message naming the plugin servers that could not be reached
    /// this request, or nothing.
    ///
    /// SAID UP FRONT BECAUSE THEIR TOOLS ARE NOT OFFERED. Without it a person asking for GitHub
    /// while GitHub's server is down hears that the coworker has no GitHub, which sends them to
    /// the wrong place (#199).
    pub fn unavailable_plugins_line(&self) -> String {
        if self.unavailable_plugins.is_empty() {
            return String::new();
        }
        let reasons = self
            .unavailable_plugins
            .values()
            .map(|reason| clip_reason(reason))
            .collect::<Vec<_>>()
            .join("; ");
        format!(
            " Some of your connected tools are unavailable on this turn, so they are not offered: \
             {reasons}. If asked for one, say it could not be reached right now and may work on a \
             later message — not that you do not have it."
        )
    }

    /// The tools that need no plugin. `open_url` and `computer` are in the default grant but are
    /// OFFERED only when the box has a display (`with_screen`); `run_recipe` only with a display
    /// and at least one granted recipe (`with_recipes`). A grant that omits a name here denies it.
    pub fn builtin_tool_names() -> &'static [&'static str] {
        &[
            "shell",
            "read_file",
            "write_file",
            "open_url",
            "computer",
            REQUEST_USER_FORM,
            RUN_RECIPE,
        ]
    }

    /// The built-ins this executor can actually run right now.
    fn offered_builtins(&self) -> impl Iterator<Item = &'static str> + '_ {
        Self::builtin_tool_names()
            .iter()
            .copied()
            .filter(move |_| !self.boxless)
            .filter(move |name| self.screen || !SCREEN_TOOLS.contains(name))
            .filter(move |name| {
                !self.network_off() || !BROWSER_TOOLS.contains(name) || *name == REQUEST_USER_FORM
            })
            // Offered by `tool_names` / `tool_schemas` on its own terms: a screen AND a grant.
            .filter(|name| *name != RUN_RECIPE)
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
                self.reaches_the_machine()
                    .then(|| USER_MACHINE_SHELL.to_string()),
            )
            .chain(self.routine_tools().map(str::to_string))
            .chain(self.plugin_desk_tools().map(str::to_string))
            .chain(self.computer_desk_tools().map(str::to_string))
            .chain(self.office_desk_tools().map(str::to_string))
            .chain(
                self.plugin_tools
                    .iter()
                    .map(|tool| tool.qualified_name.clone()),
            )
            .collect()
    }

    /// A machine attached, and a CEILING that lets this coworker reach it — its owner's switch
    /// (#268). Not the grant's profile: each command is the local-exec policy's to allow.
    fn reaches_the_machine(&self) -> bool {
        let ceiling = self.policy.ceiling.as_ref().map(|ceiling| &ceiling.tools);
        self.user_machine.is_some() && ceiling.is_some_and(|tools| tools.allows(USER_MACHINE_SHELL))
    }

    /// Every built-in, the person's machine, `message_bot` and the routine tools too: names no
    /// plugin may take, and what a new hire's ceiling holds (#268, #314, #316). The machine and
    /// `message_bot` are not the executor's to run: the server offers each where its ceiling
    /// allows (`reaches_the_machine`, `ToolRunner::with_bots`).
    pub fn every_builtin() -> impl Iterator<Item = &'static str> {
        Self::builtin_tool_names()
            .iter()
            .copied()
            .chain([USER_MACHINE_SHELL, message_bot::MESSAGE_BOT])
            .chain(routine::TOOLS)
            .chain(plugin_desk::TOOLS)
            .chain(computer_desk::TOOLS)
            .chain(office_desk::TOOLS)
    }

    /// What a person reads about a built-in tool or tool group, beside what the model reads
    /// (`builtin_description`, written for the model, in its capitals): a short label and one
    /// sentence. Apps list these; the model is never shown them (#359).
    pub fn builtin_for_people(name: &str) -> Option<(&'static str, &'static str)> {
        Some(match name {
            "shell" => ("Shell", "Runs commands on the Bot's own computer."),
            "read_file" => ("Read file", "Reads a file on the Bot's own computer."),
            "write_file" => (
                "Write file",
                "Creates or changes a file on the Bot's own computer.",
            ),
            "open_url" => ("Open page", "Opens a web page in the Bot's own browser."),
            "computer" => ("Computer", "Looks at the Bot's screen, clicks and types."),
            REQUEST_USER_FORM => ("Ask with a form", "Asks you to fill in a form in chat."),
            RUN_RECIPE => (
                "Run recipe",
                "Runs a task you taught on the Bot's computer.",
            ),
            USER_MACHINE_SHELL => (
                "Your Mac's shell",
                "Runs commands on this Mac, when you allow it.",
            ),
            message_bot::MESSAGE_BOT => {
                ("Message a Bot", "Sends a message to another of your Bots.")
            }
            routine::ROW => (
                routine::ROW_LABEL,
                "Lists, makes, edits, deletes and runs this Bot's routines.",
            ),
            routine::LIST_ROUTINES => ("List routines", "Lists this Bot's routines."),
            routine::CREATE_ROUTINE => (
                "Create routine",
                "Makes a routine that wakes this Bot on a schedule.",
            ),
            routine::UPDATE_ROUTINE => ("Update routine", "Changes one of this Bot's routines."),
            routine::DELETE_ROUTINE => (
                "Delete routine",
                "Deletes one of this Bot's routines for good.",
            ),
            routine::RUN_ROUTINE => ("Run routine", "Runs one of this Bot's routines now."),
            computer_desk::ROW => (
                computer_desk::ROW_LABEL,
                "Checks, starts, stops, restarts, resets and updates the Bot's own computer, and \
                 moves it to a screen of its own.",
            ),
            computer_desk::COMPUTER_STATUS => (
                "Computer status",
                "Reads whether the Bot's computer is running.",
            ),
            computer_desk::START_COMPUTER => ("Start computer", "Starts the Bot's computer."),
            computer_desk::SHUTDOWN_COMPUTER => (
                "Shut down computer",
                "Shuts the Bot's computer down, keeping its files.",
            ),
            computer_desk::RESTART_COMPUTER => ("Restart computer", "Restarts the Bot's computer."),
            computer_desk::USE_OWN_SCREEN => (
                "Use its own screen",
                "Moves the Bot to a screen of its own on a shared computer, or back to the shared one.",
            ),
            computer_desk::RESET_COMPUTER => (
                "Reset computer",
                "Deletes everything on the Bot's computer for good.",
            ),
            computer_desk::UPDATE_COMPUTER => (
                "Update computer",
                "Rebuilds the Bot's computer on the newest image.",
            ),
            computer_desk::SET_NETWORK => (
                "Set network",
                "Sets what the Bot's computer may reach on your network.",
            ),
            office_desk::ROW => (office_desk::ROW_LABEL, office_desk::ROW_DESCRIPTION),
            office_desk::OFFICE_FILES => (
                "Browse documents",
                "Lists the .docx, .xlsx and .pptx files on the Bot's computer.",
            ),
            office_desk::OFFICE_OPEN => (
                "Open document",
                "Opens an Office file and keeps a session you can watch live.",
            ),
            office_desk::OFFICE_CREATE => (
                "Create document",
                "Makes a new .docx, .xlsx or .pptx file on the Bot's computer.",
            ),
            office_desk::OFFICE_OUTLINE => (
                "Document outline",
                "Reads a document's headings, sheets or slides.",
            ),
            office_desk::OFFICE_GREP => ("Search document", "Finds text inside an open document."),
            office_desk::OFFICE_READ => ("Read document", "Reads a span of an open document's text."),
            office_desk::OFFICE_CELLS => ("Read cells", "Reads a range of a spreadsheet's cells."),
            office_desk::OFFICE_RENDER => (
                "Render page",
                "Rasterizes one page, slide or sheet to an image.",
            ),
            office_desk::OFFICE_VERIFY => (
                "Verify document",
                "Saves and reopens a document to prove the file is still valid.",
            ),
            office_desk::OFFICE_CLOSE => ("Close document", "Ends an open document's session."),
            office_desk::OFFICE_PROPOSE => (
                "Propose edits",
                "Stages text edits you can review before they are written.",
            ),
            office_desk::OFFICE_PROPOSE_CELLS => (
                "Propose cell edits",
                "Stages spreadsheet edits you can review before they are written.",
            ),
            office_desk::OFFICE_REVIEW => ("Review proposals", "Shows what a staged edit would change."),
            office_desk::OFFICE_ACCEPT => ("Accept edits", "Writes a reviewed proposal to the document."),
            office_desk::OFFICE_REJECT => ("Reject edits", "Discards a staged proposal without writing."),
            office_desk::OFFICE_EXPORT => (
                "Export document",
                "Saves the document to a new file and attaches it to the reply.",
            ),
            plugin_desk::ROW => (
                plugin_desk::ROW_LABEL,
                "Finds, installs and removes plugins and manages their accounts.",
            ),
            plugin_desk::LIST_PLUGINS => ("List plugins", "Lists the plugins you can use."),
            plugin_desk::PLUGIN_DETAILS => ("Plugin details", "Reads one plugin in full."),
            plugin_desk::INSTALL_PLUGIN => ("Install plugin", "Installs a plugin for you."),
            plugin_desk::UNINSTALL_PLUGIN => (
                "Uninstall plugin",
                "Uninstalls one of your plugins and its accounts.",
            ),
            plugin_desk::LIST_PLUGIN_ACCOUNTS => ("List accounts", "Lists your plugin accounts."),
            plugin_desk::ADD_PLUGIN_ACCOUNT => {
                ("Add account", "Shows you a card to add a plugin account.")
            }
            plugin_desk::RENAME_PLUGIN_ACCOUNT => {
                ("Rename account", "Renames one of your plugin accounts.")
            }
            plugin_desk::REMOVE_PLUGIN_ACCOUNT => (
                "Remove account",
                "Removes one of your plugin accounts for good.",
            ),
            plugin_desk::SET_PLUGIN_FOR_BOT => (
                "Switch plugin for a Bot",
                "Switches a plugin on or off for a Bot.",
            ),
            plugin_desk::PICK_PLUGIN_ACCOUNT => (
                "Pick account for a Bot",
                "Chooses which account a Bot uses for a plugin.",
            ),
            _ => return None,
        })
    }

    /// A built-in's words as `tool_schemas` offers them, less what a turn adds (a recipe list);
    /// and the routines' row's (#316).
    pub fn builtin_description(name: &str) -> Option<&'static str> {
        if name == RUN_RECIPE {
            return Some(RUN_RECIPE_DESCRIPTION);
        }
        if name == message_bot::MESSAGE_BOT {
            return Some(message_bot::MESSAGE_BOT_DESCRIPTION);
        }
        routine::description(name)
            .or(plugin_desk::description(name))
            .or(computer_desk::description(name))
            .or(office_desk::description(name))
            .or(builtin_tool_spec(name).map(|(description, _)| description))
    }

    /// Internal dotted `qualified_name` ↔ OpenAI-safe wire name for this coworker's plugins.
    /// Sorted so two names that sanitise the same way get the same `_2` suffix
    /// regardless of plugin-list order.
    fn plugin_wire_names(&self) -> Vec<(String, String)> {
        let mut qualified: Vec<&str> = self
            .plugin_tools
            .iter()
            .map(|tool| tool.qualified_name.as_str())
            .collect();
        qualified.sort_unstable();
        crate::mcp::openai_unique_tool_names(Self::every_builtin(), qualified)
    }

    /// A plugin tool offered under `wire_name`: its dotted `<plugin>.<server>.<tool>` name, and the
    /// title its server's annotations give it, for a person to read (never for a decision).
    pub fn plugin_tool_named(&self, wire_name: &str) -> Option<(String, Option<String>)> {
        let tool = self.lookup_plugin_tool(wire_name)?;
        let title = tool
            .annotations
            .as_ref()
            .and_then(|notes| notes.get("title"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(str::to_string);
        Some((tool.qualified_name.clone(), title))
    }

    /// Accept the model's OpenAI-safe name or a legacy dotted qualify. Policy, sessions and
    /// `split_qualified` keep the dotted form. Every builtin is already a legal OpenAI name,
    /// so only a plugin tool can arrive under a name other than its own.
    fn internal_tool_name(&self, call_name: &str) -> String {
        if let Some(tool) = self.lookup_plugin_tool(call_name) {
            return tool.qualified_name.clone();
        }
        call_name.to_string()
    }

    fn lookup_plugin_tool(&self, name: &str) -> Option<&crate::mcp::McpTool> {
        self.plugin_tools
            .iter()
            .find(|tool| tool.qualified_name == name)
            .or_else(|| {
                let qualified = self
                    .plugin_wire_names()
                    .into_iter()
                    .find(|(_, wire)| wire == name)
                    .map(|(qualified, _)| qualified)?;
                self.plugin_tools
                    .iter()
                    .find(|tool| tool.qualified_name == qualified)
            })
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
                    "function": { "name": crate::mcp::openai_safe_tool_name(name), "description": description, "parameters": parameters },
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
                .map(|recipe| {
                    let mut entry =
                        format!("`{}` — {}: {}", recipe.id, recipe.name, recipe.description);
                    if !recipe.parameters.is_empty() {
                        let params = recipe
                            .parameters
                            .iter()
                            .map(|p| {
                                format!(
                                    "{} ({}text)",
                                    p.name,
                                    if p.required { "required " } else { "optional " }
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(", ");
                        entry.push_str(&format!("; parameters: {}", params));
                    }
                    entry
                })
                .collect::<Vec<_>>()
                .join("\n");
            let ids: Vec<&str> = self
                .recipes
                .iter()
                .map(|recipe| recipe.id.as_str())
                .collect();

            // Build properties that include parameters for each recipe.
            let mut properties = serde_json::json!({
                "recipe": { "type": "string", "enum": ids, "description": "The recipe's id." }
            });

            // Add a values object property that describes the parameters of the recipes.
            let recipe_params_info = self
                .recipes
                .iter()
                .filter(|r| !r.parameters.is_empty())
                .map(|r| {
                    let param_info = r
                        .parameters
                        .iter()
                        .map(|p| {
                            format!(
                                "{}: {} ({}required)",
                                p.name,
                                format!("{:?}", p.kind).to_lowercase(),
                                if p.required { "" } else { "not " }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("`{}`: {}", r.id, param_info)
                })
                .collect::<Vec<_>>();

            if !recipe_params_info.is_empty() {
                let values_description = format!(
                    "Parameter values for recipes that require them. For recipes with parameters, pass `values` as a map of parameter names to values. Recipes with parameters:\n{}",
                    recipe_params_info.join("\n")
                );
                if let Some(props) = properties.as_object_mut() {
                    props.insert(
                        "values".to_string(),
                        serde_json::json!({
                            "type": "object",
                            "description": values_description
                        }),
                    );
                }
            }

            schemas.push(serde_json::json!({
                "type": "function",
                "function": {
                    "name": crate::mcp::openai_safe_tool_name(RUN_RECIPE),
                    "description": format!("{RUN_RECIPE_DESCRIPTION} Recipes you may run:\n{listing}"),
                    "parameters": {
                        "type": "object",
                        "properties": properties,
                        "required": ["recipe"],
                    },
                },
            }));
        }
        // The reverse-exec tool is NOT gated by the grant's profile: each command is authorized by
        // the account's local-exec policy (never/ask/bypass) and the machine's own consent, inside
        // the sink, and a profile written before the machine existed would deny every coworker a
        // capability the account enabled. Whether this coworker may reach the machine at all is
        // its ceiling's switch (#268), so it is offered when a machine is attached AND that allows.
        if self.reaches_the_machine()
            && let Some((description, parameters)) = builtin_tool_spec(USER_MACHINE_SHELL)
        {
            schemas.push(serde_json::json!({
                "type": "function",
                "function": { "name": crate::mcp::openai_safe_tool_name(USER_MACHINE_SHELL), "description": description, "parameters": parameters },
            }));
        }
        let routines = self.routine_tools().filter(|name| permitted(name));
        schemas.extend(routines.filter_map(routine::schema));
        let plugin_desk = self.plugin_desk_tools().filter(|name| permitted(name));
        schemas.extend(plugin_desk.filter_map(plugin_desk::schema));
        let computer = self.computer_desk_tools().filter(|name| permitted(name));
        schemas.extend(computer.filter_map(computer_desk::schema));
        let office = self.office_desk_tools().filter(|name| permitted(name));
        schemas.extend(office.filter_map(office_desk::schema));
        let plugin_wires = self.plugin_wire_names();
        let mut schema_budget = crate::mcp::MAX_ADVERTISED_SCHEMAS_BYTES;
        for tool in &self.plugin_tools {
            if permitted(&tool.qualified_name) {
                let wire = plugin_wires
                    .iter()
                    .find(|(qualified, _)| qualified == &tool.qualified_name)
                    .map(|(_, wire)| wire.clone())
                    .unwrap_or_else(|| crate::mcp::openai_safe_tool_name(&tool.qualified_name));
                schemas.push(serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": wire,
                        "description": tool.description.clone().unwrap_or_default(),
                        // The server's own schema, cleaned: an open object here had the model
                        // guess argument names, and every wrong guess cost a round (#196).
                        "parameters": crate::mcp::within_budget(tool.parameters(), &mut schema_budget),
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
        let tool_name = self.internal_tool_name(&call.name);
        if self.boxless && Self::builtin_tool_names().contains(&tool_name.as_str()) {
            return ToolResult::refused(&call.id, NO_COMPUTER);
        }
        // grok-box:local has neither binary (checked 22 Sep 2026). A box shell that names
        // them wakes the desktop, misses PATH, and then tries to fetch one. Refuse before
        // the egress card and before the box is started. The user machine is the other tool.
        if tool_name == "shell"
            && let Ok(args) = serde_json::from_value::<ShellArgs>(arguments.clone())
            && command_targets_absent_bir_binary(&args.command)
        {
            return ToolResult::refused(&call.id, BIR_NOT_ON_THE_BOX);
        }
        // A routine tool's arguments, and for a delete the routine it names, are asked BEFORE any
        // gate (#316): a call that could never run is told why now, never after a card.
        let routine = match (routine::is_routine_tool(&tool_name), self.routines.as_ref()) {
            (false, _) => None,
            (true, None) => return ToolResult::refused(&call.id, "routines are not on offer here"),
            (true, Some((desk, listing))) => {
                let call_of = (tool_name.as_str(), &arguments);
                match routine::admit(desk.as_ref(), *listing, context, call_of).await {
                    Ok(admitted) => Some((desk.clone(), admitted)),
                    Err(why) => return ToolResult::refused(&call.id, why),
                }
            }
        };
        // An office call's arguments are read BEFORE any gate the same way — its `document`
        // handle is just a string until the desk answers it.
        let office_call = match (
            office_desk::is_office_tool(&tool_name),
            self.office_desk.as_ref(),
        ) {
            (false, _) => None,
            (true, None) => {
                return ToolResult::refused(&call.id, "office documents are not on offer here");
            }
            (true, Some(desk)) => match office_desk::admit((tool_name.as_str(), &arguments)) {
                Ok(admitted) => Some((desk.clone(), admitted)),
                Err(why) => return ToolResult::refused(&call.id, why),
            },
        };
        // A plugin tool's arguments, and for one that asks first what it acts on, as stored, are
        // asked BEFORE any gate too (#359).
        let computer_call = match (
            computer_desk::is_computer_tool(&tool_name),
            self.computer_desk.as_ref(),
        ) {
            (false, _) => None,
            (true, None) => {
                return ToolResult::refused(&call.id, "this Bot's computer is not on offer here");
            }
            (true, Some(desk)) => {
                let call_of = (tool_name.as_str(), &arguments);
                match computer_desk::admit(desk.as_ref(), context, call_of).await {
                    Ok(admitted) => Some((desk.clone(), admitted)),
                    Err(why) => return ToolResult::refused(&call.id, why),
                }
            }
        };
        let plugin_call = match (
            plugin_desk::is_plugin_desk_tool(&tool_name),
            self.plugin_desk.as_ref(),
        ) {
            (false, _) => None,
            (true, None) => return ToolResult::refused(&call.id, "plugins are not on offer here"),
            (true, Some(desk)) => {
                let call_of = (tool_name.as_str(), &arguments);
                match plugin_desk::admit(desk.as_ref(), context, call_of).await {
                    Ok(admitted) => Some((desk.clone(), admitted)),
                    Err(why) => return ToolResult::refused(&call.id, why),
                }
            }
        };
        // Two different yeses. The gate's approval (the machine owner's or the policy's card)
        // releases the gate's ask AND skips the judge; a review approval skips only the judge.
        let gate_approved = self.approved_calls.contains(&call.id);
        let review_approved = gate_approved || self.review_approved_calls.contains(&call.id);

        // The reverse-exec tool is authorized by the LOCAL-EXEC policy (its sink judges the
        // command against never/ask/bypass + the standing rules), NOT the grant's profile — which
        // would Deny it for any coworker whose profile lists only the box tools — once its ceiling
        // lets it reach the machine at all. It runs on the USER'S machine, so it needs no box.
        let user_machine_command = if tool_name == USER_MACHINE_SHELL {
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
            if !self.reaches_the_machine() {
                return ToolResult::refused(&call.id, MACHINE_SWITCHED_OFF);
            }
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
                opengrok_policy::Action::RunTool(&tool_name),
                &self.policy,
            );
            // A DELETE ALWAYS ASKS (#316), on the policy's card, in words naming the routine as
            // stored: an allow becomes the ask, and a grant's own ask says which routine. So does
            // a plugin tool that cannot be undone or reaches another Bot (#359).
            let delete = routine
                .as_ref()
                .and_then(|(_, (_, card))| card.clone())
                .or_else(|| plugin_call.as_ref().and_then(|(_, (_, card))| card.clone()))
                .or_else(|| {
                    computer_call
                        .as_ref()
                        .and_then(|(_, (_, card))| card.clone())
                });
            // The person's Always allow for such a tool (`+<tool>` in the ceiling, #359) lifts
            // that card; a grant's own ask still stands.
            let delete = delete.filter(|_| !self.asks_unasked(&tool_name));
            if decision.needs_approval() || (decision.is_allowed() && delete.is_some()) {
                let why = decision.reason().unwrap_or("a human yes").to_string();
                Gate::Ask(AwaitingReason::PolicyApproval, delete.unwrap_or(why))
            } else if let Some(reason) = decision.reason() {
                Gate::Deny(reason.to_string())
            } else {
                Gate::Allow
            }
        };

        // The browser tools are withheld when the network is off, so one arriving here is a
        // model that was offered them on an earlier turn. Refused in words BEFORE the two
        // hand-off cards below: a login card raised for a box that cannot browse would have
        // the person type a password that is then thrown away.
        if self.network_off() && BROWSER_TOOLS.contains(&tool_name.as_str()) {
            // A collect card never touches the box. A login card on a machine that
            // cannot browse would take a password and throw it away.
            let collect =
                tool_name == REQUEST_USER_FORM && user_form::is_chat_collection(&call.arguments);
            if !collect {
                return ToolResult::refused(&call.id, NETWORK_OFF);
            }
        }

        // HITL wait, not approve-then-run — and BEFORE the judge. A form is not a tool that
        // then executes, so auto-review must not steal the card; a `computer` type while a
        // form is open must not send the secret to another model. Policy Deny still refuses.
        if tool_name == REQUEST_USER_FORM {
            if let Gate::Deny(why) = &gate {
                return ToolResult::refused(&call.id, why.as_str());
            }
            if context.screen_hold {
                return ToolResult::refused(
                    &call.id,
                    format!(
                        "a form or computer handoff is already open {}; wait for the person to \
                         finish it",
                        context.held_where()
                    ),
                );
            }
            if let Err(why) = user_form::validate_field_ids(&call.arguments) {
                return ToolResult::refused(&call.id, why);
            }
            return ToolResult::awaiting(&call.id, AwaitingReason::UserForm, "Waiting for you");
        }

        if context.screen_hold && matches!(tool_name.as_str(), "computer" | "open_url" | RUN_RECIPE)
        {
            return ToolResult::refused(
                &call.id,
                format!(
                    "a form or computer handoff is open {}; do not type, click, or open pages \
                     until the person has finished. Secrets must not be typed with `computer`",
                    context.held_where()
                ),
            );
        }

        // Prod traffic reroute: host wants the tunnel AND the box reports ready (laptop
        // client attached). Docker host-network is not that. With no standing auto-review
        // allow, leave-box tools raise the Review-an-action card. A primary-gate Ask
        // subsumes this (one card).
        let asks_the_tunnel = needs_egress_consent(&tool_name, &arguments);
        let review_inactive = self
            .auto_review
            .as_ref()
            .is_none_or(|review| !review.policy.is_active());
        // Consent to leave through the tunnel is given once per run, not once per click: "visit
        // facebook" used to cost a card for the page, another for the screenshot, another for the
        // click (21 Sep 2026). The resume paths set it from the answered card's own tool.
        let egress_consented = self.egress_consented();
        if self.egress_tunnel == EgressTunnelMode::On
            && asks_the_tunnel
            && !review_approved
            && !egress_consented
            && review_inactive
        {
            match &gate {
                Gate::Deny(why) => return ToolResult::refused(&call.id, why.clone()),
                Gate::Ask(_, _) => {}
                Gate::Allow if !gate_approved => {
                    return ToolResult::awaiting(
                        &call.id,
                        AwaitingReason::AutoReview,
                        review::EGRESS_TUNNEL_ASK_REASON,
                    );
                }
                Gate::Allow => {}
            }
        }

        // ONE judge call site, for every tool.
        let review = match (&gate, review_approved, self.auto_review.as_ref()) {
            (Gate::Deny(_), _, _) | (_, true, _) | (_, _, None) => None,
            (_, false, Some(review)) if !review.policy.is_active() => None,
            (_, false, Some(review)) => Some(
                review
                    .judge(&tool_name, &call.id, &arguments, &self.judge_failures)
                    .await,
            ),
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
        // The server's own desk, as the context's account: no box is resolved or woken.
        if let Some((desk, (ask, _))) = routine {
            return routine::run(desk.as_ref(), context, &call.id, ask).await;
        }
        if let Some((desk, (ask, _))) = plugin_call {
            return plugin_desk::run(desk.as_ref(), context, &call.id, ask).await;
        }
        if let Some((desk, (ask, _))) = computer_call {
            return computer_desk::run(desk.as_ref(), context, &call.id, ask).await;
        }
        // `office_close` only ends a session row — no box bytes — so it answers with the
        // server's own desks, and a sleeping box can never keep a document stuck open.
        if let Some((desk, office_desk::Ask::Close { document })) = &office_call {
            return office_desk::run_close(desk.as_ref(), context, &call.id, document).await;
        }

        // `machine: "group"` aims the call at the room's shared computer; anything else is the
        // coworker's own box. The model chooses this one, so it is read from the arguments
        // rather than stamped by `overwrite_identity`.
        let on_group = arguments.get("machine").and_then(Value::as_str) == Some("group");
        // A room's computer has one screen, every member's; the Bot's own computer is on the
        // screen the Bot uses (#376).
        let screen = if on_group {
            Screen::Shared
        } else {
            context.screen()
        };
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

        // The box is brought up here, by the first tool that needs it, not before the model was
        // asked: a turn that never touches the box never waits for it.
        if needs_the_box(&tool_name)
            && let Err(down) = self.ensure_awake(box_id.as_str()).await
        {
            return ToolResult::refused(&call.id, down);
        }
        // The box was asleep when the turn started, so the guest could not say whether the
        // tunnel is really there. It is awake now: ask it, once, and raise the card only if it
        // is — or, under a standing `never`, refuse in words: the tools were offered because
        // nobody could know, and the person's answer to a tunnel that IS there is no.
        if self.egress_tunnel == EgressTunnelMode::AskTheBoxAfterWake
            && BROWSER_TOOLS.contains(&tool_name.as_str())
            && self.egress_policy == EgressPolicy::Never
            && self.tunnel_is_there(box_id.as_str()).await
        {
            return ToolResult::refused(&call.id, NETWORK_OFF);
        }
        if self.egress_tunnel == EgressTunnelMode::AskTheBoxAfterWake
            && asks_the_tunnel
            && !review_approved
            && !egress_consented
            && review_inactive
            && !gate_approved
            && self.tunnel_is_there(box_id.as_str()).await
        {
            return ToolResult::awaiting(
                &call.id,
                AwaitingReason::AutoReview,
                review::EGRESS_TUNNEL_ASK_REASON,
            );
        }

        // Every other office verb runs here, box awake: reads, edits and exports are bytes on
        // it. The session row — not a live object — is what `document` names, so a proposal
        // staged last turn is still what `office_accept` is offered now.
        if let Some((desk, ask)) = office_call {
            return office_desk::run(
                desk.as_ref(),
                context,
                self.computer.as_ref(),
                box_id.as_str(),
                &call.id,
                ask,
            )
            .await;
        }

        match tool_name.as_str() {
            RUN_RECIPE => match serde_json::from_value::<RunRecipeArgs>(arguments) {
                Ok(args) => {
                    let values = args.values.unwrap_or_default();
                    self.run_recipe(box_id, &screen, context, &call.id, &args.recipe, &values)
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
                Ok(args) => self.open_url(box_id, &screen, &call.id, args).await,
                Err(error) => ToolResult::refused(&call.id, format!("bad arguments: {error}")),
            },
            "computer" => match serde_json::from_value::<ComputerArgs>(arguments) {
                Ok(args) => self.computer_use(box_id, &screen, &call.id, args).await,
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
    ///
    /// The run also asks the box to say what it saw while it played, and that reading goes into
    /// the result's words. It is the only channel there is: a `ToolResult` carries text and one
    /// image, so a structured observation the model never sees would be a fact recorded for
    /// nobody. `crate::observe` has the level, what it costs, and why none of it is a verdict.
    async fn run_recipe(
        &self,
        box_id: &BoxId,
        screen: &Screen,
        context: &ToolContext,
        call_id: &str,
        recipe_id: &str,
        values: &opengrok_recipes::Values,
    ) -> ToolResult {
        let Some(offer) = self.recipes.iter().find(|recipe| recipe.id == recipe_id) else {
            return ToolResult::refused(
                call_id,
                format!("no recipe `{recipe_id}` is granted to this coworker"),
            );
        };
        // What the person typed beats what the model inferred. The model may have read a value
        // off the sentence, or invented one; a value typed into a named field is the only one
        // that was actually stated. Anything the person left blank, the model may still fill.
        let mut values = values.clone();
        if let Some((chosen, typed)) = &self.chosen_recipe
            && chosen == recipe_id
        {
            for (name, value) in typed {
                values.insert(name.clone(), value.clone());
            }
        }
        let values = &values;

        // Bind parameter values against the recipe's declaration. A refusal from binding is a
        // refusal in words the model can act on.
        if let Err(why) = opengrok_recipes::bind(&offer.parameters, values) {
            return ToolResult::refused(call_id, why);
        }
        let Some(source) = self.recipe_source.as_ref() else {
            return ToolResult::refused(call_id, "recipes are not available on this server");
        };
        if let Err(why) = source.still_granted(recipe_id, &context.coworker_id).await {
            return ToolResult::refused(call_id, why);
        }
        let (version, mut request) = match source.recipe_request(recipe_id, values).await {
            Ok(found) => found,
            Err(why) => return ToolResult::refused(call_id, why),
        };
        // The bot's screen is taken the way the page's Run button takes it (#227), before the box
        // is touched: a chat play on top of a page run was two recipes clicking on one screen.
        let claim = match source
            .claim_run(recipe_id, version, &context.coworker_id)
            .await
        {
            Ok(claim) => claim,
            Err(why) => return ToolResult::refused(call_id, why),
        };
        let claimed = claim.as_ref().map(|claim| claim.id.as_str());
        // ASK THE BOX WHAT IT SAW. A receipt on its own answers "did any step throw", and a model
        // that gets `ok` back for the twenty-fifth identical replay has been told the truth and
        // learnt nothing. The observation is what lets it tell a run that landed where the tape
        // was taped from one that did not. See `crate::observe` for the level and its cost.
        crate::observe::ask(&mut request, self.observe);
        let receipt = match self
            .computer
            .run_recipe(box_id.as_str(), screen, &request)
            .await
        {
            Ok(raw) => RecipeReceipt::from_value(raw),
            // The box may have played some of it before the connection went: a replay would
            // type on top of it. Counted as played, and the model is told to look first.
            Err(error @ BoxError::Interrupted(_)) => {
                return ToolResult::refused(
                    call_id,
                    format!(
                        "{}. The recipe may have played part way: look at the screen before \
                         doing anything else, and do not run it again. This bot's screen stays \
                         reserved for up to a minute while the interrupted run's claim runs out, \
                         so another recipe asked for in that time is refused",
                        describe(error)
                    ),
                )
                .part_way();
            }
            // A claimed row is ended as a failed run that says why, as the page does; left to its
            // lease it would read "interrupted", which is not what happened. An interrupted play
            // (above) is left to lapse, because part way is all that is known of it; the lapse
            // holds the bot for up to RUN_LEASE_MS, and the model is told so.
            Err(error) => {
                let why = describe(error);
                if claimed.is_some() {
                    let failed = RecipeReceipt::from_value(
                        serde_json::json!({ "ok": false, "ran": 0, "error": why }),
                    );
                    let _ = source
                        .record_run(recipe_id, version, &context.coworker_id, &failed, claimed)
                        .await;
                }
                return ToolResult::refused(call_id, why);
            }
        };
        let _ = source
            .record_run(recipe_id, version, &context.coworker_id, &receipt, claimed)
            .await;
        let mut said = if receipt.ok {
            format!(
                "ran recipe \"{}\" (v{version}): {} steps; screenshot of the screen afterwards attached",
                offer.name, receipt.ran
            )
        } else {
            format!(
                "recipe \"{}\" (v{version}) stopped at step {}: {}",
                offer.name,
                receipt.stopped_at.unwrap_or(receipt.ran),
                receipt.error.as_deref().unwrap_or("a step failed")
            )
        };
        // APPENDED, NOT WOVEN IN. A receipt that carries no observation — an old box, or a
        // deployment that asked for none — leaves these words exactly as a model has been reading
        // them since before any of this existed. It goes on the stopped result too: where a tape
        // stopped is most of the story, and what was under it is the rest.
        if let Some(seen) = crate::observe::Seen::read(&receipt.raw) {
            said.push_str(". ");
            said.push_str(&seen.sentence());
        }
        let mut result = if receipt.ok {
            ToolResult::ok(call_id, said)
        } else {
            ToolResult::refused(call_id, said).part_way()
        };
        if let Some(image) = receipt.image.clone() {
            result = result.with_image(image);
        }
        result
    }

    async fn open_url(
        &self,
        box_id: &BoxId,
        screen: &Screen,
        call_id: &str,
        args: OpenUrlArgs,
    ) -> ToolResult {
        if !self.has_screen() {
            return ToolResult::refused(call_id, "this computer has no screen");
        }
        match self
            .computer
            .open_url(box_id.as_str(), screen, &args.url)
            .await
        {
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
    async fn computer_use(
        &self,
        box_id: &BoxId,
        screen: &Screen,
        call_id: &str,
        args: ComputerArgs,
    ) -> ToolResult {
        if !self.has_screen() {
            return ToolResult::refused(call_id, "this computer has no screen");
        }
        let action = match args.into_action() {
            Ok(action) => action,
            Err(why) => return ToolResult::refused(call_id, format!("bad arguments: {why}")),
        };
        let mut said = String::new();
        if let Some(action) = &action {
            if let Err(error) = self.computer.act(box_id.as_str(), screen, action).await {
                return ToolResult::refused(call_id, describe(error));
            }
            said = format!("{}; ", action.describe());
            // The display needs a moment to repaint after input before it is worth looking.
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        }
        match self.computer.screenshot(box_id.as_str(), screen).await {
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
        let name = self.internal_tool_name(name);
        let Some((plugin, server, remote)) = crate::mcp::split_qualified(&name) else {
            // A server that did not answer offered no tools this turn, so the wire name of one the
            // model remembers from an earlier turn resolves to nothing. Refused with the server's
            // reason, not as a tool that never existed.
            let down = self.unavailable_plugins.iter().find(|(key, _)| {
                name.starts_with(&format!("{}_", crate::mcp::openai_safe_tool_name(key)))
            });
            return ToolResult::refused(
                call_id,
                match down {
                    Some((_, reason)) => format!("{name} cannot run now: {}", clip_reason(reason)),
                    None => format!("there is no tool called {name}"),
                },
            );
        };

        let key = format!("{plugin}.{server}");
        let Some(session) = self.sessions.get(&key) else {
            // Named precisely: "no such tool" and "that plugin is not connected right now" send a
            // person to different places.
            let why = self
                .unavailable_plugins
                .get(&key)
                .map(|reason| format!(" ({})", clip_reason(reason)))
                .unwrap_or_default();
            return ToolResult::refused(
                call_id,
                format!("{plugin} is not connected on this run{why}, so {name} cannot run"),
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
        REQUEST_USER_FORM => Some((
            "Ask the person to fill a form in chat. Two uses. \
             A collect form (collect true) is how you ask for fields you do not have yet: a new \
             profile, a missing name, a TIN they must confirm. Put what they already told you \
             on each field as value, so the card is prefilled, and leave value off the fields \
             that are still missing. Mark those required. Wait. The answers come back as \
             Shared fields and nothing is typed into a page. Do not invent a shell command \
             for the form. \
             A sign-in is the other use: an OTP or a field they must type into a page. \
             NativeChat offers the person their saved logins for that site on the card; they \
             confirm with Touch ID, and the values are typed into the page out of your view. \
             Do NOT type passwords, one-time codes, or other secrets with `computer`: that \
             attaches a screenshot of what was typed. Raise this instead and wait. The person \
             fills in chat; the server clicks each field at the position you give (`at`, in \
             your screenshot's pixels) and types there, never showing you the secret. Give \
             `at` for every field you can see. After it settles, screenshot and confirm what \
             the page shows — filling is not login. When the page shows the email and password \
             fields TOGETHER (Facebook, most sites), raise ONE card with both fields and \
             `samePage: true` — do not split them. Only a page that asks for the email alone \
             (Google) gets an email-only card: raise it, observe, and if a password page comes \
             next call this again with a password-only form (new entryId, challengeKind \
             \"password\"). \
             If another in-sandbox challenge appears (an authenticator code, phone \
             verification on the same page), call this again with otp fields, \
             challengeKind \"otp\", the page host as liveHost and the field's `at`; \
             NativeChat offers the person's saved authenticator code for that site on the \
             card. Never re-raise a form that already settled. A passkey prompt: call this with \
             challengeKind \"passkey\", passkeyMode \"use\" (or \"register\" when the site \
             offers to add one and the person asked), no fields, liveHost set; when the result \
             says it is loaded, click the site's passkey button. A captcha, or a page \
             outside this box, is not another password form: the person finishes on the \
             computer (Open the screen). If they dismiss or decline, continue without those \
             credentials and do not loop.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "Short title shown on the card." },
                    "instruction": { "type": "string", "description": "What the person should do." },
                    "collect": {
                        "type": "boolean",
                        "description": "Ask in chat and return the answers. Nothing is typed into a page. Set this when you need fields the person must supply or confirm. Prefill fields they already answered with value."
                    },
                    "passkeyMode": {
                        "type": "string",
                        "description": "With challengeKind passkey: use (sign in with the person's passkey) or register (the site offers to add one)."
                    },
                    "challengeKind": {
                        "type": "string",
                        "description": "Optional hint: password, otp, passkey, captcha, outside_sandbox. passkey takes no fields (with passkeyMode use|register). Captcha/outside-sandbox must not be another password form."
                    },
                    "samePage": {
                        "type": "boolean",
                        "description": "The fields share one page (Facebook's email and password do). Set it on every one-page card. Without it and without positions, only the first field is typed."
                    },
                    "submit": {
                        "type": "boolean",
                        "description": "Press Return after the fill, i.e. press the page's own Log in. Set it on a one-page login card. A single field, and a fully positioned login, Return on their own."
                    },
                    "fields": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string" },
                                "label": { "type": "string" },
                                "type": { "type": "string", "description": "text, email, password, otp, …" },
                                "value": { "type": "string", "description": "Prefill for a collect form. The person can edit it. Never set this on a secret field. Dropped unless collect is true." },
                                "required": { "type": "boolean" },
                                "secret": { "type": "boolean", "description": "Mask this field; password and otp are secret even without this." },
                                "at": {
                                    "type": "object",
                                    "description": "Where this field is on your screenshot, in its pixels: the fill clicks it before typing, so the value lands in this field whatever the page has focused. Give it whenever you can see the field.",
                                    "properties": { "x": { "type": "integer" }, "y": { "type": "integer" } },
                                    "required": ["x", "y"]
                                }
                            },
                            "required": ["id", "label"]
                        }
                    },
                    "domain": { "type": "string" },
                    "liveHost": { "type": "string", "description": "Host currently on the box's screen, when known." }
                },
                "required": ["title", "fields"],
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

/// A remote server's error text, cut to what a prompt can afford: it is the server's words, and a
/// server must not be able to fill the system message with them.
fn clip_reason(reason: &str) -> String {
    const MOST: usize = 240;
    match reason.char_indices().nth(MOST) {
        Some((cut, _)) => format!("{}…", &reason[..cut]),
        None => reason.to_string(),
    }
}

/// A box failure, in words a model can act on.
fn describe(error: BoxError) -> String {
    match error {
        BoxError::NoSuchBox => "that computer no longer exists".to_string(),
        BoxError::Secret(reason) => reason.clone(),
        BoxError::Unreachable(detail) => format!("the computer is unreachable: {detail}"),
        BoxError::Interrupted(detail) => {
            format!("the connection to the computer was lost before it answered: {detail}")
        }
        BoxError::Refused { status, body } => {
            format!("the computer refused the request ({status}): {body}")
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/lib_tests.rs"]
#[allow(clippy::unwrap_used)]
mod tests;
