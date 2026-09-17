//! Workflows: the decision tree that calls recipes.
//!
//! A recipe is a taped sequence, replayed exactly. It cannot look at the screen and decide, and
//! that is not a defect — it is what a tape is. The cost of it was watched on 16 Sep 2026: a bot
//! replayed one taught search twenty-five times in a row, because every attempt left the search
//! field in a state the next attempt made worse, and nothing in the tape could notice. A workflow
//! is the sibling that can: it looks, it asks, and it chooses which recipe to play next.
//!
//! THE TREE BRANCHES OUTSIDE THE BOX, DELIBERATELY. The box's recipe runner is a flat loop with no
//! jump and no label, and every stored recipe version is immutable JSON written against that
//! shape. A conditional step added there would have to be understood by a runner that has already
//! shipped, so it is not added there: this engine calls the box one whole recipe at a time and
//! does the deciding on this side.
//!
//! # The body shape
//!
//! A workflow is stored as `recipe_version.body` with `recipe_version.kind = 'workflow'` — the
//! fourth kind beside `raw`, `filtered` and `edited`. It is the same row, the same versioning, the
//! same ownership, sharing, grants, run history and artifacts, because none of those care whether
//! a body is clicks or a decision tree.
//!
//! ```json
//! {
//!   "workflow": 1,
//!   "start": "look",
//!   "parameters": [ { "name": "term", "required": true, "kind": "text", ... } ],
//!   "budget": { "steps": 40, "seconds": 600 },
//!   "steps": {
//!     "look":   { "do": "observe", "as": "windows", "shell": "wmctrl -lx", "then": "in-chrome" },
//!     "in-chrome": { "do": "when", "fact": "windows", "test": { "contains": "chrome" },
//!                    "yes": "is-it-clean", "no": "open-chrome" },
//!     "open-chrome": { "do": "run", "recipe": "rcp_open", "then": "look",
//!                      "otherwise": "gave-up" },
//!     "is-it-clean": { "do": "ask", "name": "empty",
//!                      "question": { "kind": "noul",
//!                                    "instructions": "Is the search field empty?" },
//!                      "yes": "search", "no": "clear" },
//!     "clear":  { "do": "run", "recipe": "rcp_clear", "then": "search" },
//!     "search": { "do": "run", "recipe": "rcp_search", "values": { "q": "{{term}}" },
//!                 "then": "done" },
//!     "done":   { "do": "stop", "outcome": "done", "say": "searched once" },
//!     "gave-up":{ "do": "stop", "outcome": "gave-up", "say": "no browser to search in" }
//!   }
//! }
//! ```
//!
//! WHY A MAP OF NAMED STEPS RATHER THAN A LIST. A body is immutable once written, so every edit is
//! a new version, and a reader comparing two versions is reading two JSON documents side by side.
//! Positional indices make that comparison lie: inserting one step at the top renumbers every jump
//! after it, and a diff shows the whole tree as changed. Names survive insertion, and a jump that
//! names a step that is gone is caught by the lint rather than landing on whatever now occupies
//! that index.
//!
//! WHY `do` TAGS THE STEP AND EVERY EXIT IS ITS OWN FIELD. Each kind declares exactly the exits it
//! has — `then`/`otherwise` for a recipe, `yes`/`no` for a two-way branch, `go` keyed by the
//! answer's own word for a choice or a score — so "which step comes next" is answered by reading
//! the step, not by reading the engine. A single `next` field holding sometimes a string and
//! sometimes a map would put that knowledge back in the code.
//!
//! WHY A RECIPE IS NAMED WITHOUT A VERSION. A `run` step names `rcp_…` and gets whatever that
//! recipe's runnable version is at the moment it plays, which is the same rule the recipe routes
//! and the agent's `run_recipe` tool already follow. Pinning a version inside an immutable body
//! would mean a recipe fixed today stays broken inside every workflow that calls it until each of
//! them is rewritten, which is the opposite of why the recipe was fixed.
//!
//! WHY THE SHAPE CARRIES ITS OWN NUMBER. `"workflow": 1` is the first field, and a body that says
//! anything else is refused by name rather than read optimistically. Old bodies are permanent: the
//! day the shape changes, the reader has to be able to tell which one it is holding without
//! guessing from which fields happen to be present.
//!
//! # What a condition can actually see
//!
//! Only what the box hands back today, which is less than it sounds:
//!
//! - **A shell probe.** `observe` runs a command on the box through `Computer::run` and keeps its
//!   exit code and the shape of its output. This is the real eye: the box's desktop image carries
//!   `wmctrl`, `xdotool` and `xwininfo`, so "is a Chrome window up", "what is the focused window
//!   called" and "does this file exist" are all answerable now.
//! - **The last recipe's receipt.** `ok`, how many steps ran, which step stopped it and the error
//!   the box reported, kept as the facts `last.ok`, `last.ran`, `last.stopped_at`, `last.error`.
//!
//! And nothing else. In particular the engine CANNOT see the screen: `Computer::screenshot`
//! returns a PNG, Jev judges text and objects, and there is no vision model on this path — so a
//! question phrased as "does the search box look wrong" is answered from the facts a probe
//! gathered, never from the picture. Box PR #29 makes a recipe receipt carry what was observed
//! while it played — the window under each click, where the keystrokes went, the page URL either
//! side of a step — behind an `observe` request field. This server pins the box nine commits
//! before that (#130), so none of it is reachable yet. When it is, those observations become more
//! `last.*` facts and no step kind has to change.
//!
//! # Termination
//!
//! A tree that can jump can loop, so a walk is bounded three ways and every bound is a reported
//! ending rather than a panic, an error or a silence:
//!
//! 1. **A step budget.** No walk takes more steps than its budget allows.
//! 2. **A wall-clock budget.** No step STARTS after the deadline. It is put that way on purpose:
//!    abandoning a recipe call already in flight would not stop the box — the recipe keeps
//!    playing — it would only lose the receipt and leave the tree deciding blind, so the clock is
//!    read between steps and the last step is allowed to finish.
//! 3. **A circuit breaker on standing still.** Arriving at the same step with exactly the facts it
//!    had last time means the loop has nothing new to go on. That is not proof the world is
//!    unchanged — the twenty-five-run bug had identical receipts and a worsening screen every
//!    time, which is precisely the point — so it is allowed a few repeats before the walk is cut,
//!    rather than firing the first time round.
//!
//! The lint adds a static guard on top: a tree from which no `stop` step is reachable is refused
//! at write time. That cannot prove a tree terminates, which is why the runtime bounds exist; it
//! does catch the tree that could never have ended.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::time::Duration;

