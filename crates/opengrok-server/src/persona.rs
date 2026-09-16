//! Who a coworker is, as one system message.
//!
//! A coworker carries a `title` (what it is) and a `role` (what it is for, in the person's own
//! words). They live in different places, on purpose. The TITLE is in the seam-B profile blob
//! beside the description, because that is where the client already puts its decoration and a
//! second home for it would be a second answer. The ROLE is a column on the aggregate, because
//! it is behavioural rather than cosmetic: the run path reads it on every single turn, the
//! thousand-character bound is an invariant worth enforcing where the state lives, and a field
//! the model is told about is not decoration.
//!
//! ONE system message per run, not several. Two prompts arrive at the model as two claims about
//! the same coworker, and when they disagree the model picks one — `computer_system_prompt`'s
//! comment records the day that happened, where a prompt contradicting the tool list silently
//! disabled the tool. So the identity, the standing role and the machine discipline are composed
//! into a single string here, in that order, and every run path uses this one function.
//!
//! The room is the exception, and deliberately: `group::member_system_prompt` is transcribed from
//! the client's own orchestrator (CLAUDE.md #1) and already opens "You are {name}, one participant
//! in a group chat". Rewriting it to fit this shape would edit transcribed text, so the role is
//! APPENDED after it instead and every transcribed line stays byte-identical.

use opengrok_core::id::CoworkerId;
use serde_json::Value;

use crate::agui::AgUiState;

/// The most a role may be. Long enough for a paragraph of intent, short enough that it cannot
/// become a second system prompt smuggled through a text field.
pub const MAX_ROLE_CHARS: usize = 1000;

/// A role as the person wrote it, or the sentence to refuse it with. `None` clears it.
pub fn validate_role(role: Option<&str>) -> Result<Option<String>, String> {
    let Some(role) = role else {
        return Ok(None);
    };
    let trimmed = role.trim();
    if trimmed.is_empty() {
        // Blank is how a person clears a role, not an error: the field is nullable.
        return Ok(None);
    }
    let length = trimmed.chars().count();
    if length > MAX_ROLE_CHARS {
        return Err(format!(
            "role: {length} characters is longer than the {MAX_ROLE_CHARS} allowed"
        ));
    }
    Ok(Some(trimmed.to_string()))
}

/// What the profile says this coworker is and is for. Absent, blank and whitespace all read as
/// nothing, so a cleared field behaves the same as one never set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Persona {
    pub title: Option<String>,
    pub role: Option<String>,
}

impl Persona {
    /// Compose from the two homes: the title out of the seam-B profile blob, the role out of the
    /// aggregate's column.
    ///
    /// A `role` key in the blob is IGNORED, structurally — this function cannot read one. An
    /// older client or an earlier shape may have written one, and merging it would give a
    /// coworker two roles with the run path picking between them. The column is the only answer.
    #[must_use]
    pub fn compose(profile: Option<&Value>, role: Option<&str>) -> Self {
        let text = |value: Option<&str>| {
            value
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        };
        Self {
            title: text(profile.and_then(|p| p.get("title")).and_then(Value::as_str)),
            role: text(role),
        }
    }

    /// "You are Ada, a release engineer." — or just the name when there is no title. Always
    /// present: a coworker knowing its own name is the floor, and a model that does not know
    /// what it is called cannot answer to it.
    #[must_use]
    pub fn identity(&self, name: &str) -> String {
        match &self.title {
            Some(title) => format!("You are {name}, {}.", title.trim_end_matches('.')),
            None => format!("You are {name}."),
        }
    }

    /// The standing role, with the sentence that says it is standing. Without that sentence a
    /// role reads as instructions for this turn, and a coworker asked something unrelated drops
    /// it — the whole point is that it survives the conversation it was not written for.
    #[must_use]
    pub fn standing(&self) -> Option<String> {
        let role = self.role.as_ref()?;
        Some(format!(
            "{role}\n\nThat role stands in every conversation, whoever is speaking to you and \
             whatever they ask about."
        ))
    }
}

