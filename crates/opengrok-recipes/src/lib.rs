//! From a taught tape to a recipe the box will run.
//!
//! A tape is what the screen window records while a person teaches a task: every pointer and
//! key event on the noVNC canvas, in the screen's own pixels. A recipe is what hexuria/box's
//! `POST /v1/cua/recipe` runs in one call: clicks, drags, typed text, key presses, scrolls and
//! waits. `filter` is the one-way road between them — pure, so the server route and the tests
//! agree, and so a recipe the registry holds is one the box will accept (`lint`).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// What a recipe needs from whoever runs it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Parameter {
    pub name: String,
    pub description: String,
    pub required: bool,
    pub kind: ParameterKind,
    pub default: Option<String>,
    pub values: Option<Vec<String>>,
}

/// The type of a parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParameterKind {
    Text,
    Number,
    Boolean,
}

/// Values a run supplies, by parameter name.
pub type Values = BTreeMap<String, String>;

/// Check that a parameter name matches the required pattern.
fn is_valid_param_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Check the values against the declaration and fill in defaults. The error names the
/// parameter and says what is wrong, because this sentence is shown to a person.
/// Check a DECLARATION on its own, with no values in sight.
///
/// Declaring and running are two different moments and must be judged differently. Binding asks
/// "are these values good enough to run?", and a required parameter with nothing supplied fails
/// that question by design. Declaring asks only "is this a sensible thing to ask for?" — running
/// the run-time check here made it impossible to declare a required parameter at all, because
/// the declaration was tested against an empty set of values it was never meant to have.
pub fn check(params: &[Parameter]) -> Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    for param in params {
        if !is_valid_param_name(&param.name) {
            return Err(format!(
                "parameter name '{}' must contain only lowercase letters, digits, and underscores",
                param.name
            ));
        }
        if !seen.insert(param.name.as_str()) {
            return Err(format!("parameter '{}' is declared twice", param.name));
        }
        if let Some(allowed) = &param.values
            && allowed.is_empty()
        {
            return Err(format!(
                "parameter '{}' allows no values at all, so nothing could ever be given for it",
                param.name
            ));
        }
        // A default is a value, so it is held to what a value must be — otherwise a recipe can
        // be declared today and refuse every run tomorrow for a reason nobody typed.
        if let Some(default) = &param.default {
            let mut just_the_default = Values::new();
            just_the_default.insert(param.name.clone(), default.clone());
            let only_this = [param.clone()];
            bind(&only_this, &just_the_default)
                .map_err(|why| format!("the default for '{}' is not allowed: {why}", param.name))?;
        }
    }
    Ok(())
}

pub fn bind(params: &[Parameter], given: &Values) -> Result<Values, String> {
    let mut bound = given.clone();

    for param in params {
        if !is_valid_param_name(&param.name) {
            return Err(format!(
                "parameter name '{}' must contain only lowercase letters, digits, and underscores",
                param.name
            ));
        }

        match bound.get(&param.name) {
            Some(value) => {
                // Validate the value based on its kind
                match param.kind {
                    ParameterKind::Text => {
                        // Text values are always valid
                    }
                    ParameterKind::Number => {
                        if value.parse::<f64>().is_err() {
                            return Err(format!("parameter '{}' expects a number", param.name));
                        }
                    }
                    ParameterKind::Boolean => {
                        if !matches!(value.as_str(), "true" | "false") {
                            return Err(format!(
                                "parameter '{}' expects 'true' or 'false'",
                                param.name
                            ));
                        }
                    }
                }

                // A declared set of values is the whole point of declaring it: anything else is
                // refused by name, listing what would have been accepted.
                if let Some(allowed) = &param.values
                    && !allowed.contains(value)
                {
                    return Err(format!(
                        "parameter '{}' must be one of: {}",
                        param.name,
                        allowed.join(", ")
                    ));
                }
            }
            None => {
                // Value is missing
                if let Some(default) = &param.default {
                    bound.insert(param.name.clone(), default.clone());
                } else if param.required {
                    return Err(format!("parameter '{}' is required", param.name));
                }
            }
        }
    }

    Ok(bound)
}