// TOKIO'S CLOCK, NOT THE STANDARD LIBRARY'S. The two behave identically in a running server, and
// under `#[tokio::test(start_paused = true)]` this one is virtual — which is what lets the
// wall-clock bound be tested in milliseconds instead of by making the suite wait out a budget.
use tokio::time::Instant;

use opengrok_box::Computer;
use opengrok_core::id::CoworkerId;
use opengrok_recipes::{Parameter, Values};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{RecipeReceipt, RecipeSource};

/// The `recipe_version.kind` a decision tree is stored under.
pub const KIND: &str = "workflow";

/// The body shape this engine reads and writes. See the module doc for why it is written down
/// rather than inferred.
pub const SHAPE: u32 = 1;

/// More steps than this in one tree and it is not a tree anybody is reading. The box's own recipe
/// cap is 256 steps for the same reason.
pub const MAX_STEPS: usize = 200;

/// How much output one probe contributes to a fact. Enough for a window list, few enough that a
/// `cat` of the wrong file does not become the run's state.
const FACT_CHARS: usize = 2_000;

/// How long a probe may take before the box gives up on it, and the ceiling on what a body may
/// ask for. A probe is a question about the screen, not a job.
const PROBE_SECONDS: u32 = 30;
const PROBE_SECONDS_MAX: u32 = 120;

/// How many times a step may be re-entered with the facts it already had before the walk is cut
/// for standing still. Not one, because identical facts are not identical world — see the module
/// doc — and a couple of retries against a flaky screen is a thing an author may legitimately
/// write.
const SAME_PLACE_ALLOWED: usize = 3;

// -------------------------------------------------------------------------------------------
// The body
// -------------------------------------------------------------------------------------------

/// What a walk may spend. Both are clamped on the way in, so an immutable body cannot carry a
/// budget a later deployment considers absurd.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    pub steps: usize,
    pub seconds: u64,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            steps: 40,
            seconds: 600,
        }
    }
}

impl Budget {
    /// The ceilings. A body asking for more gets the ceiling rather than a refusal: a workflow
    /// written when the cap was higher must still run, and running it with less rope is safe in
    /// the direction that matters.
    pub const MAX_STEPS: usize = 500;
    pub const MAX_SECONDS: u64 = 3_600;

    #[must_use]
    pub fn clamped(self) -> Self {
        Self {
            steps: self.steps.clamp(1, Self::MAX_STEPS),
            seconds: self.seconds.clamp(1, Self::MAX_SECONDS),
        }
    }
}

/// What a `when` step asks of a fact. Three tests and no regular expressions: a regex in an
/// immutable body is a dialect question nobody can answer later, and these three cover what a
/// probe's output is actually asked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Test {
    /// Exactly this, after trimming.
    Is(String),
    /// This somewhere in it.
    Contains(String),
    /// Nothing there. A fact never gathered is empty too, because a probe that did not run and a
    /// probe that found nothing leave the tree with the same amount to go on.
    Empty,
}

impl Test {
    fn word(&self) -> &'static str {
        match self {
            Self::Is(_) => "is",
            Self::Contains(_) => "contains",
            Self::Empty => "empty",
        }
    }
}

/// One option of a `choice` question: a bare label, or a label with what it means. Both, because
/// a list of words is the common case and a description is what makes two similar labels
/// distinguishable to a classifier. The same pair `/jev/ask` takes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Choice {
    Bare(String),
    Described { label: String, means: String },
}

impl Choice {
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Bare(label) | Self::Described { label, .. } => label,
        }
    }

    #[must_use]
    pub fn means(&self) -> Option<&str> {
        match self {
            Self::Bare(_) => None,
            Self::Described { means, .. } => Some(means),
        }
    }
}

/// What the tree asks Jev, in Jev's own vocabulary.
///
/// THE FIELD NAMES ARE THE ONES `/jev/ask` ALREADY TAKES (`jev/routes.rs`): `instructions` for a
/// question's text, `choices`, `levels`, `yesMeans`/`noMeans`. Somebody who can write a question
/// for that route can write one here without a translation table, and the server's adapter turns
/// this into the SDK's `Question` in one place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum Question {
    /// Yes or no.
    Noul {
        instructions: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        yes_means: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        no_means: Option<String>,
    },
    /// One label out of several.
    Choice {
        instructions: String,
        choices: Vec<Choice>,
    },
    /// A rung on a rubric, given in order.
    Score {
        instructions: String,
        levels: Vec<String>,
    },
}

impl Question {
    #[must_use]
    pub fn instructions(&self) -> &str {
        match self {
            Self::Noul { instructions, .. }
            | Self::Choice { instructions, .. }
            | Self::Score { instructions, .. } => instructions,
        }
    }

