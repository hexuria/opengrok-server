//! In-chat `user-form`: the field schema, secret stripping, and the fill that types into the
//! box **outside** `computer_use`.
//!
//! WHY THIS IS NOT A SECRET DROP. The desktop's `submitSecret` (gone with seam A) stamped
//! `secretProvided: true` and DROPPED the value into the connector vault; this path types into
//! the live page. Mixing the two would vault a Google password or fill a secret into Chromium.
//!
//! WHY THIS IS NOT `computer` type. `computer_use` always attaches a PNG after the action, so a
//! password typed that way lands in the model payload. Fill calls `Computer::act(Type)` and
//! never screenshots.
//!
//! hexuria/box today is X11 type-into-focus (`POST /v1/cua/type`), not Playwright aria-ref.
//! Some logins are stepped (Google: email page, then password); most show both fields on one
//! page (Facebook). Typing into whatever the browser has focused mis-focuses on a combined
//! form; CUA still succeeds and we used to report `submitted` while the password landed in the
//! wrong box. So a field may carry `at`, its position on the model's screenshot: the fill
//! clicks it before typing. Without positions the old rules hold: default types only the
//! first focused field; `samePage: true` allows Tab; `submit: true` allows Return.

use std::collections::{BTreeMap, BTreeSet};

use opengrok_box::{Computer, CuaAction};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The builtin the model calls to raise the card. Activity: "Waiting for you".
pub const REQUEST_USER_FORM: &str = "request_user_form";

/// Transcribed field types from official 0.29/0.30 `user-form/view.tsx`.
pub const SECRET_TYPES: &[&str] = &["password", "otp"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FormField {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub required: bool,
    /// Explicit mask. `password` / `otp` types are secret even when this is absent.
    #[serde(default)]
    pub secret: bool,
    /// Where the field is on the box's screen, in the pixels of the model's own screenshot.
    /// With it, the fill clicks the field before typing; without it, the fill types into
    /// whatever the browser has focused — which is how a password once landed in the email box
    /// after Facebook redrew its page with an error (21 Sep 2026).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<FieldAt>,
}

/// A point on the box's screen, as the model saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldAt {
    pub x: i32,
    pub y: i32,
}

impl FormField {
    #[must_use]
    pub fn is_secret(&self) -> bool {
        self.secret
            || SECRET_TYPES
                .iter()
                .any(|kind| kind.eq_ignore_ascii_case(&self.r#type))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FormRequest {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub instruction: String,
    #[serde(default)]
    pub fields: Vec<FormField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(default, rename = "liveHost", skip_serializing_if = "Option::is_none")]
    pub live_host: Option<String>,
    /// Hint for the next challenge: `password`, `otp`, `captcha`, `passkey`, `outside_sandbox`.
    /// Optional; the model may omit it. Captcha / passkey / outside-sandbox must not be another
    /// password form — those go to box handoff.
    #[serde(
        default,
        rename = "challengeKind",
        skip_serializing_if = "Option::is_none"
    )]
    pub challenge_kind: Option<String>,
    /// With `challengeKind: "passkey"`: `use` (the site offers to sign in with a passkey the
    /// person has) or `register` (the site offers to add one). No fields either way.
    #[serde(
        default,
        rename = "passkeyMode",
        skip_serializing_if = "Option::is_none"
    )]
    pub passkey_mode: Option<String>,
    /// Fields share one HTML page: Tab between them. Default false — type only the first
    /// focused field so a stepped login (Facebook email, then password) cannot mistype.
    #[serde(
        default,
        rename = "samePage",
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub same_page: bool,
    /// Press Return after a successful fill. Default false; a single field, or otp/password
    /// with exactly one typed field, still Returns. Combined forms must set this explicitly.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub submit: bool,
}

/// What the model reads when the person hands the computer back after Open-the-screen.
/// Observe; do not claim login. A second `request_user_form` is for an in-sandbox OTP, not a
/// replay of the settled card.
pub const HAND_BACK_TOOL_RESULT: &str = "Person finished on computer. Screenshot and confirm login; if another challenge, call request_user_form.";

