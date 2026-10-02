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
//! - **What the box saw while that recipe played.** Box PR #29 made a receipt carry the window
//!   under each pointer step, where the keystrokes were about to go, and — when asked for it —
//!   the page URL either side of a step. Those arrive as four more facts, `last.observe`,
//!   `last.targets`, `last.focus` and `last.urls`, and no step kind had to change for them:
//!   `when` tests them like any other. `crate::observe` has what they mean and what they cost.
//!
//! And nothing else. In particular the engine CANNOT see the screen: `Computer::screenshot`
//! returns a PNG, Jev judges text and objects, and there is no vision model on this path — so a
//! question phrased as "does the search box look wrong" is answered from the facts a probe
//! gathered, never from the picture.
//!
//! WHAT THE BOX SAW IS NOT WRITTEN INTO THE TRAIL, for the same reason a probe's output is not:
//! see the `observe` step below. The trail says the level the box ran at and how many steps it
//! looked at, which is enough to debug a tree that is deciding on nothing, and says none of what
//! was on the screen.
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
//!    rather than firing the first time round. The observation facts narrow that blind spot
//!    without closing it: two replays onto two different windows are now two different places
//!    even where the receipts match, but a screen that got worse in a way nothing was looking at
//!    still reads as standing still, and the bound is what catches it.
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
/// write. The facts now include what the box saw, so two replays that landed on different windows
/// no longer count as the same place; that makes the bound bite later, not never.
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
        /// How much of the desktop this one run asks the box to report back, when the walk's own
        /// level is not enough. Absent means the walk's level, which is what every body written
        /// before this field existed carries — and those bodies are immutable, so absent has to
        /// keep meaning "whatever this deployment does".
        ///
        /// THIS IS WHERE `page` IS AFFORDABLE. It costs two loopback reads either side of every
        /// navigating step, so it is not what a bot's every run pays; a tree that has to know
        /// whether a page actually moved is asking a question nothing cheaper answers, and it is
        /// asking it of one step rather than of every recipe on the deployment.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        observe: Option<crate::observe::Observe>,
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
    /// How much of the desktop this walk's recipe runs ask the box to report back, unless a `run`
    /// step asks for something else. Decided by the caller, not read here, so a walk runs at one
    /// level from start to finish however long it takes.
    pub observe: crate::observe::Observe,
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
                    observe,
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
                    let (version, mut request) =
                        match self.recipes.recipe_request(recipe, &filled).await {
                            Ok(found) => found,
                            Err(why) => break Ending::Broken { at, why },
                        };
                    let level = observe.unwrap_or(self.observe);
                    crate::observe::ask(&mut request, level);
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
                    // No claim per step: the workflow route holds the bot's lease for the whole
                    // walk (`begin_run`), so a step's own claim would be refused by its own walk.
                    let run_id = self
                        .recipes
                        .record_run(recipe, version, self.coworker, &receipt, None)
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
                    // WHAT THE BOX SAW, AS FACTS A `when` CAN TEST. `last.ok` is the fact that
                    // lied through twenty-five runs: it says no step threw, and a tape whose
                    // coordinates have drifted onto another window throws nothing at all. These
                    // four say what was actually under the pointer, where the keys went and which
                    // pages were on screen, so a tree can branch on the desktop rather than on the
                    // absence of an exception.
                    //
                    // ALWAYS WRITTEN, EVEN EMPTY. A fact left over from the previous `run` step
                    // would have this branch deciding on the last recipe but one. `last.observe`
                    // is what separates "the box looked and saw nothing" from "the box was never
                    // asked, or is too old to know how": empty there means nobody looked, and a
                    // tree that cares can test it before it trusts the other three.
                    let seen = crate::observe::Seen::read(&receipt.raw);
                    facts.insert(
                        "last.observe".to_string(),
                        seen.as_ref()
                            .map(|seen| seen.mode.word())
                            .unwrap_or_default()
                            .to_string(),
                    );
                    facts.insert(
                        "last.targets".to_string(),
                        clip(
                            &seen
                                .as_ref()
                                .map(crate::observe::Seen::target_fact)
                                .unwrap_or_default(),
                            FACT_CHARS,
                        ),
                    );
                    facts.insert(
                        "last.focus".to_string(),
                        clip(
                            &seen
                                .as_ref()
                                .map(crate::observe::Seen::focus_fact)
                                .unwrap_or_default(),
                            FACT_CHARS,
                        ),
                    );
                    facts.insert(
                        "last.urls".to_string(),
                        clip(
                            &seen
                                .as_ref()
                                .map(crate::observe::Seen::page_fact)
                                .unwrap_or_default(),
                            FACT_CHARS,
                        ),
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
                            // The level and the count, never the reading: a walk's trail is
                            // stored and read back by everyone the workflow was shared to, and
                            // window titles are somebody's screen. Same rule as the `observe`
                            // step's output, and enough to tell a tree that decided on nothing
                            // from one that decided on something.
                            "observe": level.word(),
                            "observed": seen.as_ref().map(|seen| seen.looked_at).unwrap_or(0),
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
#[path = "../tests/unit/workflow.rs"]
mod tests;
