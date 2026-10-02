//! The routine tools (#316): a Bot lists, makes, edits and deletes its person's routines when
//! they ask in chat. A Bot that had none faked one with a loop on its box; these are the real
//! thing, through the one desk `POST`, `PATCH` and `DELETE /schedules` use (opengrok-server
//! `autonomy/desk.rs`), so a tool may store what the routes may, refused in the same words.
//!
//! RUN INSIDE `Executor::execute`, NOT BESIDE IT: the ceiling, the grant and an ask apply to them
//! as to every built-in, and none of them touches the box. One ceiling row switches all four
//! (`ROW`). A delete ALWAYS asks first, on the policy's card, naming the routine as stored —
//! never by a name the model wrote.
//!
//! THE ACCOUNT IS NEVER AN ARGUMENT. The desk is asked as the `ToolContext`'s account, and no key
//! a call writes can move it (CLAUDE.md #7). The one target a call names is `bot`, which the
//! desk finds among the Bots that person OWNS, or refuses.
//!
//! THE WORDS ARE THE CONTRACT'S (hexuria/opengrok-server#316, "Contract of record", agreed with
//! NativeChat on 2 Oct 2026, and the owner's rules after it): the names, the arguments, and the
//! refusals below. One cron per routine until #315 gives a routine several wakes.

use serde_json::{Value, json};

use crate::{ToolContext, ToolResult};

pub const LIST_ROUTINES: &str = "list_routines";
pub const CREATE_ROUTINE: &str = "create_routine";
pub const UPDATE_ROUTINE: &str = "update_routine";
pub const DELETE_ROUTINE: &str = "delete_routine";
/// The four, in the order they are offered.
pub const TOOLS: [&str; 4] = [
    LIST_ROUTINES,
    CREATE_ROUTINE,
    UPDATE_ROUTINE,
    DELETE_ROUTINE,
];

/// The ceiling row that switches all four (`GET`/`PUT /coworkers/{id}/ceiling`).
pub const ROW: &str = "routines";
pub const ROW_LABEL: &str = "Routines";
pub const ROW_DESCRIPTION: &str = "List, make, edit and delete your routines when you ask in \
     chat. Deleting one always asks you first.";

/// A `when` that is missing: the time and days are the person's to say, never the model's to guess.
pub const NO_WHEN: &str = "ask the person for the time and days with request_user_form first.";
/// More than one cron: a routine has one wake until #315.
pub const ONE_SCHEDULE: &str =
    "a routine has one schedule for now; make a second routine for another time";
/// A webhook: its key would pass through the chat, and its log, to be of any use.
pub const NO_WEBHOOK: &str = "a Bot can't make a webhook trigger: its key must not pass through \
     chat. Ask the person to add one in Routines.";
/// What a routine made for a Bot on the person's own plan says (#316): it runs only while the
/// plan can answer, by the way the plan goes, and is skipped otherwise.
pub const ONLY_WHILE: [&str; 2] = [
    "runs only while your computer is on",
    "runs only while your plan's proxy answers",
];

/// A routine id the person does not own, answered as unknown whether or not it exists.
pub fn not_yours(routine: &str) -> String {
    format!("no routine {routine} is yours; call list_routines.")
}

/// One of the person's own Bots, as a call names it: `label` is its name, or `Name (cw_…)` when
/// two of theirs share it. `on_plan` is a Bot whose own door is the person's plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bot {
    pub id: String,
    pub label: String,
    pub on_plan: bool,
}

/// The person's own Bots, `(id, name, on_plan)`, labelled as calls name them.
pub fn bots(owned: Vec<(String, String, bool)>) -> Vec<Bot> {
    let shared = |name: &str| owned.iter().filter(|(_, other, _)| other == name).count() > 1;
    let label = |id: &str, name: &str| match shared(name) {
        true => format!("{name} ({id})"),
        false => name.to_string(),
    };
    let bots = owned.iter().map(|(id, name, on_plan)| Bot {
        id: id.clone(),
        label: label(id, name),
        on_plan: *on_plan,
    });
    bots.collect()
}

