//! The run aggregate — one turn a coworker takes, and the reason this project exists.
//!
//! NOTHING THAT MATTERS LIVES IN A CLIENT (CLAUDE.md #5). A run is a row before it is a stream:
//! every event the client will see is appended to the log *first*, so closing the tab, losing the
//! network or killing the process loses the connection and never the work. The prior product got
//! this wrong and it is what created this repo (`research/lessons-opensesame.md` §4).
//!
//! What that buys, concretely:
//!   - a client that reconnects replays from the log instead of asking the model again;
//!   - a run interrupted by a restart is `Running` in the log, not silently lost — it can be
//!     resumed or failed deliberately, by something that can see it;
//!   - two clients watching one run see the same thing, because there is one truth.
//!
//! The events here are OURS, not AG-UI's. AG-UI is a rendering protocol and it changes on its own
//! schedule; the log outlives it. `payload` carries the rendered event so a replay is byte-exact,
//! but the aggregate's own vocabulary is what a future reader reasons about.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::coworker::Effort;
use crate::id::{CoworkerId, RunId};
use crate::inference::{SourceKind, TurnSource, Via};
use crate::limits::RunLimits;

/// Where a run got to. A run that is `Running` with no process behind it is the interesting case:
/// it means a restart interrupted it, and something must decide what to do about that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunStatus {
    Running,
    /// Stopped, waiting on a person. NOT a terminal state: the run can still be finished, which is
    /// what makes an approval days later possible.
    AwaitingApproval,
    Finished,
    Failed,
    /// A person changed their mind and stopped it. Terminal, and DELIBERATELY NOT `Failed`: a stop
    /// is somebody pressing a button, not the run going wrong. Flattening the two would make every
    /// later answer to "why did this run fail" name a failure that never happened, and would leave
    /// the one number that matters — how often a coworker actually breaks — unreadable.
    Stopped,
}

impl RunStatus {
    /// Has this run ended for good? A terminal run accepts no further ending: it cannot be
    /// finished, failed or stopped again, whatever arrives late.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Finished | Self::Failed | Self::Stopped)
    }

    /// The word the wire and the projection use. One place, because three readers spelling a
    /// status differently is how a client comes to believe a run is still going.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::AwaitingApproval => "awaiting-approval",
            Self::Finished => "finished",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }

    /// From the stored word. Anything unrecognised reads as `Running`, which is the open reading:
    /// a status we cannot understand must not be mistaken for an ending that lets work be dropped.
    #[must_use]
    pub fn from_stored(word: &str) -> Self {
        match word {
            "awaiting-approval" => Self::AwaitingApproval,
            "finished" => Self::Finished,
            "failed" => Self::Failed,
            "stopped" => Self::Stopped,
            _ => Self::Running,
        }
    }
}

/// Why a run that finished finished, when it was not simply done (#244). A run that reached a
/// cap still ends as `Finished` — the model's last call said what it did — but a person reading
/// the routines pane has to be able to tell "stopped at its limit" from "done".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FinishReason {
    /// A round cap or the wall clock ran out: the run ended on its wrap-up call, or on the
    /// opened target or editor sentence when the cap was reached with one open.
    Budget,
}

impl FinishReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Budget => "budget",
        }
    }

    /// The reason a `RUN_FINISHED` frame names. An unknown word is no reason: a frame is the
    /// harness's own, so a word this build does not know was written by a newer one, and reading
    /// it as `Budget` would be a guess.
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "budget" => Some(Self::Budget),
            _ => None,
        }
    }
}

/// WHY a run is waiting. Two different cards can now come from the same tool — the machine owner's
/// consent for a command, or the auto-review judge's "ask" — so the answer path has to know which
/// question was asked: the wrong verb must not settle the other card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SuspendReason {
    /// The remote-control gate wants the machine owner's consent for this command (the
    /// `local-tool-permission` card). THE DEFAULT ON PURPOSE: every suspension recorded before
    /// reasons existed meant exactly this, so an old row in the append-only log replays unchanged.
    #[default]
    ExecConsent,
    /// The coworker's policy grant marks this tool `needs_approval`.
    PolicyApproval,
    /// The auto-review judge said "ask" (the `auto-review-approval` card).
    AutoReview,
    /// The bot asked the person to fill an in-chat `user-form` (`request_user_form`). Not an
    /// approval of a tool that will then run: submit types into the box outside `computer_use`,
    /// and the tool result is synthesised so a secret never re-enters the executor.
    UserForm,
}

impl SuspendReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ExecConsent => "exec-consent",
            Self::PolicyApproval => "policy-approval",
            Self::AutoReview => "auto-review",
            Self::UserForm => "user-form",
        }
    }

    /// From the wire word; anything unrecognised is the default, which is the closed reading
    /// (an exec-consent card asks the machine owner, the strictest of the kinds).
    pub fn from_stored(word: &str) -> Self {
        match word {
            "policy-approval" => Self::PolicyApproval,
            "auto-review" => Self::AutoReview,
            "user-form" => Self::UserForm,
            _ => Self::ExecConsent,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum RunEvent {
    Started {
        thread_id: String,
        coworker_id: Option<CoworkerId>,
        /// The pin this turn captured. A resume must not reload the coworker and pick up a
        /// pin that moved while we were waiting. Absent on logs written before this field
        /// existed (`#[serde(default)]`); those keep the old behaviour — the current pin.
        #[serde(default)]
        model: Option<String>,
        /// How hard this turn thinks, captured for the reason the pin is: a resume must not
        /// pick up an effort changed while a person answered a card. Absent on logs written
        /// before this field, which read as inherit — exactly what those turns sent.
        #[serde(default)]
        effort: Effort,
        /// Where this turn's model calls go, captured for the reason the pin is: a run parked on
        /// a card carries on where it started, so a request pinned for the gateway never reaches
        /// a person's proxy mid-run, nor the reverse. Absent on logs written before this field,
        /// which read as the gateway — the only place any turn went then.
        #[serde(default)]
        inference_source: SourceKind,
        /// Which way a proxy turn went (#292), captured for the same reason: a turn its person's
        /// Mac was answering carries on at their Mac, never at the loopback, nor the reverse.
        /// Absent on a gateway turn, and on logs written before the relay, whose proxy turns all
        /// went by the loopback.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        inference_via: Option<Via>,
        /// The system message this turn opened with. A resume must not recompose it from a
        /// coworker whose role or title moved while a person was answering an approval card —
        /// the turn would change identity halfway through, at the moment somebody intervened.
        /// Same reasoning as `model` above, and absent on logs written before this field.
        #[serde(default)]
        system: Option<String>,
        /// The skill quoted into `system`, when this turn had one. A later message on the
        /// same thread that sends no `forwardedProps.skill` reads it back. Absent on logs
        /// written before this field (`#[serde(default)]`).
        #[serde(default)]
        skill_id: Option<String>,
        /// The attached skills `system` lists and `use_skill` reads (#270), captured for the
        /// reason `system` is: a resume offers exactly these, so the tool and the list it opened
        /// with agree whatever was attached or detached while it waited. Empty on logs written
        /// before this field, and a resume of one offers none rather than guess from prose.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        offered_skills: Vec<OfferedSkill>,
        /// The person's side of this turn: the AG-UI messages it was asked with that no earlier
        /// turn on the thread already carried, kept exactly as the client sent them so a field we
        /// do not model survives (CLAUDE.md #2). Without it a log is half a conversation — a new
        /// device replays answers with no questions, and a resumed run forgets what it was asked.
        ///
        /// `None` on logs written before this field, and the difference from `Some(vec![])` is
        /// load-bearing: "never journaled" makes a thread fall back to the client's copy of its
        /// history, while "journaled, and nobody spoke" (an MCP ask) does not.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<Vec<Value>>,
        /// What this run may spend, captured at its start: the server's budget narrowed by its
        /// org's ceiling and its routine's own limits (`RunLimits::and`). A resume is held to
        /// these AND to its org's ceiling as it stands then, so a ceiling lowered while the run
        /// waited binds it, and one raised, or a routine edited, cannot widen it. Empty on logs
        /// written before limits existed, which are held to the server's budget and the org's.
        #[serde(default)]
        limits: RunLimits,
        at_ms: i64,
    },
    /// One rendered protocol event, stored verbatim so a replay is byte-exact rather than
    /// re-derived — a re-derivation would drift the moment the projection changed.
    Emitted {
        seq: i64,
        payload: Value,
        at_ms: i64,
    },
    /// A tool call is waiting on a human yes. Records exactly WHICH call, because that is what a
    /// later approval has to be about — "approve the run" would be ambiguous the moment a turn
    /// asks for two things.
    Suspended {
        call_id: String,
        tool: String,
        arguments: Value,
        #[serde(default)]
        reason: SuspendReason,
        at_ms: i64,
    },
    /// A person answered. `approved` false is a refusal, which is also an answer and also ends the
    /// waiting.
    Answered {
        call_id: String,
        approved: bool,
        /// Who said so. A decision nobody is attached to cannot be audited.
        by: String,
        at_ms: i64,
    },
    Finished {
        at_ms: i64,
        /// Absent on logs written before #244, and on every run that was simply done.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<FinishReason>,
    },
    Failed {
        reason: String,
        at_ms: i64,
    },
    /// A person stopped it. Its own event rather than a `Failed` with a special reason, because a
    /// reason string is not a status: everything that counts failures — the routines pane, a
    /// future reliability number, whoever is asked why a coworker keeps breaking — reads the
    /// status and would count this one. `by` is recorded for the same reason `Answered` records
    /// it: a decision nobody is attached to cannot be audited.
    Stopped {
        by: String,
        at_ms: i64,
    },
    /// These calls are about to run, written BEFORE they do (#91). A tool's result is journaled
    /// only when its round ends, so without this a log interrupted mid-tool looks exactly like
    /// one interrupted between rounds — and only the second is safe to resume: a started tool may
    /// already have acted, and the model, not knowing, could run it again.
    ToolStarted {
        tools: Vec<StartedTool>,
        at_ms: i64,
    },
    /// The sweep found the run interrupted and carried it on (#91) instead of failing it. The
    /// generation is what fences the loop it replaced: every journal write carries the generation
    /// its loop started under, and the log refuses one from an older generation, so a loop whose
    /// lease lapsed while it was still alive can neither act again nor end the resumed run.
    Resumed {
        generation: u32,
        reason: String,
        at_ms: i64,
    },
    /// What a round spent (#256): the recipes it played on the box, and which budget it drew
    /// on. A recipe plays at most once per request (#120) and the rounds are the run's, but the
    /// loop kept both in memory only, so a run carried on after a restart forgot them: a recipe
    /// that had typed, posted or bought could play again, on a fresh budget.
    ///
    /// WRITTEN IN THE SAME APPEND AS THE ROUND'S FRAMES, so the `TOOL_CALL_RESULT` that closes a
    /// recipe's call and the record that it played land together: no crash can leave the call
    /// closed and the play forgotten. Log-only, like `ToolStarted`: the frames a client reads say
    /// `ok`, which cannot tell a recipe that stopped part way from one refused before the box.
    Spent {
        recipes: Vec<String>,
        /// `None` for a call that ran outside a round: a card's approved call.
        round: Option<RoundKind>,
        at_ms: i64,
    },
}

/// Which budget a round drew on, as the loop decides it: on the screen when every call was a
/// successful `computer` action, spoken otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RoundKind {
    Spoken,
    OnScreen,
}

/// One round's `Spent`, as the loop hands it to the journal with the round's frames.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoundSpent {
    pub recipes: Vec<String>,
    pub round: Option<RoundKind>,
}