    #[must_use]
    pub fn kind_word(&self) -> &'static str {
        match self {
            Self::Noul { .. } => "noul",
            Self::Choice { .. } => "choice",
            Self::Score { .. } => "score",
        }
    }

    fn with_instructions(&self, text: String) -> Self {
        let mut filled = self.clone();
        match &mut filled {
            Self::Noul { instructions, .. }
            | Self::Choice { instructions, .. }
            | Self::Score { instructions, .. } => *instructions = text,
        }
        filled
    }

    /// THE AGREED ANSWER WHEN JEV CANNOT ANSWER, and the words for why that answer.
    ///
    /// One rule per kind, decided once and written here rather than at each call site, so the
    /// deterministic behaviour of a workflow with no classifier is a property of the body and not
    /// of which branch of the engine happened to run:
    ///
    /// - a **noul** answers no, taking the branch that skips — the safe half of a yes/no is the
    ///   one that does not act on a judgement nobody made;
    /// - a **choice** takes the first option, WHICH IS THEREFORE THE ONE AN AUTHOR MUST MAKE SAFE;
    /// - a **score** takes the middle level, because the ends of a rubric are the opinions and
    ///   the middle is the absence of one.
    ///
    /// `None` for a question with no options at all, which the lint refuses at write time.
    fn fallback(&self) -> Option<(String, &'static str)> {
        match self {
            Self::Noul { .. } => Some(("no".to_string(), "the branch that skips")),
            Self::Choice { choices, .. } => choices
                .first()
                .map(|first| (first.label().to_string(), "the first option offered")),
            Self::Score { levels, .. } => {
                // The upper middle when there is no exact centre. Stated rather than left to a
                // reader to derive: a four-rung rubric falls on the third rung, and an author who
                // wants the other one writes an odd number of rungs.
                levels
                    .get(levels.len() / 2)
                    .map(|level| (level.clone(), "the middle level"))
            }
        }
    }
}

/// One step: what to do, and where to go from each way it can turn out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "do", rename_all = "snake_case")]
pub enum Act {
    /// Play a whole recipe on the box.
    Run {
        recipe: String,
        #[serde(default, skip_serializing_if = "Values::is_empty")]
        values: Values,
        then: String,
        /// Where a recipe that stopped short goes. Absent means a stopped recipe ends the walk,
        /// which is the honest default: a tape that did not play left the screen somewhere the
        /// author never described, and carrying on from there is how the twenty-five-run bug
        /// happened.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        otherwise: Option<String>,
    },
    /// Look at the box and keep what was seen as a named fact.
    Observe {
        #[serde(rename = "as")]
        fact: String,
        shell: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seconds: Option<u32>,
        then: String,
    },
    /// Branch on a fact, with no model involved.
    When {
        fact: String,
        test: Test,
        yes: String,
        no: String,
    },
    /// Branch on what Jev says.
    Ask {
        /// The name the answer comes back under. Also what the trail calls it.
        name: String,
        question: Question,
        /// A noul's two exits.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        yes: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        no: Option<String>,
        /// A choice's or a score's exits, keyed by the answer's own word.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        go: BTreeMap<String, String>,
    },
    /// The end, with the author's own word for which end it is.
    Stop {
        outcome: String,
        #[serde(default)]
        say: String,
    },
}

impl Act {
    fn word(&self) -> &'static str {
        match self {
            Self::Run { .. } => "run",
            Self::Observe { .. } => "observe",
            Self::When { .. } => "when",
            Self::Ask { .. } => "ask",
            Self::Stop { .. } => "stop",
        }
    }

    /// Every step this one can go to. The lint walks this; so does the reachability check.
    fn exits(&self) -> Vec<&str> {
        match self {
            Self::Run {
                then, otherwise, ..
            } => {
                let mut out = vec![then.as_str()];
                out.extend(otherwise.as_deref());
                out
            }
            Self::Observe { then, .. } => vec![then.as_str()],
            Self::When { yes, no, .. } => vec![yes.as_str(), no.as_str()],
            Self::Ask { yes, no, go, .. } => {
                let mut out: Vec<&str> = Vec::new();
                out.extend(yes.as_deref());
                out.extend(no.as_deref());
                out.extend(go.values().map(String::as_str));
                out
            }
            Self::Stop { .. } => Vec::new(),
        }
    }
}

/// A decision tree, parsed and linted.
#[derive(Debug, Clone, PartialEq)]
pub struct Workflow {
    pub start: String,
    pub steps: BTreeMap<String, Act>,
    pub parameters: Vec<Parameter>,
    pub budget: Budget,
}

/// The stored body, exactly. Separate from `Workflow` because the two have different jobs: this
/// one mirrors the JSON, that one is the thing a walk reads and has already been checked.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Body {
    #[serde(default)]
    workflow: u32,
    #[serde(default)]
    start: String,
    #[serde(default)]
    steps: BTreeMap<String, Act>,
    #[serde(default)]
    parameters: Vec<Parameter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    budget: Option<Budget>,
}

impl Workflow {
    /// Read a stored body. The sentence in the error is shown to a person, so it names the step.
    pub fn parse(body: &Value) -> Result<Self, String> {
        let shape = body.get("workflow").and_then(Value::as_u64).unwrap_or(0);
        if shape != u64::from(SHAPE) {
            return Err(if shape == 0 {
                format!(
                    "a workflow body must say which shape it is written in: \"workflow\": {SHAPE}"
                )
            } else {
                format!(
                    "this workflow is written in shape {shape}; this server understands shape \
                     {SHAPE}"
                )
            });
        }
        let parsed: Body =
            serde_json::from_value(body.clone()).map_err(|error| format!("unreadable: {error}"))?;
        let workflow = Self {
            start: parsed.start,
            steps: parsed.steps,
            parameters: parsed.parameters,
            budget: parsed.budget.unwrap_or_default().clamped(),
        };
        lint(&workflow)?;
        Ok(workflow)
    }

    /// The body to store. Round-trips `parse`, stamped with the shape it was written in.
    #[must_use]
    pub fn to_body(&self) -> Value {
        serde_json::to_value(Body {
            workflow: SHAPE,
            start: self.start.clone(),
            steps: self.steps.clone(),
            parameters: self.parameters.clone(),
            budget: Some(self.budget),
        })
        .unwrap_or_else(|_| json!({}))
    }