/// Replace {{name}} everywhere a step carries text. Unknown placeholders are an error,
/// never silently left in place — a step that types a literal "{{search_term}}" into a
/// search box is exactly the confident-wrong-thing this work exists to stop.
pub fn fill(steps: &[Step], bound: &Values) -> Result<Vec<Step>, String> {
    steps
        .iter()
        .map(|step| match step {
            Step::Type { text } => {
                let filled = substitute_placeholders(text, bound)?;
                Ok(Step::Type { text: filled })
            }
            Step::Key { key } => {
                let filled = substitute_placeholders(key, bound)?;
                Ok(Step::Key { key: filled })
            }
            other => Ok(other.clone()),
        })
        .collect()
}

/// Substitute {{name}} placeholders in text with values from bound.
/// Returns an error if an unknown placeholder is found.
fn substitute_placeholders(text: &str, bound: &Values) -> Result<String, String> {
    let mut result = String::new();
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '{' && chars.peek() == Some(&'{') {
            // Found start of placeholder
            chars.next(); // consume second {
            let mut placeholder = String::new();
            let mut found_end = false;

            while let Some(ch) = chars.next() {
                if ch == '}' && chars.peek() == Some(&'}') {
                    chars.next(); // consume second }
                    found_end = true;
                    break;
                }
                placeholder.push(ch);
            }

            if !found_end {
                return Err(format!(
                    "unclosed placeholder '{{{{{}' in text",
                    placeholder
                ));
            }

            // Look up placeholder value
            match bound.get(&placeholder) {
                Some(value) => result.push_str(value),
                None => {
                    return Err(format!(
                        "unknown placeholder '{{{{{}}}}}' in text",
                        placeholder
                    ));
                }
            }
        } else {
            result.push(ch);
        }
    }

    Ok(result)
}

/// The screen the tape was taught on. The box's is 1280×800.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Screen {
    pub width: i32,
    pub height: i32,
}

impl Default for Screen {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 800,
        }
    }
}

/// One event off the tape, as the page reports it (`kind` decides which fields matter).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct TapeEvent {
    pub kind: String,
    #[serde(default)]
    pub x: i32,
    #[serde(default)]
    pub y: i32,
    #[serde(default)]
    pub button: i32,
    #[serde(default)]
    pub dx: i32,
    #[serde(default)]
    pub dy: i32,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub at: i64,
}

/// One step of a recipe, in the box's own vocabulary (`crates/box-cua/src/recipe.rs`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Step {
    Click { x: i32, y: i32, button: i32 },
    DoubleClick { x: i32, y: i32 },
    Drag { x1: i32, y1: i32, x2: i32, y2: i32 },
    Type { text: String },
    Key { key: String },
    Scroll { x: i32, y: i32, dx: i32, dy: i32 },
    Wait { ms: u64 },
}

/// The box refuses more steps than this in one recipe.
pub const MAX_STEPS: usize = 256;
/// The box caps a wait at ten seconds.
pub const MAX_WAIT_MS: u64 = 10_000;

/// A press and release this close in time and space is a click, not a drag.
const CLICK_MS: i64 = 300;
const CLICK_PX: i32 = 4;
/// Two clicks this close on the same spot are one double click.
const DOUBLE_CLICK_MS: i64 = 350;
/// A pause longer than this between steps is worth keeping as a wait.
const PAUSE_MS: i64 = 700;