impl RoundSpent {
    /// Nothing to record: no recipe played, and no round counted.
    pub fn is_empty(&self) -> bool {
        self.recipes.is_empty() && self.round.is_none()
    }
}

/// One call about to run, as `ToolStarted` records it. The tool's name rides with its id because
/// the round's `TOOL_CALL_START` frames — the only other place the name is — are journaled only
/// when the round ends; a run interrupted mid-tool must still be able to say which tool it was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartedTool {
    pub call_id: String,
    pub tool: String,
}

impl RunEvent {
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::Started { .. } => "run-started",
            Self::Emitted { .. } => "run-emitted",
            Self::Suspended { .. } => "run-suspended",
            Self::Answered { .. } => "run-answered",
            Self::Finished { .. } => "run-finished",
            Self::Failed { .. } => "run-failed",
            Self::Stopped { .. } => "run-stopped",
            Self::ToolStarted { .. } => "run-tool-started",
            Self::Resumed { .. } => "run-resumed",
            Self::Spent { .. } => "run-spent",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub started: bool,
    pub thread_id: String,
    pub coworker_id: Option<CoworkerId>,
    /// Captured at start. See `RunEvent::Started::model`.
    pub model: Option<String>,
    /// Captured at start, and what a resume sends. See `RunEvent::Started::effort`.
    pub effort: Effort,
    /// Captured at start, and where a resume asks. See `RunEvent::Started::inference_source`.
    pub inference_source: SourceKind,
    /// Captured at start. See `RunEvent::Started::inference_via`.
    pub inference_via: Option<Via>,
    /// Captured at start. See `RunEvent::Started::system`.
    pub system: Option<String>,
    /// Captured at start. See `RunEvent::Started::skill_id`.
    pub skill_id: Option<String>,
    /// Captured at start, and what a resume offers. See `RunEvent::Started::offered_skills`.
    pub offered_skills: Vec<OfferedSkill>,
    /// Captured at start. See `RunEvent::Started::prompt`.
    pub prompt: Option<Vec<Value>>,
    /// Captured at start. See `RunEvent::Started::limits`.
    pub limits: RunLimits,
    pub status: RunStatus,
    /// The rendered events, in order — what a reconnecting client replays.
    pub emitted: Vec<Value>,
    pub failure: Option<String>,
    /// Why a finished run finished, when it was not simply done (#244).
    pub finish_reason: Option<FinishReason>,
    /// Who stopped it, when somebody did. `None` on every other run, which is how a reader tells
    /// "nobody stopped this" from "stopped by somebody we did not write down".
    pub stopped_by: Option<String>,
    /// The call waiting on a person, if any.
    pub pending: Option<PendingApproval>,
    /// Calls that have already been answered.
    ///
    /// THIS IS WHAT MAKES APPROVAL EXACTLY-ONCE. A second yes for the same call is refused by the
    /// aggregate, so a retried request, a double-clicked button and two devices answering together
    /// all converge on one answer instead of running the tool twice.
    pub answered: BTreeSet<String>,
    /// Calls whose start is journaled and whose result is not (#91), call id → tool name.
    /// Non-empty on a run that was interrupted while a tool may have been acting: its outcome is
    /// unknown.
    pub open_tools: std::collections::BTreeMap<String, String>,
    /// The generation the run is in: bumped by every `Resumed`. 0 on a run never resumed.
    pub generation: u32,
    /// An answer whose call has not started (#91): set when a person answers, cleared when the
    /// call's start or its result is journaled.
    pub unstarted_answer: Option<AnsweredCall>,
    /// Recipes this request played (#256), from `Spent`. What a resumed segment must not play
    /// again.
    pub played_recipes: BTreeSet<String>,
    /// Rounds the run spent, `(spoken, on screen)`, from `Spent` (#256).
    pub rounds_spent: (usize, usize),
}

impl Default for Run {
    fn default() -> Self {
        Self {
            started: false,
            thread_id: String::new(),
            coworker_id: None,
            model: None,
            effort: Effort::Inherit,
            inference_source: SourceKind::Gateway,
            inference_via: None,
            system: None,
            skill_id: None,
            offered_skills: Vec::new(),
            prompt: None,
            limits: RunLimits::default(),
            status: RunStatus::Running,
            emitted: Vec::new(),
            failure: None,
            finish_reason: None,
            stopped_by: None,
            pending: None,
            answered: BTreeSet::new(),
            open_tools: std::collections::BTreeMap::new(),
            generation: 0,
            unstarted_answer: None,
            played_recipes: BTreeSet::new(),
            rounds_spent: (0, 0),
        }
    }
}

/// What a person is being asked to approve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingApproval {
    pub call_id: String,
    pub tool: String,
    pub arguments: Value,
    #[serde(default)]
    pub reason: SuspendReason,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunError {
    #[error("that run has already ended")]
    AlreadyEnded,
    #[error("that run has not started")]
    NotStarted,
    #[error("that run is not waiting for an answer")]
    NotAwaiting,
    #[error("that call has already been answered")]
    AlreadyAnswered,
    #[error("that run has already started")]
    AlreadyStarted,
    /// A tool started and its result was never journaled: it may have acted, so the run is not
    /// carried on blind. Names the tools, so the failure can say which.
    #[error("interrupted while {} was running; whether it finished is unknown", .0.join(", "))]
    ToolOutcomeUnknown(Vec<String>),
    #[error("interrupted again after resuming {MAX_RESUMES} times")]
    ResumedTooOften,
    #[error("that run is waiting on a person, not interrupted")]
    WaitingOnAPerson,
}

/// How many times an interrupted run is carried on before the next interruption fails it: a run
/// that keeps taking its process down with it must stop being restarted (#91).
pub const MAX_RESUMES: u32 = 2;