    /// Every recipe this tree can play. What a route pre-flights the caller's permission against,
    /// so a run is refused before it starts rather than half way through.
    #[must_use]
    pub fn recipes(&self) -> BTreeSet<String> {
        self.steps
            .values()
            .filter_map(|act| match act {
                Act::Run { recipe, .. } => Some(recipe.clone()),
                _ => None,
            })
            .collect()
    }
}

/// Why a tree cannot be stored. One sentence, naming the step, because it is shown to whoever
/// wrote the tree.
pub fn lint(workflow: &Workflow) -> Result<(), String> {
    if workflow.steps.is_empty() {
        return Err("a workflow needs at least one step".to_string());
    }
    if workflow.steps.len() > MAX_STEPS {
        return Err(format!(
            "a workflow may have {MAX_STEPS} steps; this one has {}",
            workflow.steps.len()
        ));
    }
    opengrok_recipes::check(&workflow.parameters)?;
    if workflow.start.trim().is_empty() {
        return Err("a workflow needs a `start`: the step it begins at".to_string());
    }
    if !workflow.steps.contains_key(&workflow.start) {
        return Err(format!(
            "`start` names \"{}\", and there is no step called that",
            workflow.start
        ));
    }
    for (name, act) in &workflow.steps {
        if name.trim().is_empty() {
            return Err("a step with no name cannot be jumped to".to_string());
        }
        for exit in act.exits() {
            if !workflow.steps.contains_key(exit) {
                return Err(format!(
                    "step \"{name}\" goes to \"{exit}\", and there is no step called that"
                ));
            }
        }
        lint_act(name, act)?;
    }
    // A TREE WITH NO REACHABLE END IS REFUSED AT WRITE TIME. This does not prove a tree
    // terminates — a loop between two live steps passes it — and the runtime budgets are what
    // actually bound a walk. It proves the cheap half: that an ending exists at all, which a tree
    // written by dragging boxes around loses the moment somebody deletes the last `stop`.
    if !reaches_a_stop(workflow) {
        return Err(
            "no `stop` step can be reached from `start`, so this workflow could never end"
                .to_string(),
        );
    }
    Ok(())
}

fn lint_act(name: &str, act: &Act) -> Result<(), String> {
    match act {
        Act::Run { recipe, .. } => {
            if recipe.trim().is_empty() {
                return Err(format!("step \"{name}\" runs a recipe with no id"));
            }
        }
        Act::Observe { fact, shell, .. } => {
            if !is_a_fact_name(fact) {
                return Err(format!(
                    "step \"{name}\" keeps what it sees as \"{fact}\"; a fact's name is lowercase \
                     letters, digits, underscores and dots"
                ));
            }
            if shell.trim().is_empty() {
                return Err(format!("step \"{name}\" looks at the box with no command"));
            }
        }
        Act::When { fact, .. } => {
            if fact.trim().is_empty() {
                return Err(format!("step \"{name}\" tests a fact with no name"));
            }
        }
        Act::Ask {
            name: question_name,
            question,
            yes,
            no,
            go,
        } => lint_ask(
            name,
            question_name,
            question,
            yes.as_deref(),
            no.as_deref(),
            go,
        )?,
        Act::Stop { outcome, .. } => {
            if outcome.trim().is_empty() {
                return Err(format!(
                    "step \"{name}\" ends the workflow without saying how it ended"
                ));
            }
        }
    }
    Ok(())
}

/// THE SAME CHECKS `/jev/ask` MAKES, MADE BEFORE THE BODY IS STORED RATHER THAN BEFORE THE CALL.
///
/// A malformed question at run time is `JevError::Asked`, which this engine treats as a fault and
/// refuses to fall back from — and a body is immutable, so such a question would fail identically
/// on every run forever. Catching it here is what makes that severity affordable.
fn lint_ask(
    step: &str,
    question_name: &str,
    question: &Question,
    yes: Option<&str>,
    no: Option<&str>,
    go: &BTreeMap<String, String>,
) -> Result<(), String> {
    if question_name.trim().is_empty() {
        return Err(format!(
            "step \"{step}\" asks a question with no name: its answer comes back under it"
        ));
    }
    if question.instructions().trim().is_empty() {
        return Err(format!(
            "the question \"{question_name}\" in step \"{step}\" has no words to it"
        ));
    }
    match question {
        Question::Noul { .. } => {
            if !go.is_empty() {
                return Err(format!(
                    "step \"{step}\" asks a yes-or-no question, so it goes by `yes` and `no`, not \
                     by `go`"
                ));
            }
            if yes.is_none() || no.is_none() {
                return Err(format!(
                    "step \"{step}\" asks a yes-or-no question and must say where each answer goes"
                ));
            }
        }
        Question::Choice { choices, .. } => {
            // ONE LABEL IS NOT A CHOICE, the same refusal `/jev/ask` gives: Jev would answer it
            // with that label at a probability of one, having decided nothing, and the call would
            // cost money to tell the caller what it already knew.
            if choices.len() < 2 {
                return Err(format!(
                    "the question \"{question_name}\" in step \"{step}\" needs at least two \
                     choices to choose between"
                ));
            }
            let mut seen = BTreeSet::new();
            for choice in choices {
                let label = choice.label();
                if label.trim().is_empty() {
                    return Err(format!(
                        "the question \"{question_name}\" in step \"{step}\" has a choice with no \
                         label"
                    ));
                }
                if !seen.insert(label) {
                    return Err(format!(
                        "the question \"{question_name}\" in step \"{step}\" offers \"{label}\" \
                         twice, and an answer is found by its label"
                    ));
                }
                if !go.contains_key(label) {
                    return Err(format!(
                        "step \"{step}\" offers the answer \"{label}\" and does not say where it \
                         goes"
                    ));
                }
            }
            reject_yes_no(step, yes, no)?;
        }
        Question::Score { levels, .. } => {
            if levels.is_empty() {
                return Err(format!(
                    "the question \"{question_name}\" in step \"{step}\" has no levels to score \
                     against"
                ));
            }
            let mut seen = BTreeSet::new();
            for level in levels {
                if level.trim().is_empty() {
                    return Err(format!(
                        "the question \"{question_name}\" in step \"{step}\" has a level with no \
                         words"
                    ));
                }
                if !seen.insert(level.as_str()) {
                    return Err(format!(
                        "the question \"{question_name}\" in step \"{step}\" has two levels called \
                         \"{level}\", and a branch is found by a level's words"
                    ));
                }
                if !go.contains_key(level) {
                    return Err(format!(
                        "step \"{step}\" scores against \"{level}\" and does not say where it goes"
                    ));
                }
            }
            reject_yes_no(step, yes, no)?;
        }
    }
    Ok(())
}

