//! What the box saw while a recipe played: asking for it, reading it back, and what may be kept.
//!
//! `ok: true` on a recipe receipt means "no step returned an error", and that is the only thing it
//! has ever meant. On 16 Sep 2026 a bot replayed one taught recipe twenty-five times in a row: the
//! run history shows eight steps played every time, `ok: true` every time, `stopped_at: null` every
//! time. What was on the screen after the second run was a search box holding the term twice over
//! and a search that was never submitted. The model deciding whether to try again had nothing to
//! decide on, because the only thing it was told was that nothing had thrown.
//!
//! hexuria/box#29 gave the box something to say about that, behind a request field: `observe`.
//! This module is the wire between the two halves — the server asks for it, and puts what came
//! back where a model can read it. It deliberately does NOT interpret it. The box can report the
//! window under a click; it cannot know what the recipe was *for*, and neither can this server,
//! so nothing here turns an observation into a verdict. A sentence a model can draw a conclusion
//! from is the whole product; a conclusion drawn here would be the same confident guess that made
//! `ok: true` read as "the task is done".
//!
//! # What it costs
//!
//! `input` is X11 requests on a connection the box already holds open: about ten round trips for a
//! pointer step and about four for a `type`, capped by the box at 400ms per observation. A step
//! already pays more than that in pacing — the box's own `CLICK_GAP` is 12ms and `TYPE_CHAR_MS` is
//! 30ms per character — so it is bounded well under the timing gap the click itself takes.
//!
//! `page` adds two DevTools reads either side of each step that can navigate, each capped at
//! 250ms. On a 256-step recipe (the box's maximum) that is up to 512 reads, and the ceiling is
//! minutes rather than milliseconds. It is therefore not a default: `input` is what a bot's run
//! asks for, and `page` is opted into by whoever is willing to pay for it — the operator through
//! `OG_RECIPE_OBSERVE`, or one `run` step of a workflow that needs to know whether a page moved.
//!
//! Both numbers are the box's own, from the PR that added them; the box is Linux/X11 and this
//! crate is not, so nothing here has measured them.

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Which level a bot's recipe run asks for, when the deployment wants something other than the
/// default. One of the box's own three words; anything else is ignored with a warning, because a
/// typo must not silently buy a level nobody chose.
const LEVEL_ENV: &str = "OG_RECIPE_OBSERVE";

/// A window word long enough to tell two windows apart, short enough that a recipe that touched
/// six of them does not push a page of title text into a tool result.
const WORD_CHARS: usize = 160;
/// URLs are unbounded in principle and enormous in practice. Enough to see which page it is.
const URL_CHARS: usize = 200;
/// How many step numbers one clause names before it starts counting instead.
const STEPS_LISTED: usize = 6;
/// How many distinct windows a sentence names. A recipe that touched more than this has a bigger
/// problem than the sentence can describe.
const GROUPS_LISTED: usize = 6;
/// How many pages a sentence walks through before it counts the rest.
const PAGES_LISTED: usize = 8;

/// The word a fact uses for "the box looked here and could name nothing".
///
/// Not `none`, which is one of the box's own focus states and means something else — a workflow
/// testing `contains: "none"` must not match both.
const NOTHING_NAMED: &str = "(none)";
/// The word a fact uses for "the box looked and got no answer at all".
const NOT_READ: &str = "(unread)";

/// How much of the desktop the box is asked to report on, in the box's own three words.
///
/// Transcribed from `box-cua`'s `RecipeObserve` rather than invented: this value is serialised
/// straight into the request body, so a fourth word here would be a request the box refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Observe {
    /// Ask for nothing. The receipt is byte-identical to the one this endpoint produced before
    /// the field existed, which is why it is the box's default and not ours.
    Off,
    /// The window under each pointer step's target, and where the keys were about to go before
    /// each `type` or `key`.
    Input,
    /// Everything `input` reports, and the page URL either side of the steps that can navigate.
    Page,
}

