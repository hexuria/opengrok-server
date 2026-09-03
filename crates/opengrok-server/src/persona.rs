//! Who a coworker is, as one system message.
//!
//! A coworker carries a `title` (what it is) and a `role` (what it is for, in the person's own
//! words). Both live in the seam-B profile beside the description, because that is where the
//! client already puts what it knows about a coworker and a second home would be a second
//! answer.
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
    /// Read from a seam-B profile blob.
    #[must_use]
    pub fn from_profile(profile: Option<&Value>) -> Self {
        let field = |key: &str| {
            profile
                .and_then(|p| p.get(key))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        };
        Self {
            title: field("title"),
            role: field("role"),
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
    Persona {
        title: Persona::from_profile(profile.as_ref()).title,
        role: role
            .map(|role| role.trim().to_string())
            .filter(|role| !role.is_empty()),
    }
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
    fn the_profile_reads_absent_blank_and_whitespace_alike() {
        assert_eq!(Persona::from_profile(None), Persona::default());
        assert_eq!(
            Persona::from_profile(Some(&json!({ "title": "", "role": "   " }))),
            Persona::default(),
            "blank is not a title"
        );
        assert_eq!(
            Persona::from_profile(Some(
                &json!({ "title": " release engineer ", "role": "ships" })
            )),
            persona(Some("release engineer"), Some("ships"))
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