/// The tape as steps. Moves vanish, presses pair into clicks and drags, typed letters join into
/// one `type`, wheel bursts sum into one `scroll`, and a long pause becomes a `wait`.
pub fn filter(events: &[TapeEvent], screen: Screen) -> Vec<Step> {
    let mut steps: Vec<Step> = Vec::new();
    let mut typed = String::new();
    let mut last_at: Option<i64> = None;
    let mut down: Option<&TapeEvent> = None;
    let mut wheel: Option<(i32, i32, i32, i32, i64)> = None;

    // Emits a pause if the tape was quiet between the previous step and `at`.
    fn pause(steps: &mut Vec<Step>, last_at: &mut Option<i64>, at: i64) {
        if let Some(prev) = *last_at {
            let gap = at - prev;
            if gap > PAUSE_MS {
                steps.push(Step::Wait {
                    ms: (gap as u64).min(MAX_WAIT_MS),
                });
            }
        }
        *last_at = Some(at);
    }
    fn flush_typed(steps: &mut Vec<Step>, typed: &mut String) {
        if !typed.is_empty() {
            steps.push(Step::Type {
                text: std::mem::take(typed),
            });
        }
    }
    fn flush_wheel(steps: &mut Vec<Step>, wheel: &mut Option<(i32, i32, i32, i32, i64)>) {
        if let Some((x, y, dx, dy, _)) = wheel.take()
            && (dx != 0 || dy != 0)
        {
            steps.push(Step::Scroll { x, y, dx, dy });
        }
    }

    for event in events {
        match event.kind.as_str() {
            "down" => {
                flush_typed(&mut steps, &mut typed);
                flush_wheel(&mut steps, &mut wheel);
                down = Some(event);
            }
            "up" => {
                let Some(start) = down.take() else { continue };
                let moved = (event.x - start.x).abs().max((event.y - start.y).abs());
                pause(&mut steps, &mut last_at, start.at);
                if moved <= CLICK_PX && event.at - start.at <= CLICK_MS {
                    let button = if start.button == 2 { 3 } else { 1 };
                    // A second click on the same spot right after the first is a double click.
                    if let Some(Step::Click {
                        x,
                        y,
                        button: previous,
                    }) = steps.last().cloned()
                        && previous == 1
                        && button == 1
                        && (x - start.x).abs() <= CLICK_PX
                        && (y - start.y).abs() <= CLICK_PX
                        && last_at.is_some_and(|prev| start.at - prev <= DOUBLE_CLICK_MS)
                        && !matches!(steps.last(), Some(Step::Wait { .. }))
                    {
                        steps.pop();
                        steps.push(Step::DoubleClick { x, y });
                    } else {
                        steps.push(Step::Click {
                            x: start.x,
                            y: start.y,
                            button,
                        });
                    }
                } else {
                    steps.push(Step::Drag {
                        x1: start.x,
                        y1: start.y,
                        x2: event.x,
                        y2: event.y,
                    });
                }
                last_at = Some(event.at);
            }
            "wheel" => {
                flush_typed(&mut steps, &mut typed);
                match wheel.as_mut() {
                    Some((_, _, dx, dy, at)) if event.at - *at <= CLICK_MS => {
                        *dx += event.dx;
                        *dy += event.dy;
                        *at = event.at;
                    }
                    _ => {
                        flush_wheel(&mut steps, &mut wheel);
                        pause(&mut steps, &mut last_at, event.at);
                        wheel = Some((event.x, event.y, event.dx, event.dy, event.at));
                    }
                }
                last_at = Some(event.at);
            }
            "keydown" => {
                flush_wheel(&mut steps, &mut wheel);
                if is_modifier(&event.key) {
                    continue;
                }
                if let Some(text) = printable(&event.key) {
                    if typed.is_empty() {
                        pause(&mut steps, &mut last_at, event.at);
                    }
                    typed.push_str(text);
                } else if let Some(key) = box_key(&event.key) {
                    flush_typed(&mut steps, &mut typed);
                    pause(&mut steps, &mut last_at, event.at);
                    steps.push(Step::Key {
                        key: key.to_string(),
                    });
                }
                last_at = Some(event.at);
            }
            // Moves are the pointer travelling; the press/release pair already says where it
            // went. Key-ups add nothing a keydown did not.
            _ => {}
        }
    }
    flush_typed(&mut steps, &mut typed);
    flush_wheel(&mut steps, &mut wheel);
    clamp(steps, screen)
}

/// Keep every step inside the screen and the list inside the box's limit.
fn clamp(mut steps: Vec<Step>, screen: Screen) -> Vec<Step> {
    let cx = |x: i32| x.clamp(0, screen.width - 1);
    let cy = |y: i32| y.clamp(0, screen.height - 1);
    for step in &mut steps {
        match step {
            Step::Click { x, y, .. } | Step::DoubleClick { x, y } | Step::Scroll { x, y, .. } => {
                *x = cx(*x);
                *y = cy(*y);
            }
            Step::Drag { x1, y1, x2, y2 } => {
                *x1 = cx(*x1);
                *y1 = cy(*y1);
                *x2 = cx(*x2);
                *y2 = cy(*y2);
            }
            Step::Wait { ms } => *ms = (*ms).min(MAX_WAIT_MS),
            Step::Type { .. } | Step::Key { .. } => {}
        }
    }
    steps.truncate(MAX_STEPS);
    steps
}