/// The Bot a call names among the person's OWN, by id or label, then in any case when only one
/// matches; or, named by nobody, the session's own, which must be theirs too. Never a Bot from
/// anywhere else: the refusal lists theirs.
pub fn resolve<'a>(bots: &'a [Bot], session: &str, asked: Option<&str>) -> Result<&'a Bot, String> {
    let found: Vec<&Bot> = match asked {
        None => bots.iter().filter(|bot| bot.id == session).collect(),
        Some(asked) => match bots
            .iter()
            .find(|bot| bot.id == asked || bot.label == asked)
        {
            Some(bot) => vec![bot],
            None => bots
                .iter()
                .filter(|bot| bot.label.eq_ignore_ascii_case(asked))
                .collect(),
        },
    };
    let yours: Vec<&str> = bots.iter().map(|bot| bot.label.as_str()).collect();
    let yours = yours.join(", ");
    match (found.as_slice(), asked) {
        ([bot], _) => Ok(bot),
        ([], None) => Err(format!(
            "this Bot is not yours, so it cannot hold your routine; name one of yours as bot: {yours}"
        )),
        ([], Some(asked)) => Err(format!(
            "no Bot of yours is called \"{asked}\"; yours are: {yours}"
        )),
        _ => Err(format!(
            "more than one of your Bots answers to that; name one by id: {yours}"
        )),
    }
}

/// One routine as `list_routines` and every other answer carry it: `when` its one cron in the
/// person's 5-field form (none on a webhook routine), and never a webhook's key or address.
pub fn row(view: &opengrok_core::schedule::ScheduleView, bots: &[Bot]) -> Value {
    let bot = bots.iter().find(|bot| bot.id == view.coworker_id.as_str());
    let webhook = view.kind == opengrok_core::schedule::WakeKind::Webhook;
    let when = (!webhook).then(|| opengrok_core::schedule::display_cron(&view.cron));
    json!({ "id": view.id, "name": view.name,
        "bot": bot.map_or(view.coworker_id.as_str(), |bot| bot.label.as_str()),
        "prompt": view.prompt, "active": view.active, "when": when.into_iter().collect::<Vec<_>>(),
        "tz": view.tz, "nextDueMs": view.next_due_ms, "webhook": webhook })
}

pub fn is_routine_tool(name: &str) -> bool {
    TOOLS.contains(&name)
}

/// What a create or an update says, read: `None` is a field it left out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fields {
    pub name: Option<String>,
    pub prompt: Option<String>,
    /// One cron, as the person would write it.
    pub when: Option<String>,
    pub tz: Option<String>,
    /// One of the person's own Bots, by name or id.
    pub bot: Option<String>,
    pub active: Option<bool>,
}

/// One call, read and checked as far as its arguments go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ask {
    List,
    Create(Fields),
    Update { routine: String, fields: Fields },
    Delete { routine: String },
}

/// The person's routines, as the server keeps them. Every call is answered as `context`'s
/// account, never one an argument names, and a refusal is a sentence the model can act on.
#[async_trait::async_trait]
pub trait RoutineDesk: Send + Sync {
    /// Carry out `ask`: what the model is told back, or why not.
    async fn answer(&self, context: &ToolContext, ask: Ask) -> Result<Value, String>;
    /// The name a routine of `context`'s account is stored under, for a delete's card; for any
    /// other id, the refusal that hides whether it exists.
    async fn stored_name(&self, context: &ToolContext, routine: &str) -> Result<String, String>;
}

