//! Form mode: a card the chat box drives, whose boxes and values a plugin holds.
//!
//! A login or collect card is the model's own schema. A form-mode card is not: the model names
//! which return it means (`form`, `tin`, `period`) and nothing else, and the server builds the
//! card from what the plugin's card tool answers. A model that wrote the boxes could also write
//! the fingerprint or a value, and the person would then submit a return they never saw.
//!
//! WIRE. The keys below are the shared contract with the app and the BIR plugin (hexuria/opengrok
//! #220, Tier 1). `CardReply` and `SubmitReply` are the plugin's replies, transcribed from the
//! contract in snake case; the card itself is a `FormRequest` and keeps that type's camel case
//! (`formMode`, `readOnly`), as `liveHost` and `samePage` already are.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::user_form::{FormField, FormOption, FormRequest};

/// One page of the paper form, as the plugin names it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FormPage {
    pub n: u32,
    #[serde(default)]
    pub title: String,
}

/// Which return a card is: the form, the taxpayer's TIN and the period. Every plugin call
/// carries it, and the plugin refuses when another form is open, so two cards can never
/// write into one draft.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FormKey {
    pub form: String,
    pub tin: String,
    pub period: String,
}

/// Said to the model when the identity it gave is not one a plugin could open.
pub const BAD_FORM_KEY: &str = "form must be {\"form\": \"0619E\", \"tin\": \"123456789000\", \
\"period\": \"2026-09\"}: form is the BIR form number, tin is 9 to 14 digits with no dashes, and \
period is YYYY-MM, YYYY-Qn or YYYY. Call request_user_form again with formMode true and that form.";

impl FormKey {
    /// The identity out of `form` on a call or a card, or why it cannot be one. Checked here
    /// rather than left to the plugin so a typo is told to the model as a result it can fix,
    /// before any card is minted.
    pub fn parse(value: &Value) -> Result<Self, &'static str> {
        let text = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("")
                .to_string()
        };
        let key = Self {
            form: text("form"),
            tin: text("tin"),
            period: text("period"),
        };
        let form_ok = (1..=16).contains(&key.form.len())
            && key
                .form
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-');
        let tin_ok =
            (9..=14).contains(&key.tin.len()) && key.tin.chars().all(|c| c.is_ascii_digit());
        if form_ok && tin_ok && period_ok(&key.period) {
            Ok(key)
        } else {
            Err(BAD_FORM_KEY)
        }
    }

    #[must_use]
    pub fn to_value(&self) -> Value {
        json!({ "form": self.form, "tin": self.tin, "period": self.period })
    }
}

/// `YYYY-MM`, `YYYY-Qn` or `YYYY`.
fn period_ok(period: &str) -> bool {
    let bytes = period.as_bytes();
    let year = |at: usize| {
        bytes
            .get(at..at + 4)
            .is_some_and(|y| y.iter().all(u8::is_ascii_digit))
    };
    match bytes.len() {
        4 => year(0),
        7 if bytes[4] == b'-' && year(0) => match &period[5..] {
            quarter if quarter.starts_with('Q') => matches!(&quarter[1..], "1" | "2" | "3" | "4"),
            month => month
                .parse::<u8>()
                .is_ok_and(|m| (1..=12).contains(&m) && month.len() == 2),
        },
        _ => false,
    }
}

/// Whether a call's arguments or a card's `formRequest` ask for form mode.
#[must_use]
pub fn is_form_mode_source(source: &Value) -> bool {
    source.get("formMode").and_then(Value::as_bool) == Some(true)
}

/// Whether a transcript entry is a form-mode card.
#[must_use]
pub fn is_form_mode_entry(entry: &Value) -> bool {
    crate::user_form::is_user_form_entry(entry)
        && entry
            .pointer("/message/formRequest")
            .is_some_and(is_form_mode_source)
}

/// The plugin name a call or card gives, when it is one a plugin could have.
fn plugin_of(source: &Value) -> Option<String> {
    source
        .get("plugin")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| {
            !name.is_empty()
                && name.len() <= 64
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        })
        .map(str::to_string)
}

/// What a form-mode call keeps: the flag, a title, the plugin it names, and its identity when
/// that parses. Values, fields and a fingerprint the model wrote are dropped here, before the
/// call is journaled or executed.
#[must_use]
pub fn sanitize_request(source: &Value) -> Value {
    let mut body = json!({ "formMode": true });
    if let Some(title) = source
        .get("title")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
    {
        body["title"] = json!(title);
    }
    if let Some(plugin) = plugin_of(source) {
        body["plugin"] = json!(plugin);
    }
    if let Some(key) = source
        .get("form")
        .and_then(|form| FormKey::parse(form).ok())
    {
        body["form"] = key.to_value();
    }
    body
}

