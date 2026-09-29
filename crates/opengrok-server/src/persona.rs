//! Who a coworker is, as one system message.
//!
//! A coworker carries a `title` (what it is) and a `role` (what it is for, in the person's own
//! words). They live in different places, on purpose. The TITLE is in the coworker's profile blob
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
//! A SKILL IS THE LAST SEGMENT OF THAT ONE MESSAGE, AND LAST IS NOT A DETAIL. A skill body is
//! unbounded prose a person wrote, so of everything that goes into this message it is the likeliest
//! to contradict the machine discipline above it. `fenced_skill` says why it lands after every
//! segment that decides what the coworker may do, rather than before.
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

/// A name as the person wrote it, or the sentence to refuse it with.
///
/// Trimmed, because a name of spaces is the same lie as an empty one. Unlike a role, blank
/// cannot mean "clear it": `system_message` writes no identity line without a name, so a
/// coworker stored nameless would lose the one thing every persona is built on — it would stop
/// being able to say what it is called.
pub fn validate_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("name: a coworker needs a name to answer to".to_string());
    }
    Ok(trimmed.to_string())
}

/// The decoration a client keeps in the coworker's profile blob, as opposed to the fields the
/// aggregate owns. One list, because every door that edits a profile must write the same keys —
/// three did until P0-E removed seam A and seam B; `PATCH /coworkers/{id}` (`agui/routes.rs`) is
/// the one left — and a key one door forgot is an edit the person watched succeed and lost.
pub const PROFILE_TEXT_KEYS: [&str; 4] = ["description", "title", "avatarShape", "avatarColor"];

/// Merge the string-valued decoration out of `edits` into `profile`, in place.
///
/// A key absent leaves what is stored alone, so a partial update stays partial. An empty string
/// is written as one rather than removing the key: `Persona::compose` reads blank and absent
/// alike, so a cleared title is a coworker with no title and not one called "".
pub fn merge_profile_text(profile: &mut Value, edits: &Value) {
    let Some(map) = profile.as_object_mut() else {
        return;
    };
    for key in PROFILE_TEXT_KEYS {
        if let Some(value) = edits.get(key).and_then(Value::as_str) {
            map.insert(key.to_string(), Value::String(value.to_string()));
        }
    }
}

/// What the profile says this coworker is and is for. Absent, blank and whitespace all read as
/// nothing, so a cleared field behaves the same as one never set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Persona {
    pub title: Option<String>,
    pub role: Option<String>,
}