/// Why a recipe cannot be stored: the box would refuse it the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LintError {
    Empty,
    TooManySteps(usize),
    OutOfScreen { index: usize },
    WaitTooLong { index: usize },
    EmptyText { index: usize },
}

impl std::fmt::Display for LintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "the recipe has no steps"),
            Self::TooManySteps(n) => write!(f, "{n} steps; the box takes at most {MAX_STEPS}"),
            Self::OutOfScreen { index } => write!(f, "step {index} is outside the screen"),
            Self::WaitTooLong { index } => {
                write!(f, "step {index} waits longer than {MAX_WAIT_MS} ms")
            }
            Self::EmptyText { index } => write!(f, "step {index} types nothing"),
        }
    }
}

/// The box's own checks, applied before a recipe is stored or run.
pub fn lint(steps: &[Step], screen: Screen) -> Result<(), LintError> {
    if steps.is_empty() {
        return Err(LintError::Empty);
    }
    if steps.len() > MAX_STEPS {
        return Err(LintError::TooManySteps(steps.len()));
    }
    let inside = |x: i32, y: i32| (0..screen.width).contains(&x) && (0..screen.height).contains(&y);
    for (index, step) in steps.iter().enumerate() {
        let ok = match step {
            Step::Click { x, y, .. } | Step::DoubleClick { x, y } | Step::Scroll { x, y, .. } => {
                inside(*x, *y)
            }
            Step::Drag { x1, y1, x2, y2 } => inside(*x1, *y1) && inside(*x2, *y2),
            Step::Wait { ms } => {
                if *ms > MAX_WAIT_MS {
                    return Err(LintError::WaitTooLong { index });
                }
                true
            }
            Step::Type { text } => {
                if text.is_empty() {
                    return Err(LintError::EmptyText { index });
                }
                true
            }
            Step::Key { .. } => true,
        };
        if !ok {
            return Err(LintError::OutOfScreen { index });
        }
    }
    Ok(())
}

/// The request body the box's `POST /v1/cua/recipe` takes.
/// Accepts parameters and values for templating. Pass empty slices/maps for recipes with no parameters.
pub fn recipe_request(
    name: &str,
    params: &[Parameter],
    steps: &[Step],
    values: &Values,
) -> serde_json::Value {
    let mut request = serde_json::json!({
        "name": name,
        "stop_on_error": true,
        "screenshot": "end",
        "steps": steps,
    });

    // Include parameters and values if present
    if !params.is_empty() {
        request["params"] = serde_json::to_value(params).unwrap_or(serde_json::json!([]));
    }
    if !values.is_empty() {
        request["values"] = serde_json::to_value(values).unwrap_or(serde_json::json!({}));
    }

    request
}

fn is_modifier(key: &str) -> bool {
    matches!(
        key,
        "Shift" | "Control" | "Alt" | "Meta" | "CapsLock" | "Fn" | "Hyper" | "Super" | "OS"
    )
}

/// A key that typed something: one character, or the space bar.
fn printable(key: &str) -> Option<&str> {
    match key {
        " " | "Spacebar" => Some(" "),
        k if k.chars().count() == 1 => Some(k),
        _ => None,
    }
}