/// Person closed the handoff without finishing. Short-circuit: do not loop the form.
pub const HANDOFF_DECLINED_TOOL_RESULT: &str = "The person declined to finish on the computer. Continue without those credentials; do not type secrets with `computer`, and do not raise the same form again.";

/// Form or handoff wait ran out so the turn cannot hang (Facebook after password when phone
/// verify hits).
pub const HOLD_TIMED_OUT_TOOL_RESULT: &str = "The wait timed out. Continue without those credentials; do not type secrets with `computer`, and do not raise the same form again.";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FieldOutcome {
    pub id: String,
    pub filled: bool,
    #[serde(rename = "fillFailed")]
    pub fill_failed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormResolution {
    Submitted,
    FillFailed,
    Dismissed,
    Escalated,
}

impl FormResolution {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Submitted => "submitted",
            Self::FillFailed => "fill_failed",
            Self::Dismissed => "dismissed",
            Self::Escalated => "escalated",
        }
    }
}

/// `at: {x, y}` on a field, when the model gave one and both numbers are there. Models emit
/// floats even for an integer schema, so `640.0` is a point; a point outside any screen the
/// box could have is no point (the guest would refuse it, and the fill would then give up).
fn field_at(field: &Value) -> Option<FieldAt> {
    let at = field.get("at")?;
    let number = |key: &str| {
        at.get(key)
            .and_then(Value::as_f64)
            .filter(|n| n.is_finite() && (0.0..=10_000.0).contains(n))
            .map(|n| n.round() as i32)
    };
    Some(FieldAt {
        x: number("x")?,
        y: number("y")?,
    })
}

/// Every field of the form knows where it is: the fill clicks each one, so the focus
/// ambiguity the first-field-only default exists for does not arise.
#[must_use]
pub fn fully_positioned(form: &FormRequest) -> bool {
    !form.fields.is_empty() && form.fields.iter().all(|field| field.at.is_some())
}