/// What a routine's run is journaled as asked: its instruction, as a person's message would be,
/// under the run's own id, so it is unique and says where it came from (`Run::fired_by_routine`).
pub fn routine_prompt(run_id: &RunId, instruction: &str) -> Vec<Value> {
    let id = routine_prompt_id(run_id);
    vec![serde_json::json!({"id": id, "role": "user", "content": instruction})]
}

fn routine_prompt_id(run_id: &RunId) -> String {
    format!("{}-prompt", run_id.as_str())
}

/// A skill a turn offered `use_skill` for (#270): the id it is read by, and the name the turn's
/// system message lists it under, which is the name a resume offers it by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferedSkill {
    pub id: String,
    pub name: String,
}

/// A person's answer whose call has not started yet: what a resume after a crash between the two
/// must carry out, rather than have the model ask again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnsweredCall {
    pub call: PendingApproval,
    pub approved: bool,
}

#[derive(Debug, Clone)]
pub enum RunCommand {
    Start {
        thread_id: String,
        coworker_id: Option<CoworkerId>,
        model: Option<String>,
        /// See `RunEvent::Started::effort`.
        effort: Effort,
        /// See `RunEvent::Started::inference_source` and `inference_via`: the via is kept only
        /// on the proxy, where it says something.
        inference_source: TurnSource,
        /// The composed system message this turn opens with, captured so a resume speaks with
        /// the same identity and standing role the turn began with.
        system: Option<String>,
        /// See `RunEvent::Started::skill_id`.
        skill_id: Option<String>,
        /// See `RunEvent::Started::offered_skills`.
        offered_skills: Vec<OfferedSkill>,
        /// See `RunEvent::Started::prompt`.
        prompt: Option<Vec<Value>>,
        /// See `RunEvent::Started::limits`.
        limits: RunLimits,
        at_ms: i64,
    },
    Emit {
        payload: Value,
        at_ms: i64,
    },
    Finish {
        at_ms: i64,
        reason: Option<FinishReason>,
    },
    Fail {
        reason: String,
        at_ms: i64,
    },
    /// These calls are about to run. Refused on a run that has ended, so the write that records
    /// the start is also the question "may they".
    StartTools {
        tools: Vec<StartedTool>,
        at_ms: i64,
    },
    /// What a round spent (#256). Refused on a run that has ended, as a start is.
    RecordSpent {
        spent: RoundSpent,
        at_ms: i64,
    },
    /// Carry an interrupted run on in its next generation (#91). Refused when a tool may have
    /// acted, after `MAX_RESUMES`, and on a run that is not running.
    Resume {
        reason: String,
        at_ms: i64,
    },
    /// Stop and wait for a person.
    Suspend {
        call_id: String,
        tool: String,
        arguments: Value,
        reason: SuspendReason,
        at_ms: i64,
    },
    /// A person changed their mind. Ends the run without calling it a failure.
    Stop {
        /// Who pressed the button.
        by: String,
        at_ms: i64,
    },
    /// A person answered. Refused if that call was already answered — the exactly-once check.
    Answer {
        call_id: String,
        approved: bool,
        by: String,
        at_ms: i64,
    },
}

