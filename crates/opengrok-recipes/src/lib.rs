//! From a taught tape to a recipe the box will run.
//!
//! A tape is what the screen window records while a person teaches a task: every pointer and
//! key event on the noVNC canvas, in the screen's own pixels. A recipe is what hexuria/box's
//! `POST /v1/cua/recipe` runs in one call: clicks, drags, typed text, key presses, scrolls and
//! waits. `filter` is the one-way road between them — pure, so the server route and the tests
//! agree, and so a recipe the registry holds is one the box will accept (`lint`).

use serde::{Deserialize, Serialize};

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
pub fn recipe_request(name: &str, steps: &[Step]) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "stop_on_error": true,
        "screenshot": "end",
        "steps": steps,
    })
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
}