/// The system prompt that keeps a coworker honest about WHOSE computer it is using. A bot has its
/// OWN box (sandboxed, on the server); the user has their own machine; these are different, and a
/// bot must never present work done on its box as done on the user's computer. When it has no
/// computer, it should say so rather than pretend. Written for the day a reverse channel makes "my
/// computer" name two real machines — the distinction has to be in the model's head before then.
///
/// Lives here, next to `system_message`, so every run path (desktop, AG-UI, autonomy) can pass the
/// same tail rather than each inventing one. The two halves MUST track the tool list: a prompt
/// that contradicts the offering silently disables the tool.
#[must_use]
pub fn computer_system_prompt(
    has_computer: bool,
    has_screen: bool,
    reaches_user_machine: bool,
    user_machine_label: Option<&str>,
) -> String {
    if has_computer {
        // NO OS OR HARDWARE NAMES HERE. The box's runtime varies (Linux today; other kinds later)
        // and the user's machine is whatever they enrolled — naming either ("Linux box", "their
        // Mac") turns the prompt into a lie the day the fleet changes. The distinction that must
        // survive is WHOSE machine, not what it runs.
        let mut prompt = "You have your OWN computer: a sandboxed box running on the server. It is a DIFFERENT \
         machine from the user's own computer. Your shell, read_file and \
         write_file tools act ONLY on your own box — they cannot touch the user's machine. When you \
         run a command or create, read or change a file, it happens on YOUR box, and you must say so \
         plainly, e.g. \"I created /tmp/foo on my own computer (the box), not on your machine.\" \
         Never describe work done on your box as done on the user's computer."
            .to_string();
        if has_screen {
            // Says exactly what `open_url` and `computer` are offered as — the prompt and the
            // offering must agree, or the model is told about a screen it cannot reach.
            prompt.push_str(
                " Your box ALSO has a SCREEN: a 1280x800 desktop with a dock (Terminal, Chromium, \
                 Files). `open_url` opens a web page in your own browser there. `computer` looks at \
                 the screen (action=screenshot, which returns an image) or acts on it at pixel \
                 coordinates (click, right_click, double_click, move, drag, type, key, scroll) and \
                 returns a fresh screenshot. Take a screenshot before you act, do one step at a \
                 time, and read each screenshot before the next step. The user can watch your \
                 screen, so say what you see and what you are doing.",
            );
        }
        if reaches_user_machine {
            // The enrolled label (e.g. "Uriah's-MacBook-Pro.local") is the one name for the
            // user's machine that stays TRUE whatever it runs — the daemon reported it at
            // enrolment. Guessing an OS instead ("their Mac") becomes a lie the day a Windows
            // or second machine enrolls. No label ⇒ stay generic, never invent one.
            let machine = match user_machine_label {
                Some(label) => format!("the computer they enrolled, \"{label}\""),
                None => "the real computer they enrolled".to_string(),
            };
            prompt.push_str(&format!(
                " You ALSO have the `user_machine_shell` tool, which runs a command on the USER'S \
                 OWN machine — {machine} — with their consent: a command may \
                 run, be refused, or wait for the user to approve it, and waiting is normal — the \
                 user may answer minutes or hours later, so never retry or give up on a waiting \
                 command. When the user asks you to do something on THEIR computer, use \
                 `user_machine_shell` rather than telling them to do it themselves, and refer to \
                 their machine by that name."
            ));
        } else {
            prompt.push_str(
                " If the user asks you to do something on THEIR computer, tell them you can only \
                 use your own box and cannot reach their machine, and offer to do it on your box \
                 instead.",
            );
        }
        prompt
    } else {
        "You do NOT currently have a computer, so you cannot run shell commands or read or write \
         files anywhere. Do not claim to run commands or access any machine. If the user needs \
         something run, explain that your computer is not available yet and, where useful, give them \
         the exact command to run themselves."
            .to_string()
    }
}