fn reject_yes_no(step: &str, yes: Option<&str>, no: Option<&str>) -> Result<(), String> {
    if yes.is_some() || no.is_some() {
        return Err(format!(
            "step \"{step}\" does not ask a yes-or-no question, so its branches go in `go`"
        ));
    }
    Ok(())
}

/// A fact's name: what an `observe` writes and a `when` reads. Dots are allowed because the
/// engine's own facts use them (`last.ok`), and a body that could not spell one could not test it.
fn is_a_fact_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.')
}

fn reaches_a_stop(workflow: &Workflow) -> bool {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut queue = vec![workflow.start.as_str()];
    while let Some(name) = queue.pop() {
        if !seen.insert(name) {
            continue;
        }
        let Some(act) = workflow.steps.get(name) else {
            continue;
        };
        if matches!(act, Act::Stop { .. }) {
            return true;
        }
        queue.extend(act.exits());
    }
    false
}

// -------------------------------------------------------------------------------------------
// The judge seam
// -------------------------------------------------------------------------------------------

/// One question, with what there is to judge it against.
#[derive(Debug, Clone)]
pub struct JudgeAsk<'a> {
    /// What has been seen and done, as an object. Never a bare number or a string: Jev's own
    /// content type refuses those, and a state that cannot be sent is a malformed question.
    pub state: &'a Value,
    pub name: &'a str,
    pub question: &'a Question,
}

/// What the judge decided, as one word to branch on.
///
/// ONE SHAPE FOR THREE KINDS OF ANSWER. A noul comes back as `"yes"` or `"no"`, a choice as its
/// label, a score as the words of the level it landed on — so the engine looks a branch up the
/// same way whatever was asked, and the three-way decoding lives once, in the adapter that owns
/// the classifier's wire.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub answer: String,
    pub confidence: f64,
}

/// Why the judge did not decide.
///
/// TWO KINDS HERE WHERE THE CLASSIFIER'S OWN SEAM KEEPS FOUR, and the reduction is the decision
/// rather than a loss. `JevError` separates a malformed question, an unreachable service, a
/// timeout and a refusal because four different people have to act on them. This engine has
/// exactly two responses available — take the agreed fallback, or stop — so it names the two, and
/// the four stay four where they are read.
#[derive(Debug, Clone, thiserror::Error)]
pub enum JudgeError {
    /// The question could never have been asked. OURS, not an outage: a second identical attempt
    /// fails identically, and the body that asked it is immutable, so a fallback here would hide a
    /// permanent defect behind a plausible answer on every run for ever.
    #[error("{0}")]
    Malformed(String),
    /// The judge could not be reached, ran out of time, refused, or answered with something this
    /// question cannot use. Retryable in principle; falls back now.
    #[error("{0}")]
    Unavailable(String),
}

/// A judge for an `ask` step. A trait for the reason `ReviewJudge` is one: the engine must be
/// testable with no key, no spend and no network, and the thing behind it lives in a crate that
/// depends on this one.
#[async_trait::async_trait]
pub trait Judge: Send + Sync {
    async fn judge(&self, ask: JudgeAsk<'_>) -> Result<Verdict, JudgeError>;
}

/// Whether this walk asks anybody.
pub enum Judging<'a> {
    Ask(&'a dyn Judge),
    /// NOTHING IS PUT TO THE CLASSIFIER AT ALL, and every `ask` step takes its agreed fallback.
    /// The sentence says why, because "switched off for this run" is a decision somebody made and
    /// "this deployment has no Jev key" is a fact about the deployment, and a person reading a
    /// strange result has to be able to tell those apart.
    Off(String),
}

// -------------------------------------------------------------------------------------------
// The walk
// -------------------------------------------------------------------------------------------

/// How a walk ended. Every one of these is a reported outcome: the engine has no path that
/// panics, and none that stops without saying so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ending {
    /// A `stop` step. The tree reached an ending it declares — which is not the same as the person
    /// getting what they wanted, since "gave up" is an ending an author writes on purpose.
    Stopped {
        outcome: String,
        say: String,
    },
    OutOfSteps {
        at: String,
        budget: usize,
    },
    OutOfTime {
        at: String,
        seconds: u64,
    },
    /// The same step, with the facts it already had, more times than the breaker allows.
    GoingInCircles {
        at: String,
        times: usize,
    },
    /// A recipe stopped short and the step said nothing about where that goes.
    RecipeFailed {
        at: String,
        recipe: String,
        why: String,
    },
    /// Something the walk cannot go on from: a question that could not be put, a recipe this run
    /// may not play, a box that would not answer.
    Broken {
        at: String,
        why: String,
    },
}