/// What `Executor::execute` holds before its gates, for a routine tool: the call read, and for
/// a delete the card's sentence; or the refusal it earns now, before any card is raised.
/// `listing` is a run a routine started, which may only list (`with_routines_listing_only`).
pub async fn admit(
    desk: &dyn RoutineDesk,
    listing: bool,
    context: &ToolContext,
    (name, arguments): (&str, &Value),
) -> Result<(Ask, Option<String>), String> {
    let ask = read(name, arguments)?;
    if listing && ask != Ask::List {
        return Err(
            "a run a routine started may only list routines; making or changing one waits for \
             the person"
                .to_string(),
        );
    }
    let Ask::Delete { routine } = &ask else {
        return Ok((ask, None));
    };
    let stored: String = desk
        .stored_name(context, routine)
        .await?
        .chars()
        .take(80)
        .collect();
    let why = format!("Delete the routine \"{stored}\"? It stops for good.");
    Ok((ask, Some(why)))
}

/// Carry out an admitted call: its JSON, or the refusal, as a result either way (CLAUDE.md #8).
pub async fn run(
    desk: &dyn RoutineDesk,
    context: &ToolContext,
    call_id: &str,
    ask: Ask,
) -> ToolResult {
    match desk.answer(context, ask).await {
        Ok(answer) => ToolResult::ok(call_id, answer.to_string()),
        Err(why) => ToolResult::refused(call_id, why),
    }
}

/// A call's arguments, read: the shape each tool takes, and the contract's refusals for a `when`
/// that is missing, more than one cron, or a webhook. The identity keys `overwrite_identity`
/// writes are never read: nothing here takes an account or a coworker but `bot`.
fn read(name: &str, arguments: &Value) -> Result<Ask, String> {
    let text = |key: &str| -> Result<Option<String>, String> {
        match arguments.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(text)) => {
                Ok(Some(text.trim().to_string()).filter(|t| !t.is_empty()))
            }
            Some(_) => Err(format!("bad arguments: {key} must be a string")),
        }
    };
    let fields = || -> Result<Fields, String> {
        let hook = arguments
            .get("webhook")
            .is_some_and(|hook| !matches!(hook, Value::Null | Value::Bool(false)));
        if hook || arguments.get("kind").and_then(Value::as_str) == Some("webhook") {
            return Err(NO_WEBHOOK.to_string());
        }
        let active = match arguments.get("active") {
            None | Some(Value::Null) => None,
            Some(Value::Bool(active)) => Some(*active),
            Some(_) => return Err("bad arguments: active must be true or false".to_string()),
        };
        Ok(Fields {
            name: text("name")?,
            prompt: text("prompt")?,
            when: when(arguments.get("when"))?,
            tz: text("tz")?,
            bot: text("bot")?,
            active,
        })
    };
    let routine = || {
        text("routine")?.ok_or_else(|| {
            format!("bad arguments: name the routine by its id, as {LIST_ROUTINES} gives it")
        })
    };
    match name {
        LIST_ROUTINES => Ok(Ask::List),
        CREATE_ROUTINE => {
            let fields = fields()?;
            if fields.when.is_none() {
                return Err(NO_WHEN.to_string());
            }
            if fields.prompt.is_none() {
                return Err("bad arguments: prompt says what the Bot is told each time".to_string());
            }
            Ok(Ask::Create(fields))
        }
        UPDATE_ROUTINE => {
            let (routine, fields) = (routine()?, fields()?);
            if fields == Fields::default() {
                let why = "nothing to change: give name, prompt, when, tz, bot or active";
                return Err(why.to_string());
            }
            Ok(Ask::Update { routine, fields })
        }
        DELETE_ROUTINE => Ok(Ask::Delete {
            routine: routine()?,
        }),
        other => Err(format!("there is no routine tool called {other}")),
    }
}