impl Observe {
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Input => "input",
            Self::Page => "page",
        }
    }

    /// The box's three words and nothing else. `None` for anything unrecognised, so a caller
    /// decides what an unknown word means rather than being handed a guess.
    #[must_use]
    pub fn read(word: &str) -> Option<Self> {
        match word.trim() {
            "off" => Some(Self::Off),
            "input" => Some(Self::Input),
            "page" => Some(Self::Page),
            _ => None,
        }
    }
}

/// The level a recipe played for a bot asks for: `input`, unless `OG_RECIPE_OBSERVE` says
/// otherwise.
///
/// WHY `input` AND NOT `off`. A run nobody watched is the run that produced the twenty-five-run
/// incident, and `input` is the level that costs less than the pacing the step already pays (see
/// the module note). It catches the class of failure a fixed tape has: a taped coordinate whose
/// window has moved on, and keystrokes sent at a desktop where nothing holds the focus.
///
/// WHY NOT `page`. It is the level that would have shown *that* run's symptom — a Return that
/// submitted nothing leaves the URL where it was — but it is two loopback HTTP reads per
/// navigating step, and a recipe may have 256 of them. Paying that on every run of every recipe
/// to catch a failure the model can also see in the screenshot is the wrong trade; paying it
/// deliberately is not, which is what the env var and a workflow's per-step field are for.
///
/// READ ONCE PER PROCESS, NEVER PER REQUEST. Same habit as `OG_JEV_MODEL`: a deployment's
/// configuration is what it was at boot, so a run cannot be quietly reconfigured under a person
/// halfway through their day by an edit to an environment nobody restarted.
#[must_use]
pub fn wanted() -> Observe {
    static LEVEL: OnceLock<Observe> = OnceLock::new();
    *LEVEL.get_or_init(|| match std::env::var(LEVEL_ENV) {
        Ok(word) => match Observe::read(&word) {
            Some(level) => level,
            None => {
                tracing::warn!(
                    value = %word,
                    "{LEVEL_ENV} is not one of off/input/page; recipe runs will observe input"
                );
                Observe::Input
            }
        },
        Err(_) => Observe::Input,
    })
}

/// Ask the box, on this request body, to report what it sees.
///
/// `off` is left off the body entirely rather than written as `"off"`. The box's own default is
/// off, so a body that never mentions the field is the one every box that has ever shipped was
/// tested against — including the ones too old to know the field exists.
pub fn ask(request: &mut Value, level: Observe) {
    if level == Observe::Off {
        return;
    }
    if let Some(object) = request.as_object_mut() {
        object.insert(
            "observe".to_string(),
            Value::String(level.word().to_string()),
        );
    }
}

/// One window a pointer step aimed at, and the steps that aimed there.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Where {
    /// `None` where the box named no window: either nothing but the root was mapped at that
    /// coordinate, or the X server did not answer inside the box's deadline. The receipt cannot
    /// tell those apart, so neither does this.
    window: Option<String>,
    steps: Vec<usize>,
}

/// Where the keys were about to go, and the steps that sent them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Keys {
    /// The box's own focus word — `none`, `pointer_root`, `root`, `window`. `None` means the box
    /// looked and got no answer, which is not the same as `Some("none")`: that one is an answer,
    /// and the answer is that a keystroke would have reached nothing.
    state: Option<String>,
    window: Option<String>,
    steps: Vec<usize>,
}

/// What a receipt says the box saw, read once so every reader of it agrees.
///
/// Absent facts stay absent. Nothing here fills a gap with a plausible value, because the
/// difference between "the box looked and saw nothing" and "the box did not look" is the whole
/// reason the box echoes the level it was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    /// The level the box says it ran at, echoed from the request.
    pub mode: Observe,
    /// Steps in the receipt, whether observed or not.
    pub steps: usize,
    /// Steps that carried an observation. Lower than `steps` for any recipe with a `wait` in it:
    /// a step with no coordinate and no keys has nothing to look at.
    pub looked_at: usize,
    /// What the looking cost, summed over the steps. The box keeps this out of each step's `ms`
    /// on purpose, so this is the added time and not a re-count of the run.
    pub observe_ms: u64,
    targets: Vec<Where>,
    keys: Vec<Keys>,
    /// The pages, in the order they were seen, with consecutive repeats collapsed. Run-length
    /// rather than distinct: A → B → A is a page that came back, and folding it to A, B would
    /// hide the coming back.
    pages: Vec<String>,
}