impl Ending {
    #[must_use]
    pub fn word(&self) -> &'static str {
        match self {
            Self::Stopped { .. } => "stopped",
            Self::OutOfSteps { .. } => "out-of-steps",
            Self::OutOfTime { .. } => "out-of-time",
            Self::GoingInCircles { .. } => "going-in-circles",
            Self::RecipeFailed { .. } => "recipe-failed",
            Self::Broken { .. } => "broken",
        }
    }

    /// The sentence a person reads.
    #[must_use]
    pub fn say(&self) -> String {
        match self {
            Self::Stopped { outcome, say } if say.trim().is_empty() => outcome.clone(),
            Self::Stopped { say, .. } => say.clone(),
            Self::OutOfSteps { at, budget } => format!(
                "this workflow took all {budget} of its steps and was still at \"{at}\", so it was \
                 stopped rather than left running"
            ),
            Self::OutOfTime { at, seconds } => format!(
                "this workflow ran for its whole {seconds}s and was still at \"{at}\", so no \
                 further step was started"
            ),
            Self::GoingInCircles { at, times } => format!(
                "this workflow came back to \"{at}\" {times} times with nothing new to go on, so \
                 it was stopped rather than left repeating itself"
            ),
            Self::RecipeFailed { at, recipe, why } => format!(
                "the recipe `{recipe}` at step \"{at}\" stopped short ({why}), and that step does \
                 not say where a stopped recipe goes"
            ),
            Self::Broken { at, why } => format!("step \"{at}\" could not be taken: {why}"),
        }
    }
}

/// What a walk did.
#[derive(Debug, Clone)]
pub struct Walk {
    pub ending: Ending,
    /// How many steps were actually taken.
    pub steps: usize,
    pub ms: u64,
    /// How many `ask` steps answered themselves because nobody else could.
    pub fallbacks: usize,
    /// Whether the classifier was asked anything at all on this run.
    pub asked_jev: bool,
    /// Every step, in order — the run's activity, and the only place a fallback is visible.
    pub trail: Vec<Value>,
}

impl Walk {
    /// TRUE MEANS THE TREE REACHED AN ENDING IT DECLARES, not that the person is happy. A `stop`
    /// called "gave-up" is a decision the author wrote down and the walk carried out correctly; a
    /// budget, a circle, a stopped recipe with nowhere to go and a fault are the four ways the
    /// walk ended without the tree saying so. The receipt carries `outcome` beside this, which is
    /// the word a page should show.
    #[must_use]
    pub fn ok(&self) -> bool {
        matches!(self.ending, Ending::Stopped { .. })
    }

    /// The receipt this walk is stored under, in `recipe_run.receipt`.
    #[must_use]
    pub fn receipt(&self) -> Value {
        let mut receipt = json!({
            // WHICH BODY SHAPE THIS RUN WALKED, and deliberately not called "workflow": the run
            // route merges this receipt into a reply that already says which workflow it was, and
            // a second key of that name silently replaced the id with the number 1.
            "shape": SHAPE,
            "ok": self.ok(),
            "ending": self.ending.word(),
            "say": self.ending.say(),
            "steps": self.steps,
            "ms": self.ms,
            "jev": if self.asked_jev { "asked" } else { "off" },
            // Counted as well as listed. A person opening a run that went oddly should not have to
            // read the trail to find out whether the classifier was there.
            "fallbacks": self.fallbacks,
            "trail": self.trail,
        });
        if let Ending::Stopped { outcome, .. } = &self.ending
            && let Some(object) = receipt.as_object_mut()
        {
            object.insert("outcome".to_string(), json!(outcome));
        }
        receipt
    }
}

/// Everything a walk needs that is not the tree.
pub struct Walker<'a> {
    pub computer: &'a dyn Computer,
    pub box_id: &'a str,
    /// Where a recipe's runnable steps come from, and where its run is written down. The same seam
    /// the agent's `run_recipe` uses, so a recipe played by a tree is recorded exactly like one
    /// played by a bot.
    pub recipes: &'a dyn RecipeSource,
    pub coworker: &'a CoworkerId,
    /// The recipes this walk may play, decided by the caller before the walk starts. The engine
    /// refuses anything else by name — it does not ask, because it has no way to know who is
    /// running it.
    pub allowed: &'a BTreeSet<String>,
    pub judging: Judging<'a>,
    /// The workflow's name, for the state Jev judges.
    pub name: &'a str,
}