impl Run {
    pub fn replay<'a>(events: impl IntoIterator<Item = &'a RunEvent>) -> Self {
        let mut state = Self::default();
        for event in events {
            state.apply(event);
        }
        state
    }

    pub fn apply(&mut self, event: &RunEvent) {
        match event {
            RunEvent::Started {
                thread_id,
                coworker_id,
                model,
                effort,
                inference_source,
                inference_via,
                system,
                skill_id,
                offered_skills,
                prompt,
                limits,
                ..
            } => {
                self.started = true;
                self.thread_id = thread_id.clone();
                self.coworker_id = coworker_id.clone();
                self.model = model.clone();
                self.effort = *effort;
                self.inference_source = *inference_source;
                self.inference_via = *inference_via;
                self.system.clone_from(system);
                self.skill_id.clone_from(skill_id);
                self.offered_skills.clone_from(offered_skills);
                self.prompt.clone_from(prompt);
                self.limits = *limits;
                self.status = RunStatus::Running;
            }
            RunEvent::Emitted { payload, .. } => {
                // A journaled result closes its call: whatever the tool did is on record now.
                if payload.get("type").and_then(Value::as_str) == Some("TOOL_CALL_RESULT")
                    && let Some(call) = payload.get("toolCallId").and_then(Value::as_str)
                {
                    self.open_tools.remove(call);
                    if self
                        .unstarted_answer
                        .as_ref()
                        .is_some_and(|answered| answered.call.call_id == call)
                    {
                        self.unstarted_answer = None;
                    }
                }
                self.emitted.push(payload.clone());
            }
            RunEvent::ToolStarted { tools, .. } => {
                if self.unstarted_answer.as_ref().is_some_and(|answered| {
                    tools
                        .iter()
                        .any(|started| started.call_id == answered.call.call_id)
                }) {
                    self.unstarted_answer = None;
                }
                self.open_tools.extend(
                    tools
                        .iter()
                        .map(|started| (started.call_id.clone(), started.tool.clone())),
                );
            }
            RunEvent::Spent { recipes, round, .. } => {
                self.played_recipes.extend(recipes.iter().cloned());
                match round {
                    Some(RoundKind::Spoken) => self.rounds_spent.0 += 1,
                    Some(RoundKind::OnScreen) => self.rounds_spent.1 += 1,
                    None => {}
                }
            }
            RunEvent::Resumed { generation, .. } => self.generation = *generation,
            RunEvent::Suspended {
                call_id,
                tool,
                arguments,
                reason,
                ..
            } => {
                self.status = RunStatus::AwaitingApproval;
                self.pending = Some(PendingApproval {
                    call_id: call_id.clone(),
                    tool: tool.clone(),
                    arguments: arguments.clone(),
                    reason: *reason,
                });
            }
            RunEvent::Answered {
                call_id, approved, ..
            } => {
                self.answered.insert(call_id.clone());
                self.unstarted_answer = self
                    .pending
                    .take()
                    .filter(|pending| pending.call_id == *call_id)
                    .map(|call| AnsweredCall {
                        call,
                        approved: *approved,
                    });
                // Back to running whether the answer was yes OR no, and the sameness is deliberate:
                // a refusal still has to be delivered to the model so it can choose something else,
                // and delivering it is a turn. `approved` decides what the model is told, not
                // whether the run continues.
                self.status = RunStatus::Running;
            }
            RunEvent::Finished { reason, .. } => {
                self.status = RunStatus::Finished;
                self.finish_reason = *reason;
            }
            RunEvent::Failed { reason, .. } => {
                self.status = RunStatus::Failed;
                self.failure = Some(reason.clone());
            }
            RunEvent::Stopped { by, .. } => {
                self.status = RunStatus::Stopped;
                self.stopped_by = Some(by.clone());
                // The card is moot. A person who stopped the run is not going to answer the
                // question it was waiting on, and leaving it pending would keep the run in the
                // approvals list of somebody who has already said no to the whole thing.
                self.pending = None;
            }
        }
    }

    /// The sequence the next emitted event will carry.
    pub fn next_seq(&self) -> i64 {
        self.emitted.len() as i64
    }

    /// The system message a resume must speak with. A captured one wins; a log written before
    /// this was stored has none, and the caller composes a fresh one — the old behaviour.
    #[must_use]
    pub fn system_for_resume(&self) -> Option<String> {
        self.system.clone().filter(|text| !text.is_empty())
    }

    /// Where a resume asks: where the run started, its via included. A proxy run logged before
    /// the relay went by the loopback, the only way there was, and is never read as the account's
    /// default by now — which may be the Mac.
    pub fn source_for_resume(&self) -> TurnSource {
        let proxy = self.inference_source == SourceKind::LocalProxy;
        TurnSource {
            kind: self.inference_source,
            via: proxy.then(|| self.inference_via.unwrap_or_default()),
        }
    }

    /// The pin a resume must think with. A captured start pin wins; a log written before
    /// pins were stored falls back to the coworker's current one (the old behaviour).
    pub fn pin_for_resume(&self, current: &str) -> String {
        self.model
            .as_deref()
            .filter(|pin| !pin.is_empty())
            .unwrap_or(current)
            .to_string()
    }

    /// The text of the run's LAST assistant message. A routine that stopped on a card and carried on
    /// said something before the card ("I need you to sign in") and its answer after it; the answer
    /// is what the person needs, and a prompt the journal replays as a user message is never it.
    pub fn last_answer(&self) -> String {
        let field =
            |frame: &Value, key: &str| frame.get(key).and_then(Value::as_str).map(str::to_string);
        let mut from_the_person = std::collections::HashSet::new();
        let mut current: Option<String> = None;
        let mut text = String::new();
        for frame in &self.emitted {
            match field(frame, "type").as_deref() {
                Some("TEXT_MESSAGE_START") if field(frame, "role").as_deref() == Some("user") => {
                    if let Some(id) = field(frame, "messageId") {
                        from_the_person.insert(id);
                    }
                }
                Some("TEXT_MESSAGE_CONTENT") => {
                    let id = field(frame, "messageId");
                    if id.as_ref().is_some_and(|id| from_the_person.contains(id)) {
                        continue;
                    }
                    if id.is_some() && id != current {
                        text.clear();
                        current = id;
                    }
                    if let Some(delta) = field(frame, "delta") {
                        text.push_str(&delta);
                    }
                }
                _ => {}
            }
        }
        text
    }

    /// Whether a routine started this run (opengrok-server `autonomy::fire`), or a Bot's message
    /// did (#314): its one question is journaled under the run's own id by `routine_prompt`, the
    /// only writer of that id. A client names its own messages, so a turn could carry it only on
    /// a run of its own, and there it only narrows where the run carries on (#304).
    pub fn fired_by_routine(&self, run_id: &RunId) -> bool {
        let id = routine_prompt_id(run_id);
        matches!(self.prompt.as_deref(), Some([asked]) if asked["id"] == id.as_str())
    }

    pub fn decide(&self, command: RunCommand) -> Result<Vec<RunEvent>, RunError> {
        match command {
            // A RUN STARTS ONCE, AND NEVER AFTER IT ENDED. `Started` puts the status back to
            // `Running`, so a second one, or one after an ending (an ending may come first: a
            // run can be failed or stopped before its Start is written), would reopen the run,
            // and its next Finish would be a second ending in the log: what `ExactlyOneEnding`
            // (formal/tla) and `Ending.at_most_one_terminal` (formal/lean) say cannot happen.
            // The store's sequence check already refused both, since a Start is only appended at
            // seq 0; the aggregate now says so itself. tests/run_properties.rs found both.
            RunCommand::Start { .. } if self.started || self.status.is_terminal() => {
                Err(RunError::AlreadyStarted)
            }
            RunCommand::Start {
                thread_id,
                coworker_id,
                model,
                effort,
                inference_source,
                system,
                skill_id,
                offered_skills,
                prompt,
                limits,
                at_ms,
            } => Ok(vec![RunEvent::Started {
                thread_id,
                coworker_id,
                model,
                effort,
                inference_source: inference_source.kind,
                inference_via: inference_source
                    .via
                    .filter(|_| inference_source.kind == SourceKind::LocalProxy),
                system,
                skill_id,
                offered_skills,
                prompt,
                limits,
                at_ms,
            }]),

            RunCommand::Emit { payload, at_ms } => {
                if !self.started {
                    return Err(RunError::NotStarted);
                }
                // Appending to a finished run would let a late frame arrive after the ending a
                // client already acted on. A SUSPENDED run may still be appended to — that is the
                // whole point of suspending rather than ending.
                //
                // A STOPPED RUN MAY STILL BE APPENDED TO, and the asymmetry with `Finished` is
                // deliberate. `Finished` and `Failed` are the run declaring its OWN ending: by the
                // time either is in the log, nothing is still producing frames. A stop is declared
                // from outside, by a person, while the turn is mid-step — and the turn is journaled
                // a whole round at a time, so the round it was in the middle of arrives *after* the
                // stop lands. Refusing it would drop exactly the frames the person was looking at
                // when they pressed the button. What a stopped run refuses is another ENDING, which
                // is what makes it terminal.
                if matches!(self.status, RunStatus::Finished | RunStatus::Failed) {
                    return Err(RunError::AlreadyEnded);
                }
                Ok(vec![RunEvent::Emitted {
                    seq: self.next_seq(),
                    payload,
                    at_ms,
                }])
            }

            RunCommand::Finish { at_ms, reason } => {
                if self.status.is_terminal() {
                    return Err(RunError::AlreadyEnded);
                }
                Ok(vec![RunEvent::Finished { at_ms, reason }])
            }

            RunCommand::Fail { reason, at_ms } => {
                if self.status.is_terminal() {
                    return Err(RunError::AlreadyEnded);
                }
                Ok(vec![RunEvent::Failed { reason, at_ms }])
            }

            RunCommand::RecordSpent { spent, at_ms } => {
                if !self.started {
                    return Err(RunError::NotStarted);
                }
                if self.status.is_terminal() {
                    return Err(RunError::AlreadyEnded);
                }
                Ok(vec![RunEvent::Spent {
                    recipes: spent.recipes,
                    round: spent.round,
                    at_ms,
                }])
            }

            RunCommand::StartTools { tools, at_ms } => {
                if !self.started {
                    return Err(RunError::NotStarted);
                }
                if self.status.is_terminal() {
                    return Err(RunError::AlreadyEnded);
                }
                Ok(vec![RunEvent::ToolStarted { tools, at_ms }])
            }

            RunCommand::Resume { reason, at_ms } => {
                if !self.started {
                    return Err(RunError::NotStarted);
                }
                if self.status.is_terminal() {
                    return Err(RunError::AlreadyEnded);
                }
                if self.status == RunStatus::AwaitingApproval {
                    return Err(RunError::WaitingOnAPerson);
                }
                if !self.open_tools.is_empty() {
                    return Err(RunError::ToolOutcomeUnknown(
                        self.open_tools.values().cloned().collect(),
                    ));
                }
                if self.generation >= MAX_RESUMES {
                    return Err(RunError::ResumedTooOften);
                }
                Ok(vec![RunEvent::Resumed {
                    generation: self.generation + 1,
                    reason,
                    at_ms,
                }])
            }

            // THE PERSON PRESSED THE BUTTON; WHETHER THEY WON THE RACE WITH THE MODEL IS NOT THEIR
            // PROBLEM. `AlreadyEnded` on a run that has already ended is what the caller turns into
            // a success, so stopping twice, stopping a run that finished a moment ago, and stopping
            // one a restart failed all mean the same thing to whoever asked: it is not running.
            RunCommand::Stop { by, at_ms } => {
                if self.status.is_terminal() {
                    return Err(RunError::AlreadyEnded);
                }
                Ok(vec![RunEvent::Stopped { by, at_ms }])
            }

            RunCommand::Suspend {
                call_id,
                tool,
                arguments,
                reason,
                at_ms,
            } => {
                // A stopped run counts here too: a run nobody is going to carry on must not open a
                // card asking somebody to decide whether to carry it on.
                if self.status.is_terminal() {
                    return Err(RunError::AlreadyEnded);
                }
                Ok(vec![RunEvent::Suspended {
                    call_id,
                    tool,
                    arguments,
                    reason,
                    at_ms,
                }])
            }

            RunCommand::Answer {
                call_id,
                approved,
                by,
                at_ms,
            } => {
                // EXACTLY ONCE. A retried request, a double-clicked button, two devices answering
                // together — all land here, and only the first produces an event. The store's
                // sequence check makes the concurrent case safe too: the loser gets Conflict and
                // re-reads to find the call already answered.
                if self.answered.contains(&call_id) {
                    return Err(RunError::AlreadyAnswered);
                }
                // An ended run takes no answer. A Stop clears the card, but a run failed or
                // finished while parked (a hold that timed out, a close that wrote the ending)
                // kept it pending, and `Answered` sets the status back to `Running`: a late
                // answer revived a run the log had ended. tests/run_properties.rs found it.
                // `NotAwaiting`, as a stopped run has always answered: an ended run is not.
                if self.status.is_terminal() {
                    return Err(RunError::NotAwaiting);
                }
                let Some(pending) = &self.pending else {
                    return Err(RunError::NotAwaiting);
                };
                if pending.call_id != call_id {
                    return Err(RunError::NotAwaiting);
                }
                Ok(vec![RunEvent::Answered {
                    call_id,
                    approved,
                    by,
                    at_ms,
                }])
            }
        }
    }
}