/// What a form-mode `request_user_form` answers at once: the turn is to end, and the card is the
/// person's. The model is told not to fill or save behind the card, which would race the
/// person's own saves, and that it hears the outcome on a later turn.
pub const FORM_OPEN: &str = "The form is open in the chat. End your turn now. Do not ask for its \
boxes in chat and do not call bir_form_fill or bir_form_save while it is open; the person fills \
it and every change saves itself. You will be told when it is submitted or dismissed.";

/// Said when the app that sent this turn cannot draw a form-mode card.
pub const FORM_MODE_UNAVAILABLE: &str = "Form mode is not available in this app, so no form \
was opened. Ask for what you need in chat, or use the plugin's own tools.";

/// The answer to a form-mode call (already sanitised), or why it is refused. `drawn` is whether
/// the client that sent the turn draws form-mode cards.
pub fn open_answer(arguments: &Value, drawn: bool) -> Result<&'static str, &'static str> {
    if !drawn {
        return Err(FORM_MODE_UNAVAILABLE);
    }
    match arguments.get("form").map(FormKey::parse) {
        Some(Ok(_)) => Ok(FORM_OPEN),
        _ => Err(BAD_FORM_KEY),
    }
}

/// A string, number or bool as text. Objects are not a box's value.
fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

fn text_of(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn options_of(field: &Value) -> Vec<FormOption> {
    field
        .get("options")
        .and_then(Value::as_array)
        .map(|options| {
            options
                .iter()
                .filter_map(|option| {
                    // A bare string is its own label.
                    if let Some(value) = option.as_str() {
                        return Some(FormOption {
                            value: value.to_string(),
                            label: value.to_string(),
                        });
                    }
                    let value = option.get("value").and_then(scalar)?;
                    let label = text_of(option, "label").unwrap_or_else(|| value.clone());
                    Some(FormOption { value, label })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The keys a field keeps beyond a login card's: choices and an error on a collect or form-mode
/// card (`chosen`), and the rest only in form mode.
pub(crate) fn field_extras(field: &Value, chosen: bool, form_mode: bool) -> FormField {
    if !chosen {
        return FormField::default();
    }
    let mut extras = FormField {
        options: options_of(field),
        error: text_of(field, "error"),
        ..FormField::default()
    };
    if form_mode {
        extras.value = field.get("value").and_then(scalar);
        extras.page = field
            .get("page")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok());
        extras.part = field.get("part").and_then(scalar);
        extras.line = field.get("line").and_then(scalar);
        extras.read_only = field.get("readOnly").and_then(Value::as_bool) == Some(true);
    }
    extras
}

fn pages_of(source: &Value) -> Vec<FormPage> {
    source
        .get("pages")
        .and_then(Value::as_array)
        .map(|pages| {
            pages
                .iter()
                .filter_map(|page| {
                    let n = u32::try_from(page.get("n").and_then(Value::as_u64)?).ok()?;
                    let title = text_of(page, "title").unwrap_or_default();
                    Some(FormPage { n, title })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn notes_of(source: &Value) -> Vec<String> {
    source
        .get("notes")
        .and_then(Value::as_array)
        .map(|notes| {
            notes
                .iter()
                .filter_map(Value::as_str)
                .filter(|note| !note.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The keys a card keeps beyond a login card's, read back from a stored form-mode card.
pub(crate) fn request_extras(source: &Value, form_mode: bool) -> FormRequest {
    if !form_mode {
        return FormRequest::default();
    }
    FormRequest {
        form_mode: true,
        plugin: plugin_of(source),
        pages: pages_of(source),
        notes: notes_of(source),
        focus: text_of(source, "focus"),
        fingerprint: text_of(source, "fingerprint"),
        form: source
            .get("form")
            .and_then(|form| FormKey::parse(form).ok()),
        status: text_of(source, "status"),
        ..FormRequest::default()
    }
}

/// A `first_error` of a plugin reply: the box it is on and what it says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FirstError {
    pub key: String,
    pub message: String,
}

/// `first_error` of a CardReply or SubmitReply, when there is one.
#[must_use]
pub fn first_error(reply: &Value) -> Option<FirstError> {
    let error = reply.get("first_error").filter(|error| !error.is_null())?;
    Some(FirstError {
        key: text_of(error, "key").unwrap_or_default(),
        message: text_of(error, "message")?,
    })
}

/// The card for a plugin's CardReply. `Err` says, in words a person and a model can read, why
/// the reply is not a card. The plugin's own key for a box is the field id, so a save sends back
/// exactly the keys it gave.
pub fn card_request(plugin: &str, reply: &Value) -> Result<FormRequest, String> {
    let form = reply
        .get("form")
        .ok_or("the plugin's card has no form")
        .and_then(FormKey::parse)?;
    let fields = reply
        .get("fields")
        .and_then(Value::as_array)
        .ok_or("the plugin's card has no fields")?;
    let error = first_error(reply);
    let fields = fields
        .iter()
        .filter_map(|field| {
            let id = text_of(field, "key")?;
            Some(FormField {
                label: text_of(field, "label").unwrap_or_else(|| id.clone()),
                r#type: text_of(field, "kind").unwrap_or_else(|| "text".to_string()),
                required: field.get("required").and_then(Value::as_bool) == Some(true),
                options: options_of(field),
                value: field.get("value").and_then(scalar),
                error: error
                    .as_ref()
                    .filter(|error| error.key == id)
                    .map(|error| error.message.clone()),
                page: field
                    .get("page")
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok()),
                part: field.get("part").and_then(scalar),
                line: field.get("line").and_then(scalar),
                read_only: field.get("read_only").and_then(Value::as_bool) == Some(true),
                id,
                ..FormField::default()
            })
        })
        .collect::<Vec<_>>();
    let mut notes = notes_of(reply);
    // An error on no box on the card would otherwise be shown nowhere.
    if let Some(error) = error
        .as_ref()
        .filter(|error| !fields.iter().any(|field| field.id == error.key))
    {
        notes.insert(0, error.message.clone());
    }
    Ok(FormRequest {
        title: text_of(reply, "title").unwrap_or_else(|| format!("Form {}", form.form)),
        fields,
        form_mode: true,
        plugin: Some(plugin.to_string()),
        pages: pages_of(reply),
        notes,
        focus: error.map(|error| error.key).filter(|key| !key.is_empty()),
        fingerprint: text_of(reply, "fingerprint"),
        form: Some(form),
        status: text_of(reply, "status"),
        ..FormRequest::default()
    })
}

/// What a submit came to, from the plugin's SubmitReply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// In the plugin's queue: it goes at the queue's next tick unless cancelled first.
    Queued {
        queue_ref: String,
        fingerprint: Option<String>,
        values: serde_json::Map<String, Value>,
    },
    /// Nothing was queued; the first error says why.
    Refused {
        first_error: Option<FirstError>,
        fingerprint: Option<String>,
    },
}

/// Read a SubmitReply. A reply that is neither shape is a refusal with no error: nothing was
/// said to be queued, so nothing is claimed to be.
#[must_use]
pub fn submit_outcome(reply: &Value) -> SubmitOutcome {
    let fingerprint = text_of(reply, "fingerprint");
    match (
        reply.get("queued").and_then(Value::as_bool),
        text_of(reply, "queue_ref"),
    ) {
        (Some(true), Some(queue_ref)) => SubmitOutcome::Queued {
            queue_ref,
            fingerprint,
            values: reply
                .get("values")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
        },
        _ => SubmitOutcome::Refused {
            first_error: first_error(reply),
            fingerprint,
        },
    }
}

/// The most of a submitted return's values one line tells the model: a long return would
/// otherwise become every later prompt.
const OUTCOME_VALUES_CHARS: usize = 1_500;

/// What the Bot is told about a form-mode card on its next turn: open, dismissed, submitted, or
/// never opened. `None` for any other entry.
///
/// A form-mode card's call answered before the card had an outcome, so the outcome reaches the
/// model the way a later turn reads the transcript (`user_form::history_line`), not as that
/// call's result: writing a result into a finished run's log is a journal write the run
/// protocol does not have.
#[must_use]
pub fn history_line(entry: &Value) -> Option<String> {
    if !is_form_mode_entry(entry) {
        return None;
    }
    let form = crate::user_form::form_request_from(entry);
    let named = match form.form.as_ref() {
        Some(key) => format!(
            "\"{}\" ({} for TIN {}, {})",
            form.title, key.form, key.tin, key.period
        ),
        None => format!("\"{}\"", form.title),
    };
    let outcome = entry.get("formOutcome").unwrap_or(&Value::Null);
    let line = match entry.get("formResolution").and_then(Value::as_str) {
        Some("submitted") => {
            let queue_ref = text_of(outcome, "queueRef").unwrap_or_default();
            let values = outcome
                .get("values")
                .and_then(Value::as_object)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|(key, value)| Some(format!("{key}={}", scalar(value)?)))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            let mut values: String = values.chars().take(OUTCOME_VALUES_CHARS).collect();
            if values.is_empty() {
                values = "none recorded".to_string();
            }
            format!(
                "The person submitted the form {named}. It is queued to send as {queue_ref} at \
                 the queue's next tick and can be cancelled until then; it is not paid and BIR \
                 has not confirmed it. Values sent: {values}. Do not submit it again."
            )
        }
        Some("dismissed") => {
            let at = text_of(outcome, "lastFocus")
                .map(|key| format!(" They were last on {key}."))
                .unwrap_or_default();
            format!(
                "The person closed the form {named} without submitting it.{at} Its draft is \
                 saved and they can resume it from the chat. Do not fill or submit it for them \
                 unless they ask."
            )
        }
        Some("fill_failed") => format!(
            "The form {named} could not be opened: {}",
            form.notes
                .first()
                .map_or("the plugin did not say why.", String::as_str)
        ),
        _ => format!(
            "The form {named} is open in the chat and the person is filling it. Do not ask for \
             its boxes in chat and do not fill or save it yourself."
        ),
    };
    Some(line)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "../tests/unit/form_mode.rs"]
mod tests;