/// The box's name for a key the page reported (`KeyboardEvent.key`), for keys that are not
/// text. Unknown keys are dropped rather than guessed.
fn box_key(key: &str) -> Option<&'static str> {
    Some(match key {
        "Enter" => "Return",
        "Tab" => "Tab",
        "Escape" => "Escape",
        "Backspace" => "BackSpace",
        "Delete" => "Delete",
        "ArrowUp" => "Up",
        "ArrowDown" => "Down",
        "ArrowLeft" => "Left",
        "ArrowRight" => "Right",
        "Home" => "Home",
        "End" => "End",
        "PageUp" => "Prior",
        "PageDown" => "Next",
        "F1" => "F1",
        "F2" => "F2",
        "F3" => "F3",
        "F4" => "F4",
        "F5" => "F5",
        "F6" => "F6",
        "F7" => "F7",
        "F8" => "F8",
        "F9" => "F9",
        "F10" => "F10",
        "F11" => "F11",
        "F12" => "F12",
        _ => return None,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn ev(kind: &str, x: i32, y: i32, at: i64) -> TapeEvent {
        TapeEvent {
            kind: kind.into(),
            x,
            y,
            button: 0,
            dx: 0,
            dy: 0,
            key: String::new(),
            code: String::new(),
            at,
        }
    }
    fn key(k: &str, at: i64) -> TapeEvent {
        TapeEvent {
            key: k.into(),
            ..ev("keydown", 0, 0, at)
        }
    }

    /// The tape everyone will teach first: click the URL bar, type an address, Return.
    #[test]
    fn a_click_some_typing_and_return_become_three_steps() {
        let mut tape = vec![ev("move", 300, 60, 0), ev("move", 500, 62, 20)];
        tape.push(ev("down", 500, 62, 100));
        tape.push(ev("up", 501, 62, 180));
        for (i, c) in "example.com".chars().enumerate() {
            tape.push(key(&c.to_string(), 400 + i as i64 * 50));
            tape.push(TapeEvent {
                key: c.to_string(),
                ..ev("keyup", 0, 0, 420 + i as i64 * 50)
            });
        }
        tape.push(key("Enter", 1100));
        let steps = filter(&tape, Screen::default());
        assert_eq!(
            steps,
            vec![
                Step::Click {
                    x: 500,
                    y: 62,
                    button: 1
                },
                Step::Type {
                    text: "example.com".into()
                },
                Step::Key {
                    key: "Return".into()
                },
            ]
        );
        assert!(lint(&steps, Screen::default()).is_ok());
    }

    #[test]
    fn a_press_that_travels_is_a_drag_and_two_quick_clicks_are_a_double_click() {
        let tape = vec![
            ev("down", 100, 100, 0),
            ev("move", 200, 150, 200),
            ev("up", 300, 200, 600),
            ev("down", 50, 50, 700),
            ev("up", 50, 50, 760),
            ev("down", 51, 50, 900),
            ev("up", 51, 50, 950),
        ];
        let steps = filter(&tape, Screen::default());
        assert_eq!(
            steps,
            vec![
                Step::Drag {
                    x1: 100,
                    y1: 100,
                    x2: 300,
                    y2: 200
                },
                Step::DoubleClick { x: 50, y: 50 },
            ]
        );
    }

    #[test]
    fn a_long_pause_is_kept_as_a_wait_and_moves_vanish() {
        let mut tape = vec![ev("down", 10, 10, 0), ev("up", 10, 10, 50)];
        for i in 0..300 {
            tape.push(ev("move", 10 + i, 10, 60 + i as i64));
        }
        tape.push(ev("down", 400, 400, 3000));
        tape.push(ev("up", 400, 400, 3050));
        let steps = filter(&tape, Screen::default());
        assert_eq!(
            steps,
            vec![
                Step::Click {
                    x: 10,
                    y: 10,
                    button: 1
                },
                Step::Wait { ms: 2950 },
                Step::Click {
                    x: 400,
                    y: 400,
                    button: 1
                },
            ]
        );
    }

    #[test]
    fn a_wheel_burst_is_one_scroll_and_a_right_button_is_button_three() {
        let tape = vec![
            TapeEvent {
                dy: -100,
                ..ev("wheel", 640, 400, 0)
            },
            TapeEvent {
                dy: -120,
                ..ev("wheel", 640, 402, 80)
            },
            TapeEvent {
                button: 2,
                ..ev("down", 640, 400, 2000)
            },
            TapeEvent {
                button: 2,
                ..ev("up", 640, 400, 2040)
            },
        ];
        let steps = filter(&tape, Screen::default());
        assert_eq!(
            steps,
            vec![
                Step::Scroll {
                    x: 640,
                    y: 400,
                    dx: 0,
                    dy: -220
                },
                // The pause runs from the end of the wheel burst (t=80) to the press (t=2000).
                Step::Wait { ms: 1920 },
                Step::Click {
                    x: 640,
                    y: 400,
                    button: 3
                },
            ]
        );
    }

    #[test]
    fn lint_refuses_what_the_box_would() {
        assert_eq!(lint(&[], Screen::default()), Err(LintError::Empty));
        let off = vec![Step::Click {
            x: 5000,
            y: 5000,
            button: 1,
        }];
        assert_eq!(
            lint(&off, Screen::default()),
            Err(LintError::OutOfScreen { index: 0 })
        );
        let long = vec![Step::Wait { ms: 60_000 }];
        assert_eq!(
            lint(&long, Screen::default()),
            Err(LintError::WaitTooLong { index: 0 })
        );
        // The filter itself never produces what lint refuses.
        let clamped = filter(
            &[ev("down", 5000, 5000, 0), ev("up", 5000, 5000, 10)],
            Screen::default(),
        );
        assert!(lint(&clamped, Screen::default()).is_ok());
    }

    // Parameter binding and substitution tests

    #[test]
    fn happy_path_substitution() {
        let params = vec![Parameter {
            name: "search_term".into(),
            description: "What to search for".into(),
            required: true,
            kind: ParameterKind::Text,
            default: None,
            values: None,
        }];

        let mut given = Values::new();
        given.insert("search_term".into(), "hello".into());

        let bound = bind(&params, &given).unwrap();
        assert_eq!(bound.get("search_term").unwrap(), "hello");

        let steps = vec![
            Step::Type {
                text: "search {{search_term}}".into(),
            },
            Step::Key {
                key: "Return".into(),
            },
        ];

        let filled = fill(&steps, &bound).unwrap();
        assert_eq!(
            filled,
            vec![
                Step::Type {
                    text: "search hello".into(),
                },
                Step::Key {
                    key: "Return".into(),
                },
            ]
        );
    }

    #[test]
    fn a_required_parameter_can_be_declared() {
        // The bug this exists to stop: declaring was checked by binding against no values, so a
        // required parameter was refused at declaration and could never be created at all.
        let required = vec![Parameter {
            name: "search_term".into(),
            description: "What to search for".into(),
            required: true,
            kind: ParameterKind::Text,
            default: None,
            values: None,
        }];
        assert_eq!(check(&required), Ok(()));
        // And it is still required when a run supplies nothing.
        assert!(bind(&required, &Values::new()).is_err());
    }

    #[test]
    fn a_declaration_that_could_never_be_satisfied_is_refused() {
        let twice = vec![
            Parameter {
                name: "q".into(),
                description: String::new(),
                required: false,
                kind: ParameterKind::Text,
                default: None,
                values: None,
            },
            Parameter {
                name: "q".into(),
                description: String::new(),
                required: false,
                kind: ParameterKind::Text,
                default: None,
                values: None,
            },
        ];
        assert_eq!(check(&twice), Err("parameter 'q' is declared twice".into()));

        let empty_set = vec![Parameter {
            name: "pick".into(),
            description: String::new(),
            required: false,
            kind: ParameterKind::Text,
            default: None,
            values: Some(Vec::new()),
        }];
        assert!(check(&empty_set).is_err());

        // A default that the parameter's own rules would reject is a recipe that refuses every
        // run for a reason nobody typed.
        let bad_default = vec![Parameter {
            name: "count".into(),
            description: String::new(),
            required: false,
            kind: ParameterKind::Number,
            default: Some("lots".into()),
            values: None,
        }];
        let why = check(&bad_default).unwrap_err();
        assert!(why.contains("count"), "{why}");
    }

    #[test]
    fn missing_required_parameter() {
        let params = vec![Parameter {
            name: "query".into(),
            description: "Search query".into(),
            required: true,
            kind: ParameterKind::Text,
            default: None,
            values: None,
        }];

        let given = Values::new();
        let result = bind(&params, &given);
        assert_eq!(result, Err("parameter 'query' is required".into()));
    }

    #[test]
    fn bad_number_parameter() {
        let params = vec![Parameter {
            name: "count".into(),
            description: "How many".into(),
            required: true,
            kind: ParameterKind::Number,
            default: None,
            values: None,
        }];

        let mut given = Values::new();
        given.insert("count".into(), "not a number".into());

        let result = bind(&params, &given);
        assert_eq!(result, Err("parameter 'count' expects a number".into()));
    }

    #[test]
    fn value_outside_enum() {
        let params = vec![Parameter {
            name: "action".into(),
            description: "What to do".into(),
            required: true,
            kind: ParameterKind::Text,
            default: None,
            values: Some(vec!["click".into(), "type".into(), "scroll".into()]),
        }];

        let mut given = Values::new();
        given.insert("action".into(), "jump".into());

        let result = bind(&params, &given);
        assert_eq!(
            result,
            Err("parameter 'action' must be one of: click, type, scroll".into())
        );
    }

    #[test]
    fn unknown_placeholder() {
        let steps = vec![Step::Type {
            text: "find {{unknown}}".into(),
        }];

        let bound = Values::new();
        let result = fill(&steps, &bound);
        assert_eq!(
            result,
            Err("unknown placeholder '{{unknown}}' in text".into())
        );
    }

    #[test]
    fn invalid_parameter_name() {
        let params = vec![Parameter {
            name: "Bad-Name".into(),
            description: "Invalid".into(),
            required: true,
            kind: ParameterKind::Text,
            default: None,
            values: None,
        }];

        let given = Values::new();
        let result = bind(&params, &given);
        assert!(
            result
                .unwrap_err()
                .contains("must contain only lowercase letters, digits, and underscores")
        );
    }

    #[test]
    fn no_parameters_unchanged() {
        let steps = vec![
            Step::Type {
                text: "hello".into(),
            },
            Step::Key {
                key: "Return".into(),
            },
        ];

        let params: Vec<Parameter> = vec![];
        let values = Values::new();

        let bound = bind(&params, &values).unwrap();
        assert_eq!(bound, values);

        let filled = fill(&steps, &bound).unwrap();
        assert_eq!(filled, steps);
    }

    #[test]
    fn default_value_filled() {
        let params = vec![Parameter {
            name: "timeout".into(),
            description: "Wait time".into(),
            required: false,
            kind: ParameterKind::Number,
            default: Some("5000".into()),
            values: None,
        }];

        let given = Values::new();
        let bound = bind(&params, &given).unwrap();
        assert_eq!(bound.get("timeout").unwrap(), "5000");
    }

    #[test]
    fn boolean_parameter() {
        let params = vec![Parameter {
            name: "enabled".into(),
            description: "Toggle".into(),
            required: true,
            kind: ParameterKind::Boolean,
            default: None,
            values: None,
        }];

        let mut given = Values::new();
        given.insert("enabled".into(), "true".into());
        let bound = bind(&params, &given).unwrap();
        assert_eq!(bound.get("enabled").unwrap(), "true");

        // Test invalid boolean
        given.clear();
        given.insert("enabled".into(), "yes".into());
        let result = bind(&params, &given);
        assert_eq!(
            result,
            Err("parameter 'enabled' expects 'true' or 'false'".into())
        );
    }

    #[test]
    fn multiple_placeholders_in_one_step() {
        let params = vec![
            Parameter {
                name: "first".into(),
                description: "First".into(),
                required: true,
                kind: ParameterKind::Text,
                default: None,
                values: None,
            },
            Parameter {
                name: "second".into(),
                description: "Second".into(),
                required: true,
                kind: ParameterKind::Text,
                default: None,
                values: None,
            },
        ];

        let mut given = Values::new();
        given.insert("first".into(), "hello".into());
        given.insert("second".into(), "world".into());

        let bound = bind(&params, &given).unwrap();

        let steps = vec![Step::Type {
            text: "{{first}} {{second}}".into(),
        }];

        let filled = fill(&steps, &bound).unwrap();
        assert_eq!(
            filled,
            vec![Step::Type {
                text: "hello world".into(),
            }]
        );
    }

    #[test]
    fn recipe_request_with_parameters() {
        let params = vec![Parameter {
            name: "query".into(),
            description: "Search query".into(),
            required: true,
            kind: ParameterKind::Text,
            default: None,
            values: None,
        }];

        let mut values = Values::new();
        values.insert("query".into(), "test".into());

        let steps = vec![Step::Type {
            text: "search".into(),
        }];

        let request = recipe_request("test_recipe", &params, &steps, &values);

        assert_eq!(request["name"], "test_recipe");
        assert!(request["params"].is_array());
        assert!(request["values"].is_object());
        assert_eq!(request["values"]["query"], "test");
    }

    #[test]
    fn recipe_request_without_parameters() {
        let steps = vec![Step::Type {
            text: "hello".into(),
        }];

        let request = recipe_request("simple_recipe", &[], &steps, &Values::new());

        assert_eq!(request["name"], "simple_recipe");
        assert!(request["params"].is_null());
        assert!(request["values"].is_null());
        assert!(request["steps"].is_array());
        assert_eq!(request["steps"].as_array().unwrap().len(), 1);
    }
}