impl Seen {
    /// Read a box receipt. `None` when the box did not say it was looking — an `observe` it never
    /// echoed, a level of `off`, or a box old enough not to know the field. In every one of those
    /// cases the caller's words must stay exactly what they were before this existed, because
    /// there is nothing to report and saying so anyway is noise on every run forever.
    #[must_use]
    pub fn read(receipt: &Value) -> Option<Self> {
        let mode = receipt
            .get("observe")
            .and_then(Value::as_str)
            .and_then(Observe::read)?;
        if mode == Observe::Off {
            return None;
        }
        let steps = receipt.get("steps").and_then(Value::as_array)?;
        let mut seen = Self {
            mode,
            steps: steps.len(),
            looked_at: 0,
            observe_ms: 0,
            targets: Vec::new(),
            keys: Vec::new(),
            pages: Vec::new(),
        };
        for (position, step) in steps.iter().enumerate() {
            let Some(observed) = step.get("observed") else {
                continue;
            };
            seen.looked_at += 1;
            seen.observe_ms = seen.observe_ms.saturating_add(
                observed
                    .get("observe_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            );
            // The box's own index, so "step 3" here is the step 3 a stop reports and the step 3
            // the history's artifacts are filed under. Its position in the array is the fallback
            // and is the same number for every receipt the box actually writes.
            let index = step
                .get("index")
                .and_then(Value::as_u64)
                .map_or(position, |n| n as usize);
            let keyboard = match step.get("op").and_then(Value::as_str).unwrap_or("") {
                "type" | "key" => true,
                // A receipt with no `op` at all is not one the box writes. Fall back to what the
                // block holds rather than guessing a kind: a focus reading only ever belongs to a
                // step that was about to send keys.
                "" => observed.get("focus").is_some(),
                _ => false,
            };
            if keyboard {
                let focus = observed.get("focus");
                let state = focus
                    .and_then(|focus| focus.get("state"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let window = focus
                    .and_then(|focus| focus.get("window"))
                    .and_then(window_word);
                match seen
                    .keys
                    .iter_mut()
                    .find(|group| group.state == state && group.window == window)
                {
                    Some(group) => group.steps.push(index),
                    None => seen.keys.push(Keys {
                        state,
                        window,
                        steps: vec![index],
                    }),
                }
            } else {
                let window = observed.get("target").and_then(window_word);
                match seen.targets.iter_mut().find(|group| group.window == window) {
                    Some(group) => group.steps.push(index),
                    None => seen.targets.push(Where {
                        window,
                        steps: vec![index],
                    }),
                }
            }
            for field in ["url_before", "url_after"] {
                let Some(url) = observed
                    .get(field)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|url| !url.is_empty())
                else {
                    continue;
                };
                let url = clip(url, URL_CHARS);
                if seen.pages.last() != Some(&url) {
                    seen.pages.push(url);
                }
            }
        }
        Some(seen)
    }

    /// What the box saw, in words for a model — facts, in the order the run produced them, and no
    /// judgement about any of them.
    ///
    /// NOT A VERDICT, AND THE LEAD-IN SAYS SO. A model that is told "the click landed on the
    /// terminal" can work out that its Gmail recipe did not run; a model that is told "the recipe
    /// failed" has been handed a conclusion this server has no way to reach — it does not know
    /// what the recipe was for. The distinction is the entire point of the box's observations, and
    /// a summary is exactly where it gets lost, so everything here reports a reading and nothing
    /// here scores one.
    #[must_use]
    pub fn sentence(&self) -> String {
        let lead = format!(
            "the box also watched while it played, and reports what was on the screen — not \
             whether this achieved anything (observe: {})",
            self.mode.word()
        );
        if self.looked_at == 0 {
            return format!("{lead}: no step had anything for it to look at.");
        }
        let mut clauses = vec![format!(
            "{} of {} steps observed",
            self.looked_at, self.steps
        )];
        for group in self.targets.iter().take(GROUPS_LISTED) {
            let at = steps_word(&group.steps);
            clauses.push(match &group.window {
                Some(window) => format!("pointer steps landed on {window} (at {at})"),
                None => format!(
                    "no window the box could name was under the pointer (at {at}) — nothing was \
                     mapped there, or the X server did not answer"
                ),
            });
        }
        if let Some(more) = self
            .targets
            .len()
            .checked_sub(GROUPS_LISTED)
            .filter(|n| *n > 0)
        {
            clauses.push(format!("and {more} further windows under the pointer"));
        }
        for group in self.keys.iter().take(GROUPS_LISTED) {
            clauses.push(format!(
                "{} (at {})",
                keys_word(group),
                steps_word(&group.steps)
            ));
        }
        if let Some(more) = self
            .keys
            .len()
            .checked_sub(GROUPS_LISTED)
            .filter(|n| *n > 0)
        {
            clauses.push(format!("and {more} further places the keys went"));
        }
        if let Some((first, rest)) = self.pages.split_first() {
            clauses.push(if rest.is_empty() {
                format!("the page stayed on {first} throughout")
            } else {
                let walked = self
                    .pages
                    .iter()
                    .take(PAGES_LISTED)
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(" → ");
                match self
                    .pages
                    .len()
                    .checked_sub(PAGES_LISTED)
                    .filter(|n| *n > 0)
                {
                    Some(more) => format!("the page went {walked} → and {more} more"),
                    None => format!("the page went {walked}"),
                }
            });
        }
        // SAID OUT LOUD, EVERY TIME. Observing is time a recipe spends not acting, and a cost
        // that only appears in a config file is one nobody ever notices they are paying.
        clauses.push(format!("the looking cost {}ms", self.observe_ms));
        format!("{lead}: {}.", clauses.join("; "))
    }

    /// The distinct windows the pointer steps aimed at, newline-separated, for a workflow fact.
    ///
    /// Windows and not step numbers: a `when` tests what a fact contains, and "did any click land
    /// somewhere that is not Chrome" is the question a tree has. Which step it was is in the
    /// model's sentence, where there is a reader who can use it.
    #[must_use]
    pub fn target_fact(&self) -> String {
        joined(self.targets.iter().map(|group| {
            group
                .window
                .clone()
                .unwrap_or_else(|| NOTHING_NAMED.to_string())
        }))
    }

    /// The distinct focus states the keys met, newline-separated, for a workflow fact.
    ///
    /// The box's own state words rather than this module's sentences, because a tree tests
    /// `contains: "none"` and that has to keep meaning what the box meant by it.
    #[must_use]
    pub fn focus_fact(&self) -> String {
        joined(
            self.keys
                .iter()
                .map(|group| group.state.clone().unwrap_or_else(|| NOT_READ.to_string())),
        )
    }

    /// The pages, in the order they were seen, newline-separated, for a workflow fact. Empty
    /// unless the run asked for `page`.
    ///
    /// Not deduplicated, unlike the other two: consecutive repeats are already collapsed, and a
    /// page that came back is a different journey from a page that never left.
    #[must_use]
    pub fn page_fact(&self) -> String {
        self.pages.join("\n")
    }
}

/// Take what the box saw out of a receipt before it is written down, keeping only what it cost.
///
/// WINDOW TITLES AND PAGE URLS ARE SOMEBODY'S SCREEN. A recipe's run history is handed back to
/// everyone who may read the recipe — its owner and everyone it was shared to — but a run happens
/// on the box of whoever played it. Keeping the observations would turn a shared recipe into a way
/// to read a colleague's window titles and browsing back to its author, which is the same leak the
/// workflow engine's shell probe already refuses to write down. An observation belongs to the run
/// that asked for it: the model's own result, and that walk's own facts. It is not history.
///
/// The total `observe_ms` stays, and is hoisted to the top of the receipt. The cost of observing
/// has to be measurable from a run somebody already has rather than taken on trust from a comment,
/// and a number of milliseconds is nobody's screen.
pub fn strip_observations(receipt: &mut Value) {
    let Some(object) = receipt.as_object_mut() else {
        return;
    };
    let mut cost = 0u64;
    let mut looked = false;
    if let Some(steps) = object.get_mut("steps").and_then(Value::as_array_mut) {
        for step in steps.iter_mut() {
            let Some(step) = step.as_object_mut() else {
                continue;
            };
            if let Some(observed) = step.remove("observed") {
                looked = true;
                cost = cost.saturating_add(
                    observed
                        .get("observe_ms")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                );
            }
        }
    }
    if looked {
        object.insert("observe_ms".to_string(), Value::from(cost));
    }
}

/// One window in the words the receipt used for it: `instance.class "the title"`.
///
/// `None` when the block named the window in no way at all, which reads the same as no block: the
/// box could not tell the caller which window this was.
fn window_word(window: &Value) -> Option<String> {
    let text = |field| {
        window
            .get(field)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|found| !found.is_empty())
    };
    let word = match (text("class"), text("title")) {
        (Some(class), Some(title)) => format!("{class} \"{title}\""),
        (Some(class), None) => class.to_string(),
        (None, Some(title)) => format!("\"{title}\""),
        // A window that claimed neither a class nor a name is still a window, and its id is what
        // `GET /v1/desktop/windows` would call it, so a person can go and look.
        (None, None) => format!("window {}", text("id")?),
    };
    Some(clip(&word, WORD_CHARS))
}

/// Where a keystroke sent at that moment would have gone, in words.
fn keys_word(group: &Keys) -> String {
    match (group.state.as_deref(), group.window.as_deref()) {
        (Some("window"), Some(window)) => format!("keys went to {window}"),
        (Some("window"), None) => "keys went to a window the box could not name".to_string(),
        (Some("pointer_root"), Some(window)) => {
            format!("keys followed the pointer, which was over {window}")
        }
        (Some("pointer_root"), None) => {
            "keys followed the pointer, which was over no window".to_string()
        }
        (Some("none"), _) => "keys had nowhere to go: no window held the focus".to_string(),
        (Some("root"), _) => {
            "keys went to the root window, which nothing on this desktop reads".to_string()
        }
        // A state this server has not been taught is passed through in the box's own word rather
        // than flattened into one of the four above. Guessing which of them a new word resembles
        // is how a receipt starts saying something the box never said.
        (Some(other), Some(window)) => {
            format!("keys were about to go to {window} (focus: {other})")
        }
        (Some(other), None) => format!("the box read the focus as \"{other}\""),
        (None, _) => "the box looked for the keyboard focus and got no answer".to_string(),
    }
}

/// `step 3`, or `steps 0, 1, 3`, or `steps 0, 1, 2, 3, 4, 5 and 9 more`.
fn steps_word(steps: &[usize]) -> String {
    let shown = steps
        .iter()
        .take(STEPS_LISTED)
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    match steps.len().checked_sub(STEPS_LISTED).filter(|n| *n > 0) {
        Some(more) => format!("steps {shown} and {more} more"),
        None if steps.len() == 1 => format!("step {shown}"),
        None => format!("steps {shown}"),
    }
}

/// Distinct, in first-seen order, one per line. A fact is read by `contains`, so the separator
/// only has to be something a window title or a URL will not contain.
fn joined(words: impl Iterator<Item = String>) -> String {
    let mut kept: Vec<String> = Vec::new();
    for word in words {
        if !kept.contains(&word) {
            kept.push(word);
        }
    }
    kept.join("\n")
}

/// Clip on a char boundary and say so, rather than cutting a title off mid-word and leaving a
/// reader to wonder whether that is what the window is called.
fn clip(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}…[clipped {} chars]", count - max)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#[path = "../tests/unit/observe.rs"]
mod tests;