impl Persona {
    /// Compose from the two homes: the title out of the coworker's profile blob, the role out of
    /// the aggregate's column.
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
/// Why the browser tools are missing this turn, when they are: the person routes this
/// computer's traffic through their own desktop and has said this computer may not use it. Said
/// so the model does not go looking for `open_url`, and does not claim it cannot see at all.
pub fn network_off_line(network_off: bool, unconfirmed: bool) -> String {
    if !network_off {
        return String::new();
    }
    if unconfirmed {
        // The withholding is the server's caution, not the person's choice: say that, and do
        // not point them at a setting they never touched.
        return " This turn you have NO browser or screen tools: `open_url`, `computer`, \
                 `credential.request` and `request_user_form` are not available, and the login \
                 instructions above do not apply, because this computer's use of the person's \
                 network could not be confirmed right now. Your shell and files still work. If \
                 asked to browse, open a page, log in somewhere or look at your screen, say that \
                 your network access could not be confirmed this turn and to try again shortly."
            .to_string();
    }
    " Your box's web traffic would go out through the person's own network, and they have \
     switched that off for this computer, so this turn you have NO browser or screen tools: \
     `open_url`, `computer`, `credential.request` and `request_user_form` are not available, \
     and the login instructions above do not apply. Your shell and files still work. If asked \
     to browse, open a page, log in somewhere or look at your screen, say that THEY switched \
     off this computer's use of their network (Settings → Computer, or the Computer pane) and \
     can turn it back on — do not say you have no screen or that screen access is unavailable \
     in this chat, which is not the reason."
        .to_string()
}

#[must_use]
/// What the person reached for this turn, said once, at the end of the computer prompt.
///
/// The tools are already on offer; this only tells the model which ones the person named, and
/// says plainly that the rest are still there. Without the second half a model reads a named
/// tool as the only permitted one and gives up when it does not fit.
pub fn preferred_tools_line(preferred: &[String]) -> String {
    if preferred.is_empty() {
        return String::new();
    }
    let named = preferred
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let these = if preferred.len() == 1 { "it" } else { "them" };
    format!(
        " For THIS message the person named {named}. Reach for {these} first where {these} fits \
         the request, and say so if you do not. Every other tool you have is still available: a \
         named tool is what they reached for, not the only thing you may use."
    )
}

/// The recipe the person picked in the composer, and what they typed into its fields.
///
/// THE TOOL ALREADY HAS THE VALUES; THE MODEL DOES NOT. `run_recipe` is handed the person's
/// values whatever the model passes, so a turn was already runnable — but the model, told
/// nothing, read a bare "run the youtube recipe" as a request missing its subject and asked for
/// a search term the person had already typed into a field named after it. Asking for what you
/// have been given is worse than guessing: it tells the person their answer did not arrive.
///
/// So the values are named here in the same breath as the recipe. Nothing is asserted about what
/// the tool will do with them — that is the executor's business and it wins either way.
#[must_use]
pub fn chosen_recipe_line(
    name: &str,
    values: &std::collections::BTreeMap<String, String>,
) -> String {
    let name = name.trim();
    if name.is_empty() {
        return String::new();
    }
    if values.is_empty() {
        return format!(
            " For THIS message the person chose the recipe `{name}`. Run it with `run_recipe` \
             rather than working the screen step by step, and say that you used it."
        );
    }
    let filled = values
        .iter()
        .map(|(field, value)| format!("{field} = {value:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        " For THIS message the person chose the recipe `{name}` and filled it in: {filled}. Those \
         values are already on their way to the recipe, so run it with `run_recipe` rather than \
         asking for them again or working the screen step by step, and say that you used it."
    )
}

// The words a skill is framed in, and how its files and its absence are said, live beside the
// name check and the frontmatter parser they rest on (`opengrok_plugins::skill`).
pub use opengrok_plugins::skill::{
    SKILL_CLOSING_LINE, SKILL_DRAFT_LINE, SKILL_FILES_UNAVAILABLE, SKILL_UNAVAILABLE_LINE,
    SkillAuthor, SkillDoor, fenced_skill, skill_files_line, skill_files_unavailable_line,
    skill_marker, skill_name_from_system,
};

pub fn computer_system_prompt(
    has_computer: bool,
    has_screen: bool,
    has_recipes: bool,
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
         Never describe work done on your box as done on the user's computer. When a page asks \
         for a site login, call `request_user_form` and wait. NativeChat offers the person their \
         saved logins for that site on the card; they confirm with Touch ID, and the values are \
         typed into the page out of your view. You never receive a password and must not type \
         one with `computer`. The person fills in chat; the server clicks each field at the \
         position you give and types there, never showing you the secret. After a user-form \
         settles, screenshot and confirm what \
         the page shows; filling is not a successful login. When the page shows the email and \
         password fields TOGETHER, raise ONE card with both fields, `samePage: true` and \
         `submit: true`, giving each field its position (`at`) from your screenshot — do not \
         split them and do not focus a field first. Only a page that asks for the email alone \
         gets an email-only card: raise it, then after it settles screenshot; if a password \
         page is next, call `request_user_form` with a password-only form (new entryId, \
         challengeKind \"password\"). \
         If another in-sandbox challenge appears (an authenticator code, a phone code on the same \
         page), call `request_user_form` again with otp fields and challengeKind \"otp\", the \
         page host as liveHost, and the code field's position (`at`); NativeChat offers the \
         person their saved authenticator code for that site on the card, and the digits are \
         typed out of your view — never re-raise a form that already settled. \
         A passkey: when the page offers to sign in with a passkey, call `request_user_form` with \
         challengeKind \"passkey\", passkeyMode \"use\", no fields, and the page host as liveHost, \
         and wait; NativeChat lists the person's passkeys for that site and they confirm with \
         Touch ID. When the tool result says the passkey is loaded, click the site's passkey \
         button and screenshot. When a signed-in page offers to ADD a passkey and the person \
         asked for one, do the same with passkeyMode \"register\", then click the site's create \
         button once the holder is ready. You never see a key. Captcha or a page outside this box \
         is not another password form: the person finishes on the computer (Open the screen). If they \
         dismiss or decline, continue without those credentials and do not loop. When tools are \
         offered, call one; do not narrate a plan of the work instead of starting a tool. \
         Never chat I'll / First I'll / The X isn't answering. After a listing, answer with facts. \
         After a failed tool, retry once silently or say one short failure fact — never a diary. \
         Never claim create or save until the call that writes has returned ok."
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
                 screen, so say what you see and what you are doing. DO WHAT WAS ASKED, THEN \
                 STOP. Finishing is a step: when the thing you were asked for is on the screen, \
                 say so and stop, rather than looking for the next thing that could be done. \
                 Opening what you found, dismissing what appeared, or tidying up afterwards are \
                 new requests, and they are the user's to make. If an action did not do what you \
                 expected, do NOT repeat it unchanged — the same action on the same screen gives \
                 the same result. Say what you see instead, and either try something different \
                 or ask.",
            );
        }
        if has_screen && has_recipes {
            // `run_recipe` is offered only with a screen and at least one grant; the prompt says
            // it under the same condition.
            prompt.push_str(
                " You have TAUGHT RECIPES for this computer: tasks a person showed you step by \
                 step. When a request matches a recipe's description, run it with `run_recipe` \
                 (one call, the whole task) instead of clicking through it yourself, then read \
                 the screenshot it returns and say which recipe you used. If it stops part way, \
                 say at which step and finish by hand with `computer`. RUN A GIVEN RECIPE AT \
                 MOST ONCE PER REQUEST. A recipe replays a fixed sequence, so running it again \
                 repeats what it already did rather than correcting it: a second run types the \
                 same words into a field that already holds them. If the screenshot does not \
                 show what you expected, say what you actually see and either finish by hand or \
                 ask — never play the recipe again.",
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

/// Who the coworker is talking with and what day it is, opening the tail (#193).
///
/// FROM THE BEARER'S ACCOUNT, NEVER FROM THE BODY (CLAUDE.md #7): an org-shared coworker holds a
/// conversation with each member, and the only thing that may say which member this is is the
/// token. THE DAY, NOT THE TIME: the line is captured in `Started.system`, so a run resumed
/// tomorrow keeps the day it began on, and a clock would change the system message — and every
/// provider's prompt cache with it — on every turn. The zone is said out loud because "today" is
/// only true somewhere, and nothing on an account says where a person is yet.
#[must_use]
pub fn speaker_line(who: &str, today: chrono::NaiveDate, zone: &str) -> String {
    let day = today.format("%Y-%m-%d, %A");
    match who {
        "" => format!("Today is {day} ({zone})."),
        who => format!("You are talking with {who}. Today is {day} ({zone})."),
    }
}

/// What a person is called here: the name on their account, or the address they signed in with.
/// On one line and bounded, because it lands in a system message and a name is not a place to
/// open a paragraph of instructions.
#[must_use]
pub fn called(first: &str, last: &str, email: &str) -> String {
    let name = format!("{first} {last}");
    let name = if name.trim().is_empty() { email } else { &name };
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(80)
        .collect()
}

/// A routine's opening line: nobody is talking, the coworker acts for whoever hired it, and the
/// moment it fired is the "now" its instruction means.
#[must_use]
pub fn routine_line(hirer: &str, fired: chrono::DateTime<chrono::Utc>) -> String {
    let hirer = match hirer {
        "" => "the person who hired you".to_string(),
        who => format!("{who}, who hired you"),
    };
    format!(
        "Nobody is talking with you: this turn is a routine you run on your own schedule for \
         {hirer}. It fired at {} UTC.",
        fired.format("%Y-%m-%d %H:%M, %A")
    )
}

/// What the bearer is called, read from their account. A read that fails names nobody — the
/// date still goes — rather than costing the turn.
pub async fn caller(state: &AgUiState, account: &opengrok_core::id::AccountId) -> String {
    match state.auth.store.load_account(account).await {
        Ok((account, _)) => called(&account.first_name, &account.last_name, &account.email),
        Err(error) => {
            tracing::warn!(%error, "could not read who is speaking for the system message");
            String::new()
        }
    }
}

/// The persona of a coworker as the run path needs it: the title from the coworker's profile, where
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
#[path = "../tests/unit/persona.rs"]
#[allow(clippy::unwrap_used)]
mod tests;