/// Pull a form out of tool arguments or a transcript `formRequest`, ignoring identity keys the
/// executor stamps and any values the model must never keep.
#[must_use]
pub fn form_request_from(value: &Value) -> FormRequest {
    let source = form_source(value);
    let fields = source
        .get("fields")
        .and_then(Value::as_array)
        .map(|fields| {
            fields
                .iter()
                .filter_map(|field| {
                    let id = field.get("id").and_then(Value::as_str)?.to_string();
                    if id.is_empty() {
                        return None;
                    }
                    Some(FormField {
                        id,
                        label: field
                            .get("label")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        r#type: field
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        required: field
                            .get("required")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        secret: field
                            .get("secret")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        at: field_at(field),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    FormRequest {
        title: source
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        instruction: source
            .get("instruction")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        fields,
        domain: source
            .get("domain")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        live_host: source
            .get("liveHost")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        challenge_kind: source
            .get("challengeKind")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        passkey_mode: source
            .get("passkeyMode")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        same_page: source
            .get("samePage")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        submit: source
            .get("submit")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

/// Arguments stored on the suspension / card: field defs only. Identity keys and any `values`
/// the model smuggled are dropped so a password cannot sit on the entry waiting for submit.
#[must_use]
pub fn sanitize_arguments(arguments: &Value) -> Value {
    let form = form_request_from(arguments);
    let mut body = json!({
        "title": form.title,
        "instruction": form.instruction,
        "fields": form.fields,
    });
    if let Some(domain) = form.domain {
        body["domain"] = json!(domain);
    }
    if let Some(host) = form.live_host {
        body["liveHost"] = json!(host);
    }
    if let Some(mode) = form.passkey_mode {
        body["passkeyMode"] = json!(mode);
    }
    if let Some(kind) = form.challenge_kind {
        body["challengeKind"] = json!(kind);
    }
    if form.same_page {
        body["samePage"] = json!(true);
    }
    if form.submit {
        body["submit"] = json!(true);
    }
    // A collect card is a question, not a page fill. Non-secret prefills stay so the
    // person sees what they already said. A login card still drops every value: a
    // model-smuggled email or password must not sit on the entry.
    if is_chat_collection(arguments) {
        body["collect"] = json!(true);
        if let (Some(out_fields), Some(in_fields)) = (
            body.get_mut("fields").and_then(Value::as_array_mut),
            form_source(arguments)
                .get("fields")
                .and_then(Value::as_array),
        ) {
            for field in out_fields {
                let secret = field
                    .get("secret")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    || field
                        .get("type")
                        .and_then(Value::as_str)
                        .is_some_and(|kind| {
                            SECRET_TYPES
                                .iter()
                                .any(|secret| secret.eq_ignore_ascii_case(kind))
                        });
                if secret {
                    continue;
                }
                let id = field.get("id").and_then(Value::as_str);
                let mut matching = in_fields
                    .iter()
                    .filter(|row| row.get("id").and_then(Value::as_str) == id);
                let Some(source) = matching.next() else {
                    continue;
                };
                // Submit values are keyed by id, so duplicate ids have no safe association.
                if matching.next().is_some() {
                    continue;
                }
                if let Some(value) = source.get("value").and_then(scalar_text) {
                    let value = value.trim();
                    if !value.is_empty() {
                        field["value"] = json!(value);
                    }
                }
            }
        }
    }
    body
}

/// The person is answering in chat. The values come back to the model. Nothing is typed
/// into a page. A login card leaves this false, so a missing flag cannot skip the fill.
#[must_use]
pub fn is_chat_collection(value: &Value) -> bool {
    form_source(value).get("collect").and_then(Value::as_bool) == Some(true)
}

// Cards and tool arguments use the same precedence, including a present null wrapper.
fn form_source(value: &Value) -> &Value {
    value
        .get("formRequest")
        .or_else(|| value.pointer("/message/formRequest"))
        .unwrap_or(value)
}

/// Reject ambiguous ids before a card waits: its answers are keyed by id, not by row.
pub(crate) fn validate_field_ids(arguments: &Value) -> Result<(), &'static str> {
    let mut seen = BTreeSet::new();
    if let Some(fields) = form_source(arguments)
        .get("fields")
        .and_then(Value::as_array)
    {
        for id in fields
            .iter()
            .filter_map(|field| field.get("id").and_then(Value::as_str))
            .filter(|id| !id.is_empty())
        {
            if !seen.insert(id) {
                return Err(
                    "Field IDs must be unique. Give each field a different id and call request_user_form again.",
                );
            }
        }
    }
    Ok(())
}

/// A string, number, or bool the person or the model wrote. Objects are not field text.
fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

/// What the model reads after a collect card. The answers are the point. A page was not filled.
#[must_use]
pub fn collection_tool_result(form: &FormRequest, shared: &BTreeMap<String, String>) -> String {
    let title = if form.title.is_empty() {
        "a form"
    } else {
        form.title.as_str()
    };
    let shared_line = if shared.is_empty() {
        "No fields were filled.".to_string()
    } else {
        let listed = shared
            .iter()
            .map(|(id, value)| format!("{id}={value}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("Shared fields: {listed}.")
    };
    format!(
        "The person submitted \"{title}\". Nothing was typed into a page. {shared_line} \
Use these values. Do not ask for a field they already filled."
    )
}

#[must_use]
pub fn is_user_form_entry(entry: &Value) -> bool {
    entry.pointer("/message/type").and_then(Value::as_str) == Some("user-form")
}

/// Unresolved iff `formResolution` is absent and `widgetDismissed` is not true — official 0.30.
#[must_use]
pub fn is_unresolved(entry: &Value) -> bool {
    if !is_user_form_entry(entry) {
        return false;
    }
    if entry
        .get("formResolution")
        .and_then(Value::as_str)
        .is_some_and(|word| !word.is_empty())
    {
        return false;
    }
    entry.get("widgetDismissed").and_then(Value::as_bool) != Some(true)
}

/// Live Grok Bot box handoff: `boxRequestId` present and `boxResolution` **absent**
/// (null/empty also count as live — the locked wire omits the key until resolve).
/// A stray `boxRequestId` on any other card converts that card into a handoff, so user-form
/// must never carry one — escalate emits a **separate** `sand://box` attachment.
#[must_use]
pub fn is_live_handoff(entry: &Value) -> bool {
    let Some(id) = entry.get("boxRequestId").and_then(Value::as_str) else {
        return false;
    };
    if id.is_empty() {
        return false;
    }
    match entry.get("boxResolution") {
        None | Some(Value::Null) => true,
        Some(Value::String(word)) if word.is_empty() => true,
        Some(_) => false,
    }
}

/// Screen tools must not race the person: unresolved user-form **or** a live box handoff.
/// Escalating the form settles `formResolution` but must **keep** this hold until hand-back
/// or decline — clearing it on `escalated` is the bug vs Grok Bot.
#[must_use]
pub fn holds_the_screen(entry: &Value) -> bool {
    is_unresolved(entry) || is_live_handoff(entry)
}

/// Pull submit values for THIS form's fields only. Extra keys (a smuggled password on a
/// neighbouring id, identity aliases) are dropped so they cannot be typed or stored.
#[must_use]
pub fn submitted_values(form: &FormRequest, raw: &Value) -> BTreeMap<String, String> {
    let Some(object) = raw.as_object() else {
        return BTreeMap::new();
    };
    form.fields
        .iter()
        .filter_map(|field| {
            let value = object.get(&field.id)?;
            let text = scalar_text(value)?;
            Some((field.id.clone(), text))
        })
        .collect()
}

/// Values the model may see after submit. Secret fields are omitted, not redacted — a placeholder
/// would still teach the model the password's length and that a value existed.
#[must_use]
pub fn shared_values(
    form: &FormRequest,
    values: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    form.fields
        .iter()
        .filter(|field| {
            !field.is_secret()
                && form
                    .fields
                    .iter()
                    .filter(|other| other.id == field.id)
                    .count()
                    == 1
        })
        .filter_map(|field| {
            let value = values.get(&field.id)?;
            Some((field.id.clone(), value.clone()))
        })
        .collect()
}

/// Audit: label + length only. Never the value, not even at debug.
pub fn audit_lengths(form: &FormRequest, values: &BTreeMap<String, String>) {
    for field in &form.fields {
        let Some(value) = values.get(&field.id) else {
            continue;
        };
        tracing::info!(
            field = %field.label,
            id = %field.id,
            secret = field.is_secret(),
            len = value.len(),
            "user-form field received; value not logged"
        );
    }
}

/// Does `haystack` contain any secret submit value? Used by leakage tests and as a last-chance
/// strip before a tool result is built.
#[must_use]
pub fn contains_secret_value(
    haystack: &str,
    form: &FormRequest,
    values: &BTreeMap<String, String>,
) -> bool {
    form.fields.iter().any(|field| {
        field.is_secret()
            && values
                .get(&field.id)
                .is_some_and(|value| !value.is_empty() && haystack.contains(value))
    })
}

/// What the model reads after the person answers. Secrets never appear here. Filling is not
/// login: the model must screenshot after settle and must not re-raise a settled entry.
#[must_use]
pub fn tool_result_content(
    form: &FormRequest,
    resolution: FormResolution,
    shared: &BTreeMap<String, String>,
    timed_out: bool,
) -> String {
    let title = if form.title.is_empty() {
        "a form"
    } else {
        form.title.as_str()
    };
    let secret_note = "Secret field values were typed into the page and never shown to you.";
    let observe = if types_only_first_field(form) {
        "Only the FIRST field was typed: this card had several fields but no `samePage`, and not every field had a position, so the rest could not be placed. Screenshot and confirm what the page shows now; do not claim login succeeded. If the remaining fields are on the same page, raise ONE new card for them with `samePage: true` and each field's `at`; if a password page is next, call request_user_form with a password-only form (new entryId, challengeKind \"password\"). Never re-raise a form that already settled."
    } else if fully_positioned(form) {
        "Each field was clicked at the position you gave, then typed. Screenshot and confirm what the page shows now; do not claim login succeeded until you see it. If the page had moved since your screenshot and a value landed in the wrong field, raise the card again with fresh positions. If another in-sandbox challenge (OTP, phone verification on the same page) appears, call request_user_form again with otp fields — never re-raise a form that already settled. A passkey prompt is a passkey card (challengeKind \"passkey\"); a captcha, or a page outside this box, is handoff, not another password form."
    } else {
        "Screenshot and confirm what the page shows now; do not claim login succeeded until you see it. If another in-sandbox challenge (OTP, phone verification on the same page) appears, call request_user_form again with otp fields — never re-raise a form that already settled. A passkey prompt is a passkey card (challengeKind \"passkey\"); a captcha, or a page outside this box, is handoff, not another password form."
    };
    match resolution {
        FormResolution::Submitted => {
            let shared_line = if shared.is_empty() {
                "No non-secret fields were shared.".to_string()
            } else {
                let listed = shared
                    .iter()
                    .map(|(id, value)| format!("{id}={value}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("Shared fields: {listed}.")
            };
            format!(
                "The person submitted the form \"{title}\". It was filled into the page. {observe} {shared_line} {secret_note}"
            )
        }
        FormResolution::FillFailed => {
            format!(
                "The person submitted the form \"{title}\" but it could not be filled into the page — the page may have moved or changed. {observe} {secret_note} Do not type secrets with `computer`."
            )
        }
        FormResolution::Dismissed if timed_out => HOLD_TIMED_OUT_TOOL_RESULT.to_string(),
        FormResolution::Dismissed => {
            format!(
                "The person dismissed the form \"{title}\" without filling anything. Continue without those credentials; do not type secrets with `computer`, and do not raise the same form again."
            )
        }
        FormResolution::Escalated => {
            format!(
                "The person chose to finish the form \"{title}\" on the computer. Wait for them to hand it back. Do not type secrets. Do not claim the form is done."
            )
        }
    }
}

/// Instruction shown on the separate `sand://box` attachment when the person opens the screen.
#[must_use]
pub fn handoff_instruction(form: &FormRequest) -> String {
    let title = if form.title.is_empty() {
        "this form"
    } else {
        form.title.as_str()
    };
    if form.instruction.is_empty() {
        format!("Finish {title} on the computer.")
    } else {
        format!("{title}: {}", form.instruction)
    }
}

/// The sentence a settled card gives the model. A collect card never claims a page was filled.
/// A login card still does, because that path types into the page.
#[must_use]
pub fn model_facing_result(
    entry: &Value,
    form: &FormRequest,
    resolution: FormResolution,
    shared: &BTreeMap<String, String>,
    timed_out: bool,
) -> String {
    if is_chat_collection(entry) {
        // Old settled cards can carry values saved before the current field filtering.
        let shared = shared_values(form, shared);
        collection_resolution(form, resolution, &shared, timed_out)
    } else {
        tool_result_content(form, resolution, shared, timed_out)
    }
}

fn collection_resolution(
    form: &FormRequest,
    resolution: FormResolution,
    shared: &BTreeMap<String, String>,
    timed_out: bool,
) -> String {
    let title = if form.title.is_empty() {
        "a form"
    } else {
        form.title.as_str()
    };
    match resolution {
        FormResolution::Submitted => collection_tool_result(form, shared),
        FormResolution::FillFailed => format!(
            "The person submitted \"{title}\" but the answers were not kept. Nothing was typed \
             into a page. Ask again with request_user_form and collect true."
        ),
        FormResolution::Dismissed if timed_out => format!(
            "The wait for \"{title}\" timed out. Continue without those fields. Do not invent \
             them, and do not raise the same form again."
        ),
        FormResolution::Dismissed => format!(
            "The person dismissed \"{title}\" without answering. Continue without those fields. \
             Do not invent them, and do not raise the same form again."
        ),
        FormResolution::Escalated => format!(
            "The person chose to finish \"{title}\" on the computer. Wait for them. Do not invent \
             the fields."
        ),
    }
}

/// History line for a later turn reading the gateway transcript. None for an unresolved card
/// (the waiting tool result has not landed yet) and none for a non-form entry.
#[must_use]
pub fn history_line(entry: &Value) -> Option<String> {
    if !is_user_form_entry(entry) {
        return None;
    }
    let form = form_request_from(entry);
    let word = entry.get("formResolution").and_then(Value::as_str)?;
    let resolution = match word {
        "submitted" => FormResolution::Submitted,
        "fill_failed" => FormResolution::FillFailed,
        "dismissed" => FormResolution::Dismissed,
        "escalated" => FormResolution::Escalated,
        _ => return None,
    };
    let timed_out = entry.get("timedOut").and_then(Value::as_bool) == Some(true);
    let shared = entry
        .get("sharedValues")
        .and_then(Value::as_object)
        .map(|object| {
            object
                .iter()
                .filter_map(|(id, value)| Some((id.clone(), value.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let line = model_facing_result(entry, &form, resolution, &shared, timed_out);
    if contains_secret_value(&line, &form, &shared) {
        return Some(model_facing_result(
            entry,
            &form,
            resolution,
            &BTreeMap::new(),
            timed_out,
        ));
    }
    Some(line)
}

/// Combined email+password without `samePage` and without positions: type only the first
/// focused field. A form whose every field carries `at` is clicked field by field, so all of
/// it is typed whether or not the model remembered `samePage`.
#[must_use]
pub fn types_only_first_field(form: &FormRequest) -> bool {
    form.fields.len() > 1 && !form.same_page && !fully_positioned(form)
}

/// Return is unsafe on a stepped multi-field form (Facebook). Safe when the person asked
/// (`submit`), there is a single field, or an otp/password challenge typed exactly one field
/// on a same-page / single-field form.
#[must_use]
pub fn should_press_return(form: &FormRequest, typed_count: usize) -> bool {
    if typed_count == 0 {
        return false;
    }
    if form.submit {
        return true;
    }
    if types_only_first_field(form) {
        return false;
    }
    if form.fields.len() == 1 {
        return true;
    }
    // A positioned login typed in full is the page's own Log in: every field was clicked and
    // typed, a password is among them, and the model called it a one-page card (`samePage`
    // or `submit`). Without the Return the page sits filled and unsubmitted, which the old
    // stepped flow's password-only card never did. The one-page mark is required so that a
    // card whose positions went stale while the person typed cannot submit a password as a
    // username on its own: an unmarked positioned card is filled, and the model presses the
    // page's button after it has looked.
    if fully_positioned(form)
        && (form.same_page || form.submit)
        && typed_count == form.fields.len()
        && form.fields.iter().any(|field| field.is_secret())
    {
        return true;
    }
    let kind = form.challenge_kind.as_deref().unwrap_or("");
    typed_count == 1 && (kind.eq_ignore_ascii_case("otp") || kind.eq_ignore_ascii_case("password"))
}

/// Type the form into the box. A field that carries `at` is clicked first, its contents
/// selected, then typed — so the value lands in THAT field whatever the browser had focused.
/// A field without `at` is typed into the focused field, as before: default multi-field
/// types the first field only, no Tab, no Return; `samePage` Tabs from field to field (past a
/// skipped one too, or the next value would land in the previous box); `submit` (or a single
/// / otp / password field) Returns. No screenshot — the model observes after settle.
pub async fn fill_into_focus(
    computer: &dyn Computer,
    box_id: &str,
    form: &FormRequest,
    values: &BTreeMap<String, String>,
) -> Vec<FieldOutcome> {
    let mut outcomes = Vec::new();
    let mut typed_count = 0usize;
    let type_limit = if types_only_first_field(form) {
        form.fields.len().min(1)
    } else {
        form.fields.len()
    };
    let mut give_up = false;
    for (index, field) in form.fields.iter().take(type_limit).enumerate() {
        if give_up {
            outcomes.push(FieldOutcome {
                id: field.id.clone(),
                filled: false,
                fill_failed: true,
            });
            continue;
        }
        let value = values.get(&field.id).cloned().unwrap_or_default();
        // On a same-page form without positions, focus walks the fields by Tab — for every
        // field after the first, typed or skipped, or the next value lands in the wrong box.
        let tab_here = form.same_page && index > 0 && field.at.is_none();
        if value.is_empty() {
            if tab_here
                && let Err(error) = computer
                    .act(
                        box_id,
                        &CuaAction::Key {
                            key: "Tab".to_string(),
                        },
                    )
                    .await
            {
                tracing::warn!(field = %field.label, %error, "user-form: Tab past a blank field failed");
                give_up = true;
            }
            outcomes.push(FieldOutcome {
                id: field.id.clone(),
                filled: !field.required && !give_up,
                fill_failed: field.required || give_up,
            });
            continue;
        }
        // Reach the field: by its position when the model gave one, else by Tab.
        let reach = match field.at {
            Some(at) => aim_at(computer, box_id, at).await,
            None if tab_here => computer
                .act(
                    box_id,
                    &CuaAction::Key {
                        key: "Tab".to_string(),
                    },
                )
                .await
                .map_err(|error| error.to_string()),
            None => Ok(()),
        };
        if let Err(error) = reach {
            tracing::warn!(
                field = %field.label,
                error,
                "user-form: could not reach the field; it and the rest will not be typed"
            );
            outcomes.push(FieldOutcome {
                id: field.id.clone(),
                filled: false,
                fill_failed: true,
            });
            give_up = true;
            continue;
        }
        match computer.act(box_id, &CuaAction::Type { text: value }).await {
            Ok(()) => {
                outcomes.push(FieldOutcome {
                    id: field.id.clone(),
                    filled: true,
                    fill_failed: false,
                });
                typed_count += 1;
            }
            Err(error) => {
                tracing::warn!(field = %field.label, %error, "user-form: Type failed");
                outcomes.push(FieldOutcome {
                    id: field.id.clone(),
                    filled: false,
                    fill_failed: true,
                });
            }
        }
    }
    for rest in form.fields.iter().skip(outcomes.len()) {
        // Not this challenge — do not mark fill_failed (that would lie that CUA missed).
        outcomes.push(FieldOutcome {
            id: rest.id.clone(),
            filled: false,
            fill_failed: false,
        });
    }
    if overall_resolution(&outcomes) == FormResolution::Submitted
        && should_press_return(form, typed_count)
        && let Err(error) = computer
            .act(
                box_id,
                &CuaAction::Key {
                    key: "Return".to_string(),
                },
            )
            .await
    {
        tracing::warn!(%error, "user-form: post-fill Return failed; fields were typed");
    }
    outcomes
}

/// Click the field and select what is in it, so the typed value replaces rather than appends
/// (Facebook keeps the email typed on the previous attempt).
async fn aim_at(computer: &dyn Computer, box_id: &str, at: FieldAt) -> Result<(), String> {
    computer
        .act(
            box_id,
            &CuaAction::Click {
                x: at.x,
                y: at.y,
                button: None,
            },
        )
        .await
        .map_err(|error| format!("click: {error}"))?;
    // A page may move focus in a script after the click (a floating label, a React
    // re-render); give it a moment before the select and the typing land.
    tokio::time::sleep(AIM_SETTLE).await;
    computer
        .act(
            box_id,
            &CuaAction::Key {
                key: "ctrl+a".to_string(),
            },
        )
        .await
        .map_err(|error| format!("select all: {error}"))?;
    tokio::time::sleep(AIM_SETTLE).await;
    Ok(())
}

/// The pause after a click and after the select, before the next input lands.
const AIM_SETTLE: std::time::Duration = std::time::Duration::from_millis(150);

#[must_use]
pub fn overall_resolution(outcomes: &[FieldOutcome]) -> FormResolution {
    if outcomes.iter().any(|outcome| outcome.fill_failed) {
        FormResolution::FillFailed
    } else {
        FormResolution::Submitted
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "../tests/unit/user_form.rs"]
mod tests;