/// `when`: one cron, as a string or a list of one. More than one is #315's, not yet; a webhook
/// wake (#315's `{kind: "webhook"}`) is the person's to add.
fn when(given: Option<&Value>) -> Result<Option<String>, String> {
    let wakes: Vec<&Value> = match given {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(wakes)) => wakes.iter().collect(),
        Some(one) => vec![one],
    };
    let mut crons = Vec::new();
    for wake in wakes {
        let cron = match wake {
            Value::String(cron) => Some(cron.as_str()),
            Value::Object(wake) if wake.get("kind").and_then(Value::as_str) == Some("webhook") => {
                return Err(NO_WEBHOOK.to_string());
            }
            Value::Object(wake) => wake.get("cron").and_then(Value::as_str),
            _ => None,
        };
        match cron.map(str::trim) {
            Some("webhook") => return Err(NO_WEBHOOK.to_string()),
            Some(cron) if !cron.is_empty() => crons.push(cron.to_string()),
            _ => {
                return Err(
                    "bad arguments: when is one 5-field cron, like \"0 9 * * MON-FRI\"".to_string(),
                );
            }
        }
    }
    match crons.len() {
        0 => Ok(None),
        1 => Ok(crons.pop()),
        _ => Err(ONE_SCHEDULE.to_string()),
    }
}

/// What a tool says it is for, as a turn offers it and a ceiling describes it; the group's row
/// for `ROW`.
pub fn description(name: &str) -> Option<&'static str> {
    Some(match name {
        LIST_ROUTINES => {
            "List your person's routines: each one's id, name, the Bot it wakes, its prompt, \
             whether it is on, when it wakes (a 5-field cron), the time zone that is read in, and \
             nextDueMs (epoch milliseconds). Webhook keys and addresses are never listed."
        }
        CREATE_ROUTINE => {
            "Make a routine: one of your person's Bots is woken on a schedule and told its prompt, \
             with nobody watching. `when` is REQUIRED and is never guessed: if the person has not \
             said both the time and the days, call request_user_form first with a field for each, \
             then make it with what they answer. A routine wakes at most once a minute."
        }
        UPDATE_ROUTINE => {
            "Change one of your person's routines, by its id from list_routines: only what you \
             pass changes. `when` replaces its schedule; `active` false pauses it, true wakes it."
        }
        DELETE_ROUTINE => {
            "Delete one of your person's routines for good, by its id from list_routines. It \
             always asks the person first, naming the routine."
        }
        ROW => ROW_DESCRIPTION,
        _ => return None,
    })
}

/// The function definition a turn offers for `name`.
pub fn schema(name: &str) -> Option<Value> {
    let description = description(name)?;
    let string = |what: &str| json!({ "type": "string", "description": what });
    let routine = string("The routine's id, as list_routines gives it.");
    let fields = json!({
        "name": string("What to call it. Left out on a new one, its prompt's first words."),
        "prompt": string("What the Bot is told each time it wakes, in full: it sees nothing \
            of this chat then."),
        "when": string("When it wakes: ONE 5-field cron, minute hour day-of-month month \
            day-of-week, like \"0 9 * * MON-FRI\" for 9:00 on weekdays."),
        "tz": string("The IANA time zone the cron is read in, like \"Asia/Manila\". Left out \
            on a new one, your person's own zone."),
        "bot": string("Which of your person's own Bots it wakes, by name or id. Left out on a \
            new one, you."),
    });
    let (properties, required) = match name {
        LIST_ROUTINES => (json!({}), json!([])),
        CREATE_ROUTINE => (fields, json!(["prompt", "when"])),
        UPDATE_ROUTINE => {
            let mut properties = fields;
            properties["routine"] = routine;
            properties["active"] = json!({ "type": "boolean",
                "description": "false pauses it; true wakes it again." });
            (properties, json!(["routine"]))
        }
        DELETE_ROUTINE => (json!({ "routine": routine }), json!(["routine"])),
        _ => return None,
    };
    let parameters = json!({ "type": "object", "properties": properties, "required": required });
    Some(json!({ "type": "function",
        "function": { "name": name, "description": description, "parameters": parameters } }))
}

#[cfg(test)]
#[path = "../tests/unit/routine.rs"]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