/// The one system message a run carries: identity, then the standing role, then whatever else
/// the run needs the model to know — today the machine discipline. Blocks are separated by a
/// blank line so the model reads them as distinct claims rather than one run-on instruction.
#[must_use]
pub fn system_message(name: &str, persona: &Persona, tail: Option<&str>) -> String {
    // A blank name means the coworker row could not be read; say nothing rather than "You are ."
    let mut blocks: Vec<String> = match name.trim() {
        "" => Vec::new(),
        name => vec![persona.identity(name)],
    };
    if let Some(standing) = persona.standing() {
        blocks.push(standing);
    }
    if let Some(tail) = tail.map(str::trim).filter(|text| !text.is_empty()) {
        blocks.push(tail.to_string());
    }
    blocks.join("\n\n")
}

/// The room's transcribed prompt with the standing role appended. The transcription is returned
/// unchanged when there is no role, and never edited when there is: the role is a new paragraph
/// after it, so a diff of the transcribed text stays empty.
#[must_use]
pub fn with_standing_role(transcribed: &str, persona: &Persona) -> String {
    match persona.standing() {
        Some(standing) => format!("{transcribed}\n\n{standing}"),
        None => transcribed.to_string(),
    }
}

/// The persona of a coworker as the run path needs it: the title from the seam-B profile, where
/// the client's decoration lives, and the role from the aggregate, where a field the model reads
/// every turn belongs. A failed read is not a failed turn — a coworker with no persona is still
/// a coworker, and holding a turn because a profile row would not load would be the wrong trade.
pub async fn of(state: &AgUiState, coworker: &CoworkerId, role: Option<String>) -> Persona {
    let profile = state
        .auth
        .store
        .seamb_profile(coworker)
        .await
        .ok()
        .flatten();
    Persona::compose(profile.as_ref(), role.as_deref())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn persona(title: Option<&str>, role: Option<&str>) -> Persona {
        Persona {
            title: title.map(str::to_string),
            role: role.map(str::to_string),
        }
    }

    #[test]
    fn a_role_is_trimmed_capped_and_clearable() {
        assert_eq!(validate_role(None).unwrap(), None);
        assert_eq!(validate_role(Some("   ")).unwrap(), None, "blank clears it");
        assert_eq!(
            validate_role(Some("  keeps the release notes  ")).unwrap(),
            Some("keeps the release notes".to_string())
        );
        let long = "x".repeat(MAX_ROLE_CHARS);
        assert!(validate_role(Some(&long)).is_ok(), "the limit itself fits");
        let over = "x".repeat(MAX_ROLE_CHARS + 1);
        let refusal = validate_role(Some(&over)).unwrap_err();
        assert!(
            refusal.starts_with("role: 1001 characters is longer than the 1000"),
            "the sentence says the numbers: {refusal}"
        );
        // Counted in characters, not bytes: a paragraph of accented prose is not secretly halved.
        let accented = "é".repeat(MAX_ROLE_CHARS);
        assert!(validate_role(Some(&accented)).is_ok());
    }

    #[test]
    fn composing_reads_absent_blank_and_whitespace_alike() {
        assert_eq!(Persona::compose(None, None), Persona::default());
        assert_eq!(
            Persona::compose(Some(&json!({ "title": "" })), Some("   ")),
            Persona::default(),
            "blank is not a title, and blank is not a role"
        );
        assert_eq!(
            Persona::compose(
                Some(&json!({ "title": " release engineer " })),
                Some(" ships ")
            ),
            persona(Some("release engineer"), Some("ships")),
            "both trimmed"
        );
    }

    /// The two homes, and which one answers. A blob that carries a role — written by an older
    /// client, or left from an earlier shape — must not give the coworker a second role.
    #[test]
    fn the_column_is_the_only_role_and_a_role_in_the_blob_is_ignored() {
        let blob = json!({ "title": "a release engineer", "role": "STALE ROLE FROM THE BLOB" });
        assert_eq!(
            Persona::compose(Some(&blob), Some("the column's role")),
            persona(Some("a release engineer"), Some("the column's role")),
            "the column wins"
        );
        assert_eq!(
            Persona::compose(Some(&blob), None),
            persona(Some("a release engineer"), None),
            "and with no column role the coworker has none — the blob's is not a fallback"
        );
        assert!(
            !system_message("Ada", &Persona::compose(Some(&blob), None), None).contains("STALE"),
            "nothing from the blob's role reaches the model"
        );
    }

    #[test]
    fn the_identity_line_names_the_coworker_with_or_without_a_title() {
        assert_eq!(persona(None, None).identity("Ada"), "You are Ada.");
        assert_eq!(
            persona(Some("a release engineer"), None).identity("Ada"),
            "You are Ada, a release engineer."
        );
        assert_eq!(
            persona(Some("a release engineer."), None).identity("Ada"),
            "You are Ada, a release engineer.",
            "a title the person already ended with a full stop does not get two"
        );
    }

    #[test]
    fn a_coworker_whose_row_would_not_load_says_nothing_about_itself() {
        assert_eq!(
            system_message(
                "",
                &persona(Some("a release engineer"), None),
                Some("Tail.")
            ),
            "Tail.",
            "no name, no identity line — never \"You are .\""
        );
        assert_eq!(system_message("  ", &Persona::default(), None), "");
    }

    #[test]
    fn the_system_message_is_one_message_in_a_fixed_order() {
        let full = system_message(
            "Ada",
            &persona(
                Some("a release engineer"),
                Some("Keep the changelog honest."),
            ),
            Some("You have your OWN computer."),
        );
        assert_eq!(
            full,
            "You are Ada, a release engineer.\n\nKeep the changelog honest.\n\nThat role stands in \
             every conversation, whoever is speaking to you and whatever they ask about.\n\nYou \
             have your OWN computer."
        );
        // No role: identity and the machine discipline, nothing invented in between.
        assert_eq!(
            system_message(
                "Ada",
                &persona(None, None),
                Some("You have your OWN computer.")
            ),
            "You are Ada.\n\nYou have your OWN computer."
        );
        // No tail: a run that needs nothing else still says who the coworker is.
        assert_eq!(
            system_message("Ada", &persona(None, Some("Ships.")), None),
            "You are Ada.\n\nShips.\n\nThat role stands in every conversation, whoever is speaking \
             to you and whatever they ask about."
        );
        assert_eq!(
            system_message("Ada", &persona(None, None), Some("   ")),
            "You are Ada.",
            "an empty tail adds no blank block"
        );
    }

    #[test]
    fn the_computer_prompt_tracks_whether_the_tools_exist() {
        let box_only = computer_system_prompt(true, false, false, None);
        assert!(
            box_only.contains("You have your OWN computer"),
            "{box_only}"
        );
        assert!(
            box_only.contains("cannot reach their machine"),
            "no reverse channel ⇒ say so: {box_only}"
        );
        assert!(
            !box_only.contains("user_machine_shell"),
            "must not name a tool that is not offered: {box_only}"
        );

        let with_user = computer_system_prompt(true, false, true, Some("office.local"));
        assert!(
            with_user.contains("`user_machine_shell`"),
            "the offered tool must be named: {with_user}"
        );
        assert!(
            with_user.contains("office.local"),
            "the enrolled label is the true name: {with_user}"
        );

        let none = computer_system_prompt(false, false, true, Some("ignored"));
        assert!(
            none.contains("You do NOT currently have a computer"),
            "{none}"
        );
        assert!(
            !none.contains("user_machine_shell"),
            "no box ⇒ the reverse channel is not offered either: {none}"
        );
    }

    /// The transcription rule, as a test: the room's text must survive byte-for-byte.
    #[test]
    fn the_rooms_transcribed_prompt_is_never_edited() {
        let transcribed = "You are Ada, one participant in a group chat (Pair).\n\nYour persona: x";
        assert_eq!(
            with_standing_role(transcribed, &persona(None, None)),
            transcribed,
            "no role, no change at all"
        );
        let with_role = with_standing_role(transcribed, &persona(None, Some("Ships.")));
        assert!(
            with_role.starts_with(transcribed),
            "the transcription is a prefix, untouched: {with_role}"
        );
        assert!(
            with_role.ends_with("whatever they ask about."),
            "{with_role}"
        );
    }
}