/// The read model: what a client asking "what happened in this run" is answered from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunView {
    pub id: RunId,
    pub thread_id: String,
    pub status: RunStatus,
    pub event_count: i64,
    pub updated_at_ms: i64,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn started() -> Run {
        let mut run = Run::default();
        for event in run
            .decide(RunCommand::Start {
                thread_id: "t1".to_string(),
                coworker_id: None,
                model: None,
                effort: Effort::Inherit,
                inference_source: Default::default(),
                system: None,
                skill_id: None,
                offered_skills: Vec::new(),
                prompt: None,
                limits: Default::default(),
                at_ms: 1,
            })
            .unwrap()
        {
            run.apply(&event);
        }
        run
    }

    /// #91: THE LOG CAN TELL A CRASH MID-TOOL FROM ONE BETWEEN ROUNDS. A tool's start is
    /// journaled before it runs, and its result clears it: a run with an open tool was
    /// interrupted while something may have acted, and must not be resumed blind.
    #[test]
    fn a_started_tool_stays_open_until_its_result_is_journaled() {
        let started_tool = |call_id: &str, tool: &str| StartedTool {
            call_id: call_id.to_string(),
            tool: tool.to_string(),
        };
        let mut run = started();
        for event in run
            .decide(RunCommand::StartTools {
                tools: vec![
                    started_tool("call_a", "shell"),
                    started_tool("call_b", "computer"),
                ],
                at_ms: 2,
            })
            .unwrap()
        {
            run.apply(&event);
        }
        assert_eq!(
            run.open_tools.get("call_a").map(String::as_str),
            Some("shell")
        );
        assert_eq!(
            run.open_tools.get("call_b").map(String::as_str),
            Some("computer"),
            "the log can name the tool an interruption caught, not only its id"
        );
        for event in run
            .decide(RunCommand::Emit {
                payload: json!({ "type": "TOOL_CALL_RESULT", "toolCallId": "call_a", "content": "ok" }),
                at_ms: 3,
            })
            .unwrap()
        {
            run.apply(&event);
        }
        assert_eq!(
            run.open_tools
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["call_b"],
            "a result closes its own call and no other"
        );
    }

    /// Starting a tool on a run that has ended is refused: the write that says "a tool is
    /// starting" is also the question "may it", and an ended run's answer is no.
    #[test]
    fn a_tool_cannot_start_on_an_ended_run() {
        let one = || {
            vec![StartedTool {
                call_id: "call_a".to_string(),
                tool: "shell".to_string(),
            }]
        };
        let mut run = started();
        for event in run
            .decide(RunCommand::Fail {
                reason: "swept".to_string(),
                at_ms: 2,
            })
            .unwrap()
        {
            run.apply(&event);
        }
        assert!(matches!(
            run.decide(RunCommand::StartTools {
                tools: one(),
                at_ms: 3
            }),
            Err(RunError::AlreadyEnded)
        ));
        assert!(matches!(
            Run::default().decide(RunCommand::StartTools {
                tools: one(),
                at_ms: 3
            }),
            Err(RunError::NotStarted)
        ));
    }

    /// The event is its own type, so a log written before it existed replays unchanged and a new
    /// one reads back.
    #[test]
    fn a_tool_started_event_round_trips() {
        let event = RunEvent::ToolStarted {
            tools: vec![StartedTool {
                call_id: "call_a".to_string(),
                tool: "shell".to_string(),
            }],
            at_ms: 2,
        };
        assert_eq!(event.event_type(), "run-tool-started");
        let text = serde_json::to_string(&event).unwrap();
        assert_eq!(serde_json::from_str::<RunEvent>(&text).unwrap(), event);
    }

    fn start_shell(run: &mut Run, call_id: &str) {
        apply_all(
            run,
            RunCommand::StartTools {
                tools: vec![StartedTool {
                    call_id: call_id.to_string(),
                    tool: "shell".to_string(),
                }],
                at_ms: 2,
            },
        );
    }

    /// #91: A RESUME MOVES THE RUN INTO ITS NEXT GENERATION, and a loop of an older one is fenced
    /// off by that number. It stays running: a resume is the run carrying on, not a new run.
    #[test]
    fn a_resume_moves_the_run_into_its_next_generation() {
        let mut run = started();
        assert_eq!(run.generation, 0);
        apply_all(
            &mut run,
            RunCommand::Resume {
                reason: "restart".to_string(),
                at_ms: 3,
            },
        );
        assert_eq!(run.generation, 1);
        assert_eq!(run.status, RunStatus::Running);
    }

    /// A tool started and never answered may have acted: resuming would let the model run it
    /// again, so the aggregate refuses and names the tool.
    #[test]
    fn a_run_with_a_tool_open_is_not_resumed() {
        let mut run = started();
        start_shell(&mut run, "call_a");
        let refused = run.decide(RunCommand::Resume {
            reason: "restart".to_string(),
            at_ms: 3,
        });
        assert!(
            matches!(&refused, Err(RunError::ToolOutcomeUnknown(tools)) if tools == &vec!["shell".to_string()]),
            "expected the open tool to refuse the resume: {refused:?}"
        );
    }

    /// A run that keeps being interrupted stops being resumed: twice, then no more.
    #[test]
    fn a_run_is_resumed_at_most_twice() {
        let mut run = started();
        for _ in 0..MAX_RESUMES {
            apply_all(
                &mut run,
                RunCommand::Resume {
                    reason: "restart".to_string(),
                    at_ms: 3,
                },
            );
        }
        assert!(matches!(
            run.decide(RunCommand::Resume {
                reason: "restart".to_string(),
                at_ms: 4
            }),
            Err(RunError::ResumedTooOften)
        ));
    }

    /// Only a running run is resumed: an ended one stays ended, and a parked one is waiting on a
    /// person, which is not an interruption.
    #[test]
    fn an_ended_or_parked_run_is_not_resumed() {
        let mut failed = started();
        apply_all(
            &mut failed,
            RunCommand::Fail {
                reason: "x".to_string(),
                at_ms: 2,
            },
        );
        assert!(matches!(
            failed.decide(RunCommand::Resume {
                reason: "r".to_string(),
                at_ms: 3
            }),
            Err(RunError::AlreadyEnded)
        ));
        let mut parked = started();
        apply_all(
            &mut parked,
            RunCommand::Suspend {
                call_id: "call_a".to_string(),
                tool: "shell".to_string(),
                arguments: json!({"command": "ls"}),
                reason: SuspendReason::default(),
                at_ms: 2,
            },
        );
        assert!(matches!(
            parked.decide(RunCommand::Resume {
                reason: "r".to_string(),
                at_ms: 3
            }),
            Err(RunError::WaitingOnAPerson)
        ));
    }

    /// AN ANSWER IS KEPT UNTIL ITS CALL STARTS. A crash between the two leaves a run whose resume
    /// must run the call the person approved — resumed as a fresh round, the model would ask
    /// again and the person get a second card.
    fn spend(run: &mut Run, recipes: &[&str], round: Option<RoundKind>) {
        let spent = RoundSpent {
            recipes: recipes.iter().map(|recipe| recipe.to_string()).collect(),
            round,
        };
        for event in run
            .decide(RunCommand::RecordSpent { spent, at_ms: 2 })
            .unwrap()
        {
            run.apply(&event);
        }
    }

    #[test]
    fn what_a_run_spent_survives_a_reload() {
        let mut run = started();
        spend(&mut run, &["search-youtube"], Some(RoundKind::Spoken));
        spend(&mut run, &[], Some(RoundKind::OnScreen));
        spend(&mut run, &[], Some(RoundKind::OnScreen));
        spend(&mut run, &["post-reply"], None);
        let expected: BTreeSet<String> = ["post-reply", "search-youtube"]
            .into_iter()
            .map(str::to_string)
            .collect();
        assert_eq!(run.played_recipes, expected);
        assert_eq!(
            run.rounds_spent,
            (1, 2),
            "an approved call outside a round counts no round"
        );
        let event = RunEvent::Spent {
            recipes: vec!["search-youtube".to_string()],
            round: Some(RoundKind::OnScreen),
            at_ms: 2,
        };
        assert_eq!(event.event_type(), "run-spent");
        let stored = serde_json::to_value(&event).unwrap();
        assert_eq!(stored["round"], "on-screen");
        let back: RunEvent = serde_json::from_value(stored).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn an_ended_run_records_nothing_spent() {
        let mut run = started();
        for event in run
            .decide(RunCommand::Finish {
                at_ms: 2,
                reason: None,
            })
            .unwrap()
        {
            run.apply(&event);
        }
        assert_eq!(
            run.decide(RunCommand::RecordSpent {
                spent: RoundSpent {
                    recipes: vec!["x".to_string()],
                    round: None
                },
                at_ms: 3
            }),
            Err(RunError::AlreadyEnded)
        );
    }

    #[test]
    fn an_answered_call_is_kept_until_it_starts() {
        let mut run = started();
        apply_all(
            &mut run,
            RunCommand::Suspend {
                call_id: "call_a".to_string(),
                tool: "shell".to_string(),
                arguments: json!({"command": "ls"}),
                reason: SuspendReason::default(),
                at_ms: 2,
            },
        );
        apply_all(
            &mut run,
            RunCommand::Answer {
                call_id: "call_a".to_string(),
                approved: true,
                by: "ada".to_string(),
                at_ms: 3,
            },
        );
        let answered = run.unstarted_answer.clone().unwrap();
        assert_eq!(answered.call.call_id, "call_a");
        assert!(answered.approved);
        start_shell(&mut run, "call_a");
        assert!(run.unstarted_answer.is_none(), "its start settles it");

        // A refusal is delivered as a result, never started: the result settles it.
        let mut refused = started();
        apply_all(
            &mut refused,
            RunCommand::Suspend {
                call_id: "call_b".to_string(),
                tool: "shell".to_string(),
                arguments: json!({"command": "rm"}),
                reason: SuspendReason::default(),
                at_ms: 2,
            },
        );
        apply_all(
            &mut refused,
            RunCommand::Answer {
                call_id: "call_b".to_string(),
                approved: false,
                by: "ada".to_string(),
                at_ms: 3,
            },
        );
        assert!(
            refused
                .unstarted_answer
                .as_ref()
                .is_some_and(|a| !a.approved)
        );
        apply_all(
            &mut refused,
            RunCommand::Emit {
                payload: json!({ "type": "TOOL_CALL_RESULT", "toolCallId": "call_b", "content": "refused" }),
                at_ms: 4,
            },
        );
        assert!(refused.unstarted_answer.is_none());
    }

    /// The turn's identity survives the wait. A role edited while a person answered an
    /// approval card must not change the coworker halfway through the turn it interrupted.
    #[test]
    #[allow(clippy::expect_used)]
    fn a_resume_speaks_with_the_system_message_the_turn_opened_with() {
        let mut run = Run::default();
        for event in run
            .decide(RunCommand::Start {
                thread_id: "t1".to_string(),
                coworker_id: None,
                model: Some("openai/gpt-5.6-luna".to_string()),
                effort: Effort::Inherit,
                inference_source: Default::default(),
                system: Some("You are Ada.".to_string()),
                skill_id: None,
                offered_skills: Vec::new(),
                prompt: None,
                limits: Default::default(),
                at_ms: 1,
            })
            .expect("start")
        {
            run.apply(&event);
        }
        assert_eq!(run.system_for_resume().as_deref(), Some("You are Ada."));
        // A log written before this was captured has none, and the caller composes afresh.
        let mut old = Run::default();
        for event in old
            .decide(RunCommand::Start {
                thread_id: "t1".to_string(),
                coworker_id: None,
                model: None,
                effort: Effort::Inherit,
                inference_source: Default::default(),
                system: None,
                skill_id: None,
                offered_skills: Vec::new(),
                prompt: None,
                limits: Default::default(),
                at_ms: 1,
            })
            .expect("start")
        {
            old.apply(&event);
        }
        assert_eq!(old.system_for_resume(), None);
        // An empty capture is treated as none rather than as an empty prompt.
        let mut blank = Run::default();
        for event in blank
            .decide(RunCommand::Start {
                thread_id: "t1".to_string(),
                coworker_id: None,
                model: None,
                effort: Effort::Inherit,
                inference_source: Default::default(),
                system: Some(String::new()),
                skill_id: None,
                offered_skills: Vec::new(),
                prompt: None,
                limits: Default::default(),
                at_ms: 1,
            })
            .expect("start")
        {
            blank.apply(&event);
        }
        assert_eq!(blank.system_for_resume(), None);
    }

    #[test]
    fn emitted_events_are_numbered_in_order() {
        let mut run = started();
        for index in 0..3 {
            let events = run
                .decide(RunCommand::Emit {
                    payload: json!({ "n": index }),
                    at_ms: 10,
                })
                .unwrap();
            assert!(matches!(events[0], RunEvent::Emitted { seq, .. } if seq == index));
            for event in &events {
                run.apply(event);
            }
        }
        assert_eq!(run.emitted.len(), 3);
    }

    /// The replay guarantee: what a reconnecting client sees is exactly what was sent.
    #[test]
    fn replaying_the_log_reproduces_every_emitted_event_in_order() {
        let mut run = started();
        let mut log = vec![RunEvent::Started {
            thread_id: "t1".to_string(),
            coworker_id: None,
            model: None,
            effort: Effort::Inherit,
            inference_source: Default::default(),
            inference_via: None,
            system: None,
            skill_id: None,
            offered_skills: Vec::new(),
            prompt: None,
            limits: Default::default(),
            at_ms: 1,
        }];
        for index in 0..5 {
            for event in run
                .decide(RunCommand::Emit {
                    payload: json!({ "n": index }),
                    at_ms: 10,
                })
                .unwrap()
            {
                run.apply(&event);
                log.push(event);
            }
        }
        let replayed = Run::replay(&log);
        assert_eq!(replayed.emitted, run.emitted);
        assert_eq!(replayed.emitted[3], json!({ "n": 3 }));
    }

    /// A late frame after the ending would arrive after a client already acted on it.
    #[test]
    fn a_finished_run_refuses_further_events() {
        let mut run = started();
        for event in run
            .decide(RunCommand::Finish {
                at_ms: 20,
                reason: None,
            })
            .unwrap()
        {
            run.apply(&event);
        }
        assert_eq!(
            run.decide(RunCommand::Emit {
                payload: json!({}),
                at_ms: 30
            }),
            Err(RunError::AlreadyEnded)
        );
    }

    #[test]
    fn a_run_cannot_end_twice() {
        let mut run = started();
        for event in run
            .decide(RunCommand::Finish {
                at_ms: 20,
                reason: None,
            })
            .unwrap()
        {
            run.apply(&event);
        }
        assert_eq!(
            run.decide(RunCommand::Finish {
                at_ms: 21,
                reason: None
            }),
            Err(RunError::AlreadyEnded)
        );
        assert_eq!(
            run.decide(RunCommand::Fail {
                reason: "late".to_string(),
                at_ms: 22
            }),
            Err(RunError::AlreadyEnded)
        );
    }

    #[test]
    fn a_failure_is_recorded_with_its_reason() {
        let mut run = started();
        for event in run
            .decide(RunCommand::Fail {
                reason: "upstream hung up".to_string(),
                at_ms: 20,
            })
            .unwrap()
        {
            run.apply(&event);
        }
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(run.failure.as_deref(), Some("upstream hung up"));
    }

    /// The interesting case after a restart: still `Running`, with no process behind it.
    #[test]
    fn an_interrupted_run_replays_as_running() {
        let log = vec![
            RunEvent::Started {
                thread_id: "t1".to_string(),
                coworker_id: None,
                model: None,
                effort: Effort::Inherit,
                inference_source: Default::default(),
                inference_via: None,
                system: None,
                skill_id: None,
                offered_skills: Vec::new(),
                prompt: None,
                limits: Default::default(),
                at_ms: 1,
            },
            RunEvent::Emitted {
                seq: 0,
                payload: json!({ "type": "RUN_STARTED" }),
                at_ms: 2,
            },
        ];
        let run = Run::replay(&log);
        assert_eq!(run.status, RunStatus::Running);
        assert_eq!(run.emitted.len(), 1, "and nothing it emitted was lost");
    }

    fn suspended() -> Run {
        let mut run = started();
        for event in run
            .decide(RunCommand::Suspend {
                call_id: "c1".to_string(),
                tool: "shell".to_string(),
                arguments: json!({"command": "rm -rf /"}),
                reason: SuspendReason::default(),
                at_ms: 10,
            })
            .unwrap()
        {
            run.apply(&event);
        }
        run
    }

    #[test]
    fn suspending_records_which_call_is_waiting() {
        let run = suspended();
        assert_eq!(run.status, RunStatus::AwaitingApproval);
        let pending = run.pending.as_ref().unwrap();
        assert_eq!(pending.call_id, "c1");
        assert_eq!(pending.tool, "shell");
        // The arguments are kept, because a person approving needs to see what they are approving.
        assert_eq!(pending.arguments["command"], "rm -rf /");
    }

    /// A suspended run is NOT ended: it must still accept events, which is what lets it be
    /// finished when the answer arrives days later.
    #[test]
    fn a_suspended_run_can_still_be_added_to_and_finished() {
        let mut run = suspended();
        let events = run
            .decide(RunCommand::Emit {
                payload: json!({"type": "TEXT_MESSAGE_CONTENT"}),
                at_ms: 20,
            })
            .unwrap();
        for event in &events {
            run.apply(event);
        }
        assert_eq!(run.emitted.len(), 1);
        assert!(
            run.decide(RunCommand::Finish {
                at_ms: 30,
                reason: None
            })
            .is_ok()
        );
    }

    /// THE EXACTLY-ONCE PROPERTY. A second answer for the same call produces no event, so the tool
    /// cannot run twice however many times the request is retried.
    #[test]
    fn a_call_can_only_be_answered_once() {
        let mut run = suspended();
        let first = run
            .decide(RunCommand::Answer {
                call_id: "c1".to_string(),
                approved: true,
                by: "acct_1".to_string(),
                at_ms: 20,
            })
            .unwrap();
        assert_eq!(first.len(), 1);
        for event in &first {
            run.apply(event);
        }

        // Every later attempt, however it arrives.
        for _ in 0..3 {
            assert_eq!(
                run.decide(RunCommand::Answer {
                    call_id: "c1".to_string(),
                    approved: true,
                    by: "acct_1".to_string(),
                    at_ms: 21,
                }),
                Err(RunError::AlreadyAnswered)
            );
        }
    }

    /// Answering a call that is not the one waiting must not release the one that is.
    #[test]
    fn answering_the_wrong_call_does_nothing() {
        let run = suspended();
        assert_eq!(
            run.decide(RunCommand::Answer {
                call_id: "some-other-call".to_string(),
                approved: true,
                by: "acct_1".to_string(),
                at_ms: 20,
            }),
            Err(RunError::NotAwaiting)
        );
        // And the real one is still waiting.
        assert_eq!(run.status, RunStatus::AwaitingApproval);
    }

    /// A run nobody suspended cannot be answered — an answer is a reply, not a command.
    #[test]
    fn a_running_run_cannot_be_answered() {
        assert_eq!(
            started().decide(RunCommand::Answer {
                call_id: "c1".to_string(),
                approved: true,
                by: "acct_1".to_string(),
                at_ms: 20,
            }),
            Err(RunError::NotAwaiting)
        );
    }

    /// A refusal is an answer too: it ends the waiting and the run continues, because the model
    /// still has to be told no.
    #[test]
    fn a_refusal_also_releases_the_run() {
        let mut run = suspended();
        for event in run
            .decide(RunCommand::Answer {
                call_id: "c1".to_string(),
                approved: false,
                by: "acct_1".to_string(),
                at_ms: 20,
            })
            .unwrap()
        {
            run.apply(&event);
        }
        assert_eq!(run.status, RunStatus::Running);
        assert!(run.pending.is_none());
    }

    /// Replay reaches the same conclusion, which is what makes the guarantee survive a restart:
    /// a process that comes back mid-approval must not accept a second answer.
    #[test]
    fn exactly_once_survives_a_replay() {
        let log = vec![
            RunEvent::Started {
                thread_id: "t1".to_string(),
                coworker_id: None,
                model: None,
                effort: Effort::Inherit,
                inference_source: Default::default(),
                inference_via: None,
                system: None,
                skill_id: None,
                offered_skills: Vec::new(),
                prompt: None,
                limits: Default::default(),
                at_ms: 1,
            },
            RunEvent::Suspended {
                call_id: "c1".to_string(),
                tool: "shell".to_string(),
                arguments: json!({}),
                reason: SuspendReason::default(),
                at_ms: 2,
            },
            RunEvent::Answered {
                call_id: "c1".to_string(),
                approved: true,
                by: "acct_1".to_string(),
                at_ms: 3,
            },
        ];
        let run = Run::replay(&log);
        assert_eq!(
            run.decide(RunCommand::Answer {
                call_id: "c1".to_string(),
                approved: true,
                by: "acct_1".to_string(),
                at_ms: 4,
            }),
            Err(RunError::AlreadyAnswered),
            "a restarted process must not accept a second answer"
        );
    }

    /// A STOP IS NOT A FAILURE. The status says stopped and nothing is recorded as a failure
    /// reason, so "why did this run fail" never has to answer for somebody changing their mind.
    #[test]
    fn stopping_is_its_own_ending_and_not_a_failure() {
        let mut run = started();
        for event in run
            .decide(RunCommand::Stop {
                by: "acct_1".to_string(),
                at_ms: 20,
            })
            .unwrap()
        {
            run.apply(&event);
        }
        assert_eq!(run.status, RunStatus::Stopped);
        assert_eq!(run.failure, None, "a stop has no failure to report");
        assert_eq!(run.stopped_by.as_deref(), Some("acct_1"));
        assert!(run.status.is_terminal());
    }

    /// Idempotence, from the aggregate's side: every attempt after the first says the run has
    /// already ended, which is what the route turns into a success.
    #[test]
    fn a_stopped_run_refuses_every_further_ending() {
        let mut run = started();
        for event in run
            .decide(RunCommand::Stop {
                by: "acct_1".to_string(),
                at_ms: 20,
            })
            .unwrap()
        {
            run.apply(&event);
        }
        for attempt in [
            RunCommand::Stop {
                by: "acct_1".to_string(),
                at_ms: 21,
            },
            RunCommand::Finish {
                at_ms: 22,
                reason: None,
            },
            RunCommand::Fail {
                reason: "late".to_string(),
                at_ms: 23,
            },
        ] {
            assert_eq!(run.decide(attempt), Err(RunError::AlreadyEnded));
        }
        assert_eq!(
            run.status,
            RunStatus::Stopped,
            "and the outcome is unchanged"
        );
    }

    /// Stopping a run that already ended must not rewrite what happened to it.
    #[test]
    fn stopping_a_finished_run_leaves_its_outcome_alone() {
        let mut run = started();
        for event in run
            .decide(RunCommand::Finish {
                at_ms: 20,
                reason: None,
            })
            .unwrap()
        {
            run.apply(&event);
        }
        assert_eq!(
            run.decide(RunCommand::Stop {
                by: "acct_1".to_string(),
                at_ms: 21,
            }),
            Err(RunError::AlreadyEnded)
        );
        assert_eq!(run.status, RunStatus::Finished);

        let mut failed = started();
        for event in failed
            .decide(RunCommand::Fail {
                reason: "upstream hung up".to_string(),
                at_ms: 20,
            })
            .unwrap()
        {
            failed.apply(&event);
        }
        assert_eq!(
            failed.decide(RunCommand::Stop {
                by: "acct_1".to_string(),
                at_ms: 21,
            }),
            Err(RunError::AlreadyEnded)
        );
        assert_eq!(failed.status, RunStatus::Failed);
        assert_eq!(failed.failure.as_deref(), Some("upstream hung up"));
    }

    /// THE FRAMES THE TURN WAS MID-ROUND ON ARE STILL THE RECORD. The loop journals a whole round
    /// at a time, so the round in progress when somebody pressed stop lands after the stop does;
    /// refusing it would end the transcript one step before the moment the person was watching.
    #[test]
    fn frames_still_in_flight_when_the_stop_landed_are_kept() {
        let mut run = started();
        for event in run
            .decide(RunCommand::Stop {
                by: "acct_1".to_string(),
                at_ms: 20,
            })
            .unwrap()
        {
            run.apply(&event);
        }
        let events = run
            .decide(RunCommand::Emit {
                payload: json!({"type": "TEXT_MESSAGE_CONTENT", "delta": "half a sentence"}),
                at_ms: 21,
            })
            .unwrap();
        for event in &events {
            run.apply(event);
        }
        assert_eq!(run.emitted.len(), 1);
        assert_eq!(
            run.status,
            RunStatus::Stopped,
            "a late frame does not un-stop the run"
        );
    }

    /// Stopping while a card is open is the case the button exists for as much as any other, and
    /// the card must not outlive the run it was asking about.
    #[test]
    fn stopping_a_run_that_was_waiting_closes_its_card() {
        let mut run = suspended();
        for event in run
            .decide(RunCommand::Stop {
                by: "acct_1".to_string(),
                at_ms: 20,
            })
            .unwrap()
        {
            run.apply(&event);
        }
        assert_eq!(run.status, RunStatus::Stopped);
        assert!(run.pending.is_none(), "the question is moot");
        assert_eq!(
            run.decide(RunCommand::Answer {
                call_id: "c1".to_string(),
                approved: true,
                by: "acct_1".to_string(),
                at_ms: 21,
            }),
            Err(RunError::NotAwaiting),
            "and answering it cannot restart the run"
        );
    }

    fn start_command() -> RunCommand {
        RunCommand::Start {
            thread_id: "t1".to_string(),
            coworker_id: None,
            model: None,
            effort: Effort::Inherit,
            inference_source: Default::default(),
            system: None,
            skill_id: None,
            offered_skills: Vec::new(),
            prompt: None,
            limits: Default::default(),
            at_ms: 30,
        }
    }

    fn apply_all(run: &mut Run, command: RunCommand) {
        for event in run.decide(command).unwrap() {
            run.apply(&event);
        }
    }

    /// The property test's first counterexample: a second Start reopened a finished run, and
    /// its next Finish was a second ending.
    #[test]
    fn a_finished_run_cannot_be_started_again() {
        let mut run = started();
        apply_all(
            &mut run,
            RunCommand::Finish {
                at_ms: 2,
                reason: None,
            },
        );
        assert_eq!(run.decide(start_command()), Err(RunError::AlreadyStarted));
        assert_eq!(run.status, RunStatus::Finished);
    }

    /// Its second: an ending recorded before the Start, then the Start.
    #[test]
    fn a_run_ended_before_it_started_cannot_start() {
        let mut run = Run::default();
        apply_all(
            &mut run,
            RunCommand::Finish {
                at_ms: 1,
                reason: None,
            },
        );
        assert_eq!(run.decide(start_command()), Err(RunError::AlreadyStarted));
    }

    /// Its third: a run failed while parked kept its card pending, and a late answer set it
    /// back to running. A Stop always closed the card; a failure and a finish now refuse too.
    #[test]
    fn a_run_failed_while_waiting_takes_no_answer() {
        for ending in [
            RunCommand::Fail {
                reason: "the hold timed out".to_string(),
                at_ms: 20,
            },
            RunCommand::Finish {
                at_ms: 20,
                reason: None,
            },
        ] {
            let mut run = suspended();
            apply_all(&mut run, ending);
            let status = run.status;
            assert_eq!(
                run.decide(RunCommand::Answer {
                    call_id: "c1".to_string(),
                    approved: true,
                    by: "acct_1".to_string(),
                    at_ms: 21,
                }),
                Err(RunError::NotAwaiting),
                "a late answer revives nothing"
            );
            assert!(status.is_terminal());
        }
    }

    /// The whole point of the log: a process that comes back must reach the same conclusion, or a
    /// stop is undone by a restart.
    #[test]
    fn a_stop_survives_a_replay() {
        let log = vec![
            RunEvent::Started {
                thread_id: "t1".to_string(),
                coworker_id: None,
                model: None,
                effort: Effort::Inherit,
                inference_source: Default::default(),
                inference_via: None,
                system: None,
                skill_id: None,
                offered_skills: Vec::new(),
                prompt: None,
                limits: Default::default(),
                at_ms: 1,
            },
            RunEvent::Emitted {
                seq: 0,
                payload: json!({ "type": "RUN_STARTED" }),
                at_ms: 2,
            },
            RunEvent::Stopped {
                by: "acct_1".to_string(),
                at_ms: 3,
            },
        ];
        let run = Run::replay(&log);
        assert_eq!(run.status, RunStatus::Stopped);
        assert_eq!(run.emitted.len(), 1, "and nothing it emitted was lost");
        assert_eq!(
            run.decide(RunCommand::Finish {
                at_ms: 4,
                reason: None
            }),
            Err(RunError::AlreadyEnded),
            "a restarted process must not finish a run somebody stopped"
        );
    }

    /// The stored word round-trips, because the projection is read back as a status and a
    /// misreading would put a stopped run back in front of the recovery sweep.
    #[test]
    fn the_stored_status_word_round_trips() {
        for status in [
            RunStatus::Running,
            RunStatus::AwaitingApproval,
            RunStatus::Finished,
            RunStatus::Failed,
            RunStatus::Stopped,
        ] {
            assert_eq!(RunStatus::from_stored(status.as_str()), status);
        }
        assert_eq!(
            RunStatus::from_stored("something we have never heard of"),
            RunStatus::Running,
            "an unreadable status must not be mistaken for an ending"
        );
    }

    #[test]
    fn the_stored_suspend_reason_word_round_trips() {
        for reason in [
            SuspendReason::ExecConsent,
            SuspendReason::PolicyApproval,
            SuspendReason::AutoReview,
            SuspendReason::UserForm,
        ] {
            assert_eq!(SuspendReason::from_stored(reason.as_str()), reason);
        }
        assert_eq!(
            SuspendReason::from_stored("something we have never heard of"),
            SuspendReason::ExecConsent,
            "an unreadable reason is the closed reading"
        );
    }

    #[test]
    fn emitting_before_starting_is_refused() {
        assert_eq!(
            Run::default().decide(RunCommand::Emit {
                payload: json!({}),
                at_ms: 1
            }),
            Err(RunError::NotStarted)
        );
    }

    #[test]
    fn a_started_run_remembers_its_pin() {
        let mut run = Run::default();
        for event in run
            .decide(RunCommand::Start {
                thread_id: "t1".to_string(),
                coworker_id: None,
                model: Some("openai/gpt-5.5".to_string()),
                effort: Effort::Inherit,
                inference_source: Default::default(),
                system: None,
                skill_id: None,
                offered_skills: Vec::new(),
                prompt: None,
                limits: Default::default(),
                at_ms: 1,
            })
            .unwrap()
        {
            run.apply(&event);
        }
        assert_eq!(run.model.as_deref(), Some("openai/gpt-5.5"));
        assert_eq!(run.pin_for_resume("oag/auto"), "openai/gpt-5.5");
    }

    #[test]
    fn a_log_without_a_pin_resumes_on_the_current_one() {
        let event: RunEvent = serde_json::from_str(
            r#"{"type":"started","thread_id":"t1","coworker_id":null,"at_ms":1}"#,
        )
        .unwrap();
        let run = Run::replay([&event]);
        assert_eq!(run.model, None);
        assert_eq!(run.pin_for_resume("oag/auto"), "oag/auto");
    }

    /// What a run may spend goes into the log with its start and comes back out of it, so a
    /// resume days later is held to the budget the run began with.
    #[test]
    fn a_started_run_keeps_the_limits_it_started_with() {
        let limits = RunLimits {
            max_rounds: std::num::NonZeroU32::new(3),
            max_computer_rounds: std::num::NonZeroU32::new(24),
            max_wall_ms: std::num::NonZeroU64::new(900_000),
        };
        let started = Run::default()
            .decide(RunCommand::Start {
                thread_id: "t1".to_string(),
                coworker_id: None,
                model: None,
                effort: Effort::Inherit,
                inference_source: Default::default(),
                system: None,
                skill_id: None,
                offered_skills: Vec::new(),
                prompt: None,
                limits,
                at_ms: 1,
            })
            .unwrap();
        let stored: Vec<RunEvent> = started
            .iter()
            .map(|event| serde_json::from_value(serde_json::to_value(event).unwrap()).unwrap())
            .collect();
        assert_eq!(Run::replay(&stored).limits, limits);
    }

    /// A start written before run limits existed still folds, as a run that set none: it is held
    /// to the server's budget, which is what every run was held to then.
    #[test]
    fn a_start_written_before_run_limits_sets_none() {
        let event: RunEvent = serde_json::from_str(
            r#"{"type":"started","thread_id":"t1","coworker_id":null,"model":"xai/grok-4.6","at_ms":1}"#,
        )
        .unwrap();
        let run = Run::replay([&event]);
        assert!(run.started);
        assert_eq!(run.limits, RunLimits::default());
    }

    /// A log written before prompts were journaled still folds, and says it was never journaled
    /// rather than that nobody spoke: the thread's history falls back to the client's copy.
    #[test]
    fn a_start_written_before_prompts_still_reads() {
        let event: RunEvent = serde_json::from_str(
            r#"{"type":"started","thread_id":"t1","coworker_id":null,"at_ms":1}"#,
        )
        .unwrap();
        assert_eq!(Run::replay([&event]).prompt, None);
    }

    /// The person's words go in as they came, with fields nobody here models, and come back out
    /// of the log the same (CLAUDE.md #2).
    #[test]
    fn a_journaled_prompt_keeps_what_the_client_sent() {
        let asked = json!({"id":"m1","role":"user","content":"hi","replyTo":{"messageId":"a0"},
            "aFieldFromNextRelease":[1]});
        let mut run = Run::default();
        let events = run
            .decide(RunCommand::Start {
                thread_id: "t1".to_string(),
                coworker_id: None,
                model: None,
                effort: Effort::Inherit,
                inference_source: Default::default(),
                system: None,
                skill_id: None,
                offered_skills: Vec::new(),
                prompt: Some(vec![asked.clone()]),
                limits: Default::default(),
                at_ms: 1,
            })
            .unwrap();
        let stored: Vec<RunEvent> = events
            .iter()
            .map(|event| serde_json::from_value(serde_json::to_value(event).unwrap()).unwrap())
            .collect();
        for event in &stored {
            run.apply(event);
        }
        assert_eq!(run.prompt, Some(vec![asked]));
    }
}