impl Walker<'_> {
    /// Walk the tree. `bound` is already bound against the workflow's parameters — the caller does
    /// that, because a value that does not bind is a refusal to the person who typed it and should
    /// never become a run row.
    pub async fn walk(&self, workflow: &Workflow, bound: &Values) -> Walk {
        let started = Instant::now();
        let deadline = Duration::from_secs(workflow.budget.seconds);
        let mut facts: BTreeMap<String, String> = BTreeMap::new();
        let mut trail: Vec<Value> = Vec::new();
        let mut visits: BTreeMap<u64, usize> = BTreeMap::new();
        let mut at = workflow.start.clone();
        let mut taken = 0usize;
        let mut fallbacks = 0usize;
        let mut asked_jev = false;

        let ending = loop {
            if taken >= workflow.budget.steps {
                break Ending::OutOfSteps {
                    at,
                    budget: workflow.budget.steps,
                };
            }
            // READ BETWEEN STEPS, NEVER ACROSS ONE. See the module doc: cancelling a recipe call in
            // flight does not stop the box, it only loses the receipt.
            if started.elapsed() >= deadline {
                break Ending::OutOfTime {
                    at,
                    seconds: workflow.budget.seconds,
                };
            }
            let here = mark(&at, &facts);
            let times = visits.entry(here).or_insert(0);
            *times += 1;
            if *times > SAME_PLACE_ALLOWED {
                break Ending::GoingInCircles { times: *times, at };
            }
            let Some(act) = workflow.steps.get(&at) else {
                // Unreachable through `parse`, which lints every exit. Kept as an ending rather
                // than an unwrap because a body can also be handed in by a caller that built it
                // itself, and a missing step is not worth a dead process.
                break Ending::Broken {
                    why: format!("there is no step called \"{at}\""),
                    at,
                };
            };
            taken += 1;
            let mut entry = json!({ "step": at, "do": act.word() });

            let next = match act {
                Act::Stop { outcome, say } => {
                    push(&mut trail, &mut entry, json!({ "outcome": outcome }));
                    break Ending::Stopped {
                        outcome: outcome.clone(),
                        say: say.clone(),
                    };
                }
                Act::Observe {
                    fact,
                    shell,
                    seconds,
                    then,
                } => {
                    let command = match opengrok_recipes::substitute(shell, bound) {
                        Ok(command) => command,
                        Err(why) => break Ending::Broken { at, why },
                    };
                    let patience = seconds.unwrap_or(PROBE_SECONDS).clamp(1, PROBE_SECONDS_MAX);
                    match self.computer.run(self.box_id, &command, patience).await {
                        Ok(output) => {
                            let seen = clip(output.stdout.trim(), FACT_CHARS);
                            // WHAT WAS SEEN IS NOT WRITTEN DOWN, ON PURPOSE. The fact feeds this
                            // walk's branches and the state Jev judges, and then it is gone: a
                            // workflow can be shared, its runs are readable by the person it was
                            // shared FROM, and a probe whose output landed in the receipt would
                            // make a shared workflow a way to read a colleague's screen back to
                            // its author. The command, the exit code and how much came back are
                            // enough to debug a probe that found nothing.
                            let chars = seen.chars().count();
                            facts.insert(fact.clone(), seen);
                            facts.insert(format!("{fact}.exit"), output.exit_code.to_string());
                            push(
                                &mut trail,
                                &mut entry,
                                json!({
                                    "as": fact,
                                    "shell": command,
                                    "exit": output.exit_code,
                                    "chars": chars,
                                    "went": then,
                                }),
                            );
                            then.clone()
                        }
                        Err(error) => {
                            break Ending::Broken {
                                at,
                                why: format!("looking at the box failed: {error}"),
                            };
                        }
                    }
                }
                Act::When {
                    fact,
                    test,
                    yes,
                    no,
                } => {
                    let value = facts.get(fact).map(String::as_str).unwrap_or("").trim();
                    let held = match test {
                        Test::Is(want) => match opengrok_recipes::substitute(want, bound) {
                            Ok(want) => value == want.trim(),
                            Err(why) => break Ending::Broken { at, why },
                        },
                        Test::Contains(part) => match opengrok_recipes::substitute(part, bound) {
                            Ok(part) => value.contains(&part),
                            Err(why) => break Ending::Broken { at, why },
                        },
                        Test::Empty => value.is_empty(),
                    };
                    let went = if held { yes } else { no };
                    push(
                        &mut trail,
                        &mut entry,
                        json!({ "fact": fact, "test": test.word(), "held": held, "went": went }),
                    );
                    went.clone()
                }
                Act::Run {
                    recipe,
                    values,
                    then,
                    otherwise,
                } => {
                    if !self.allowed.contains(recipe) {
                        break Ending::Broken {
                            at,
                            why: format!("recipe `{recipe}` is not one this run may play"),
                        };
                    }
                    let mut filled = Values::new();
                    let mut bad = None;
                    for (key, value) in values {
                        match opengrok_recipes::substitute(value, bound) {
                            Ok(value) => {
                                filled.insert(key.clone(), value);
                            }
                            Err(why) => {
                                bad = Some(format!(
                                    "the value for `{key}` could not be filled in: {why}"
                                ));
                                break;
                            }
                        }
                    }
                    if let Some(why) = bad {
                        break Ending::Broken { at, why };
                    }
                    let (version, request) =
                        match self.recipes.recipe_request(recipe, &filled).await {
                            Ok(found) => found,
                            Err(why) => break Ending::Broken { at, why },
                        };
                    let raw = match self.computer.run_recipe(self.box_id, &request).await {
                        Ok(raw) => raw,
                        Err(error) => {
                            // A BOX THAT WOULD NOT ANSWER IS NOT A RECIPE THAT FAILED. `otherwise`
                            // means "it played and did not get there"; a machine out of reach is a
                            // state, not a verdict about the task, and sending it down the same
                            // branch would have the tree acting on a judgement nobody made.
                            break Ending::Broken {
                                at,
                                why: format!("the box would not play `{recipe}`: {error}"),
                            };
                        }
                    };
                    let receipt = RecipeReceipt::from_value(raw);
                    let run_id = self
                        .recipes
                        .record_run(recipe, version, self.coworker, &receipt)
                        .await;
                    facts.insert("last.recipe".to_string(), recipe.clone());
                    facts.insert("last.ok".to_string(), receipt.ok.to_string());
                    facts.insert("last.ran".to_string(), receipt.ran.to_string());
                    facts.insert(
                        "last.stopped_at".to_string(),
                        receipt
                            .stopped_at
                            .map(|n| n.to_string())
                            .unwrap_or_default(),
                    );
                    facts.insert(
                        "last.error".to_string(),
                        clip(receipt.error.as_deref().unwrap_or(""), FACT_CHARS),
                    );
                    let went = if receipt.ok {
                        Some(then.clone())
                    } else {
                        otherwise.clone()
                    };
                    push(
                        &mut trail,
                        &mut entry,
                        json!({
                            "recipe": recipe,
                            "version": version,
                            "ok": receipt.ok,
                            "ran": receipt.ran,
                            "stoppedAt": receipt.stopped_at,
                            "error": receipt.error,
                            "runId": run_id,
                            "went": went,
                        }),
                    );
                    match went {
                        Some(went) => went,
                        None => {
                            break Ending::RecipeFailed {
                                at,
                                recipe: recipe.clone(),
                                why: receipt
                                    .error
                                    .clone()
                                    .unwrap_or_else(|| "a step failed".to_string()),
                            };
                        }
                    }
                }
                Act::Ask {
                    name,
                    question,
                    yes,
                    no,
                    go,
                } => {
                    let instructions =
                        match opengrok_recipes::substitute(question.instructions(), bound) {
                            Ok(text) => text,
                            Err(why) => break Ending::Broken { at, why },
                        };
                    let question = question.with_instructions(instructions);
                    let state = self.state_of(&at, &facts, &trail);
                    let asked = match &self.judging {
                        Judging::Off(because) => Err(because.clone()),
                        Judging::Ask(judge) => {
                            asked_jev = true;
                            match judge
                                .judge(JudgeAsk {
                                    state: &state,
                                    name,
                                    question: &question,
                                })
                                .await
                            {
                                Ok(verdict) => Ok(verdict),
                                // A MALFORMED QUESTION IS NOT AN OUTAGE AND IS NOT FALLEN BACK
                                // FROM. It is our bug — the body asked something that could never
                                // be put — and because the body is immutable it will be our bug on
                                // every run until somebody writes a new version. Answering it "no"
                                // would make a permanent defect read as a cautious decision.
                                Err(JudgeError::Malformed(why)) => {
                                    break Ending::Broken {
                                        at,
                                        why: format!(
                                            "the question \"{name}\" could not be put to Jev: \
                                             {why}"
                                        ),
                                    };
                                }
                                Err(JudgeError::Unavailable(why)) => Err(why),
                            }
                        }
                    };
                    let (answer, confidence, mut because) = match asked {
                        Ok(verdict) => (verdict.answer, Some(verdict.confidence), None),
                        Err(why) => match question.fallback() {
                            Some((answer, rule)) => (answer, None, Some((why, rule))),
                            None => {
                                break Ending::Broken {
                                    at,
                                    why: format!(
                                        "the question \"{name}\" has no options to fall back to"
                                    ),
                                };
                            }
                        },
                    };
                    let mut answer = answer;
                    let mut went =
                        branch_for(&question, &answer, yes.as_deref(), no.as_deref(), go);
                    if went.is_none() && because.is_none() {
                        // Jev answered something this question never offered. Upstream's problem
                        // rather than a malformed question, so it lands on the fallback with the
                        // reason recorded, exactly as an outage does.
                        if let Some((fallback, rule)) = question.fallback() {
                            because = Some((
                                format!(
                                    "Jev answered \"{answer}\", which is not one of the answers \
                                     this question offered"
                                ),
                                rule,
                            ));
                            went =
                                branch_for(&question, &fallback, yes.as_deref(), no.as_deref(), go);
                            answer = fallback;
                        }
                    }
                    let Some(went) = went else {
                        break Ending::Broken {
                            at,
                            why: format!("step does not say where the answer \"{answer}\" goes"),
                        };
                    };
                    let mut said = json!({
                        "question": name,
                        "kind": question.kind_word(),
                        "answer": answer,
                        "confidence": confidence,
                        "went": went,
                    });
                    if let Some((why, rule)) = &because {
                        fallbacks += 1;
                        // EVERY FALLBACK IS WRITTEN INTO THE RUN, both here and in the log. A
                        // person reading a strange result has to be able to see that the model was
                        // absent rather than wrong, and a run whose answers were all defaults
                        // looks exactly like a run whose answers were all considered.
                        tracing::warn!(
                            step = %at,
                            question = %name,
                            answer = %answer,
                            why = %why,
                            "a workflow answered its own question: Jev did not"
                        );
                        if let Some(object) = said.as_object_mut() {
                            object.insert(
                                "fallback".to_string(),
                                json!({ "because": why, "rule": rule }),
                            );
                        }
                    }
                    push(&mut trail, &mut entry, said);
                    went
                }
            };
            at = next;
        };

        // The step that ended the walk is in the trail for every ending but the ones that never
        // got to take it; those name the step they were at in the ending itself.
        Walk {
            ending,
            steps: taken,
            ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            fallbacks,
            asked_jev,
            trail,
        }
    }

    /// What Jev judges: where the walk is, what it has gathered, and the last few things it did.
    ///
    /// AN OBJECT, NEVER A BARE STRING OR NUMBER — Jev's content type refuses those, and a state
    /// that cannot be sent is a malformed question rather than an outage.
    fn state_of(&self, at: &str, facts: &BTreeMap<String, String>, trail: &[Value]) -> Value {
        /// Enough of the trail to say what has been tried, few enough that a long walk does not
        /// send its whole history on every question.
        const RECENT: usize = 8;
        json!({
            "workflow": self.name,
            "at": at,
            "facts": facts,
            "did": trail.iter().rev().take(RECENT).rev().collect::<Vec<_>>(),
        })
    }
}

/// Which exit an answer takes.
fn branch_for(
    question: &Question,
    answer: &str,
    yes: Option<&str>,
    no: Option<&str>,
    go: &BTreeMap<String, String>,
) -> Option<String> {
    match question {
        Question::Noul { .. } => {
            if answer == "yes" {
                yes.map(str::to_string)
            } else if answer == "no" {
                no.map(str::to_string)
            } else {
                None
            }
        }
        Question::Choice { .. } | Question::Score { .. } => go.get(answer).cloned(),
    }
}

/// Merge the kind-specific fields into the step's trail entry and file it.
fn push(trail: &mut Vec<Value>, entry: &mut Value, extra: Value) {
    if let (Some(entry), Some(extra)) = (entry.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            entry.insert(key.clone(), value.clone());
        }
    }
    trail.push(entry.clone());
}

/// Where the walk is, as one number. Hashed rather than kept whole because the facts can be a
/// couple of kilobytes each and this is remembered once per step.
fn mark(at: &str, facts: &BTreeMap<String, String>) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    at.hash(&mut hasher);
    for (key, value) in facts {
        key.hash(&mut hasher);
        value.hash(&mut hasher);
    }
    hasher.finish()
}

/// Clip on a char boundary and SAY SO. A silently clipped probe is a tree deciding on half an
/// answer while believing it has the whole one.
fn clip(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept} …[clipped {} chars]", count - max)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
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
        async fn run_recipe(&self, _box_id: &str, request: &Value) -> BoxResult<Value> {
            self.commands
                .lock()
                .unwrap()
                .push(format!("recipe {}", request["name"]));
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
            recipes,
            coworker: &bot,
            allowed: &allowed,
            judging,
            name: "Search once",
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
        let down =
            || StubJudge::failing(JudgeError::Unavailable("Jev is unreachable: x".to_string()));

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
        let judge =
            StubJudge::failing(JudgeError::Malformed("a rubric with no levels".to_string()));
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
}
