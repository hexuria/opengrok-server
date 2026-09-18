//! In-chat `user-form`: the field schema, secret stripping, and the fill that types into the
//! box **outside** `computer_use`.
//!
//! WHY THIS IS NOT `submitSecret`. That verb stamps `secretProvided: true` and DROPS the value
//! (connector vault). This path types into the live page. Mixing them would either vault a
//! Google password or fill a connector secret into Chromium.
//!
//! WHY THIS IS NOT `computer` type. `computer_use` always attaches a PNG after the action, so a
//! password typed that way lands in the model payload. Fill calls `Computer::act(Type)` and
//! never screenshots.
//!
//! hexuria/box today is X11 type-into-focus (`POST /v1/cua/type`), not Playwright aria-ref.
//! Facebook login is often stepped (email page, then password). Tab/Return on a combined
//! email+password form mis-focuses; CUA still succeeds and we used to report `submitted`
//! while the password landed in the wrong box. Default: type only the first focused field.
//! `samePage: true` allows Tab; `submit: true` allows Return.

use std::collections::BTreeMap;

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

/// Pull a form out of tool arguments or a transcript `formRequest`, ignoring identity keys the
/// executor stamps and any values the model must never keep.
#[must_use]
pub fn form_request_from(value: &Value) -> FormRequest {
    let source = value
        .get("formRequest")
        .or_else(|| value.pointer("/message/formRequest"))
        .unwrap_or(value);
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
    if let Some(kind) = form.challenge_kind {
        body["challengeKind"] = json!(kind);
    }
    if form.same_page {
        body["samePage"] = json!(true);
    }
    if form.submit {
        body["submit"] = json!(true);
    }
    body
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
            let text = match value {
                Value::String(text) => text.clone(),
                Value::Number(number) => number.to_string(),
                Value::Bool(flag) => flag.to_string(),
                _ => return None,
            };
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
        .filter(|field| !field.is_secret())
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
        "Screenshot and confirm what the page shows now; do not claim login succeeded. Auth is one challenge per form: if a password page is next, call request_user_form with a password-only form (new entryId, challengeKind \"password\"). Never re-raise a form that already settled."
    } else {
        "Screenshot and confirm what the page shows now; do not claim login succeeded until you see it. If another in-sandbox challenge (OTP, phone verification on the same page) appears, call request_user_form again with otp fields — never re-raise a form that already settled. Captcha, passkey, or a page outside this box is handoff, not another password form."
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
    let line = tool_result_content(&form, resolution, &shared, timed_out);
    if contains_secret_value(&line, &form, &shared) {
        return Some(tool_result_content(
            &form,
            resolution,
            &BTreeMap::new(),
            timed_out,
        ));
    }
    Some(line)
}

/// Combined email+password without `samePage`: type only the first focused field.
#[must_use]
pub fn types_only_first_field(form: &FormRequest) -> bool {
    form.fields.len() > 1 && !form.same_page
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
    let kind = form.challenge_kind.as_deref().unwrap_or("");
    typed_count == 1 && (kind.eq_ignore_ascii_case("otp") || kind.eq_ignore_ascii_case("password"))
}

/// Type into the currently focused field. Default multi-field: first field only, no Tab,
/// no Return. `samePage` Tabs; `submit` (or a single / otp / password field) Returns.
/// No screenshot — the model observes after settle.
pub async fn fill_into_focus(
    computer: &dyn Computer,
    box_id: &str,
    form: &FormRequest,
    values: &BTreeMap<String, String>,
) -> Vec<FieldOutcome> {
    let mut outcomes = Vec::new();
    let mut previous_typed = false;
    let mut typed_count = 0usize;
    let type_limit = if types_only_first_field(form) {
        form.fields.len().min(1)
    } else {
        form.fields.len()
    };
    for field in form.fields.iter().take(type_limit) {
        let value = values.get(&field.id).cloned().unwrap_or_default();
        if field.required && value.is_empty() {
            outcomes.push(FieldOutcome {
                id: field.id.clone(),
                filled: false,
                fill_failed: true,
            });
            previous_typed = false;
            continue;
        }
        if value.is_empty() {
            outcomes.push(FieldOutcome {
                id: field.id.clone(),
                filled: true,
                fill_failed: false,
            });
            previous_typed = false;
            continue;
        }
        if previous_typed
            && let Err(error) = computer
                .act(
                    box_id,
                    &CuaAction::Key {
                        key: "Tab".to_string(),
                    },
                )
                .await
        {
            tracing::warn!(
                field = %field.label,
                %error,
                "user-form: Tab between fields failed; remaining fields will not be typed"
            );
            outcomes.push(FieldOutcome {
                id: field.id.clone(),
                filled: false,
                fill_failed: true,
            });
            for rest in form.fields.iter().skip(outcomes.len()) {
                if outcomes.iter().any(|seen| seen.id == rest.id) {
                    continue;
                }
                outcomes.push(FieldOutcome {
                    id: rest.id.clone(),
                    filled: false,
                    fill_failed: true,
                });
            }
            break;
        }
        match computer.act(box_id, &CuaAction::Type { text: value }).await {
            Ok(()) => {
                outcomes.push(FieldOutcome {
                    id: field.id.clone(),
                    filled: true,
                    fill_failed: false,
                });
                previous_typed = true;
                typed_count += 1;
            }
            Err(error) => {
                tracing::warn!(field = %field.label, %error, "user-form: Type failed");
                outcomes.push(FieldOutcome {
                    id: field.id.clone(),
                    filled: false,
                    fill_failed: true,
                });
                previous_typed = false;
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
mod tests {
    use super::*;
    use async_trait::async_trait;
    use opengrok_box::{BoxResult, CommandOutput, StartedCommand};
    use std::sync::Mutex;

    #[test]
    fn password_and_otp_are_secret_without_the_flag() {
        let password = FormField {
            id: "p".into(),
            label: "Password".into(),
            r#type: "password".into(),
            required: true,
            secret: false,
        };
        let otp = FormField {
            id: "o".into(),
            label: "Code".into(),
            r#type: "otp".into(),
            required: true,
            secret: false,
        };
        let email = FormField {
            id: "e".into(),
            label: "Email".into(),
            r#type: "email".into(),
            required: true,
            secret: false,
        };
        assert!(password.is_secret());
        assert!(otp.is_secret());
        assert!(!email.is_secret());
    }

    #[test]
    fn sanitize_drops_values_and_identity() {
        let raw = json!({
            "title": "Google account email",
            "instruction": "Enter the address.",
            "fields": [{
                "id": "email",
                "label": "Email",
                "type": "email",
                "required": true,
                "value": "should-not-keep@example.com"
            }],
            "values": { "password": "s3cret" },
            "coworker_id": "cw_x",
            "box_id": "box_x",
            "domain": "accounts.google.com",
            "liveHost": "accounts.google.com"
        });
        let cleaned = sanitize_arguments(&raw);
        assert!(cleaned.get("values").is_none(), "{cleaned}");
        assert!(cleaned.get("coworker_id").is_none(), "{cleaned}");
        let dumped = cleaned.to_string();
        assert!(!dumped.contains("s3cret"), "{dumped}");
        assert!(!dumped.contains("should-not-keep"), "{dumped}");
        assert_eq!(cleaned["title"], "Google account email");
        assert_eq!(cleaned["liveHost"], "accounts.google.com");
        assert_eq!(cleaned["fields"][0]["id"], "email");
        assert!(cleaned["fields"][0].get("value").is_none());
    }

    #[test]
    fn challenge_kind_round_trips_on_sanitize() {
        let raw = json!({
            "title": "Enter code",
            "fields": [{ "id": "otp", "label": "Code", "type": "otp", "required": true }],
            "challengeKind": "otp",
            "samePage": true,
            "submit": true,
            "values": { "otp": "123456" }
        });
        let cleaned = sanitize_arguments(&raw);
        assert_eq!(cleaned["challengeKind"], "otp");
        assert_eq!(cleaned["samePage"], true);
        assert_eq!(cleaned["submit"], true);
        assert!(cleaned.get("values").is_none());
        assert!(!cleaned.to_string().contains("123456"));
    }

    #[test]
    fn shared_values_omit_secrets() {
        let form = FormRequest {
            title: "Sign in".into(),
            instruction: String::new(),
            fields: vec![
                FormField {
                    id: "email".into(),
                    label: "Email".into(),
                    r#type: "email".into(),
                    required: true,
                    secret: false,
                },
                FormField {
                    id: "password".into(),
                    label: "Password".into(),
                    r#type: "password".into(),
                    required: true,
                    secret: false,
                },
            ],
            domain: None,
            live_host: None,
            challenge_kind: None,
            same_page: false,
            submit: false,
        };
        let values = BTreeMap::from([
            ("email".into(), "ada@example.com".into()),
            ("password".into(), "s3cret-pass".into()),
        ]);
        let shared = shared_values(&form, &values);
        assert_eq!(
            shared.get("email").map(String::as_str),
            Some("ada@example.com")
        );
        assert!(!shared.contains_key("password"));
        let content = tool_result_content(&form, FormResolution::Submitted, &shared, false);
        assert!(content.contains("ada@example.com"));
        assert!(content.contains("filled into the page"));
        assert!(content.contains("Screenshot"));
        assert!(content.contains("password-only"));
        assert!(content.contains("new entryId"));
        assert!(!content.contains("logged in"));
        assert!(!content.contains("s3cret-pass"));
        assert!(!contains_secret_value(&content, &form, &values));
    }

    #[test]
    fn unresolved_reads_the_official_rule() {
        let pending = json!({
            "kind": "send-message",
            "id": "e_1",
            "message": { "type": "user-form", "formRequest": { "title": "x", "fields": [] } }
        });
        assert!(is_unresolved(&pending));
        let settled = json!({
            "kind": "send-message",
            "id": "e_1",
            "formResolution": "submitted",
            "message": { "type": "user-form", "formRequest": { "title": "x", "fields": [] } }
        });
        assert!(!is_unresolved(&settled));
        let dismissed_widget = json!({
            "kind": "send-message",
            "id": "e_1",
            "widgetDismissed": true,
            "message": { "type": "user-form", "formRequest": { "title": "x", "fields": [] } }
        });
        assert!(!is_unresolved(&dismissed_widget));
        assert!(history_line(&pending).is_none());
        let line = history_line(&settled).expect("settled history");
        assert!(
            line.contains("submitted")
                || line.contains("filled")
                || line.contains("Filled")
                || line.contains("submitted")
                || line.contains("form")
        );
        assert!(!line.contains("s3cret"));
    }

    #[derive(Default)]
    struct FillSpy {
        acts: Mutex<Vec<CuaAction>>,
        shots: Mutex<u32>,
        fail_type: bool,
    }

    #[async_trait]
    impl Computer for FillSpy {
        async fn create(&self, _ttl: Option<u64>) -> BoxResult<String> {
            Ok("box".into())
        }
        async fn run(&self, _b: &str, _c: &str, _t: u32) -> BoxResult<CommandOutput> {
            Err(opengrok_box::BoxError::NoSuchBox)
        }
        async fn start(&self, _b: &str, _c: &str) -> BoxResult<StartedCommand> {
            Err(opengrok_box::BoxError::NoSuchBox)
        }
        async fn watch(&self, _b: &str, _p: &str) -> BoxResult<StartedCommand> {
            Err(opengrok_box::BoxError::NoSuchBox)
        }
        async fn read_file(&self, _b: &str, _p: &str) -> BoxResult<String> {
            Err(opengrok_box::BoxError::NoSuchBox)
        }
        async fn write_file(&self, _b: &str, _p: &str, _c: &str) -> BoxResult<()> {
            Ok(())
        }
        async fn expose_port(&self, _b: &str, _p: u16, _t: &str) -> BoxResult<String> {
            Ok(String::new())
        }
        async fn stop(&self, _b: &str) -> BoxResult<()> {
            Ok(())
        }
        async fn resume(&self, _b: &str) -> BoxResult<()> {
            Ok(())
        }
        async fn destroy(&self, _b: &str) -> BoxResult<()> {
            Ok(())
        }
        async fn state(&self, _b: &str) -> BoxResult<String> {
            Ok("running".into())
        }
        async fn screenshot(&self, _b: &str) -> BoxResult<opengrok_box::Screenshot> {
            *self.shots.lock().unwrap() += 1;
            Err(opengrok_box::no_screen())
        }
        async fn act(&self, _b: &str, action: &CuaAction) -> BoxResult<()> {
            self.acts.lock().unwrap().push(action.clone());
            if self.fail_type && matches!(action, CuaAction::Type { .. }) {
                return Err(opengrok_box::BoxError::NoSuchBox);
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn fill_types_only_the_first_field_by_default() {
        let spy = FillSpy::default();
        let form = FormRequest {
            title: "Sign in".into(),
            instruction: String::new(),
            fields: vec![
                FormField {
                    id: "email".into(),
                    label: "Email".into(),
                    r#type: "email".into(),
                    required: true,
                    secret: false,
                },
                FormField {
                    id: "password".into(),
                    label: "Password".into(),
                    r#type: "password".into(),
                    required: true,
                    secret: false,
                },
            ],
            domain: None,
            live_host: None,
            challenge_kind: None,
            same_page: false,
            submit: false,
        };
        let values = BTreeMap::from([
            ("email".into(), "ada@example.com".into()),
            ("password".into(), "s3cret-pass".into()),
        ]);
        let outcomes = fill_into_focus(&spy, "box_1", &form, &values).await;
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes[0].filled && !outcomes[0].fill_failed);
        assert!(
            !outcomes[1].filled && !outcomes[1].fill_failed,
            "password is the next challenge, not a CUA miss: {:?}",
            outcomes[1]
        );
        assert_eq!(overall_resolution(&outcomes), FormResolution::Submitted);
        let acts = spy.acts.lock().unwrap().clone();
        assert_eq!(
            acts,
            vec![CuaAction::Type {
                text: "ada@example.com".into()
            }],
            "default multi-field must not Tab or Return: {acts:?}"
        );
        assert_eq!(*spy.shots.lock().unwrap(), 0, "fill must not screenshot");
    }

    #[tokio::test]
    async fn a_single_password_field_may_press_return() {
        let spy = FillSpy::default();
        let form = FormRequest {
            title: "Password".into(),
            instruction: String::new(),
            fields: vec![FormField {
                id: "password".into(),
                label: "Password".into(),
                r#type: "password".into(),
                required: true,
                secret: false,
            }],
            domain: None,
            live_host: None,
            challenge_kind: Some("password".into()),
            same_page: false,
            submit: false,
        };
        let values = BTreeMap::from([("password".into(), "s3cret-pass".into())]);
        let outcomes = fill_into_focus(&spy, "box_1", &form, &values).await;
        assert_eq!(overall_resolution(&outcomes), FormResolution::Submitted);
        let acts = spy.acts.lock().unwrap().clone();
        assert_eq!(
            acts,
            vec![
                CuaAction::Type {
                    text: "s3cret-pass".into()
                },
                CuaAction::Key {
                    key: "Return".into()
                },
            ]
        );
        assert_eq!(*spy.shots.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn same_page_with_submit_tabs_and_returns() {
        let spy = FillSpy::default();
        let form = FormRequest {
            title: "Sign in".into(),
            instruction: String::new(),
            fields: vec![
                FormField {
                    id: "email".into(),
                    label: "Email".into(),
                    r#type: "email".into(),
                    required: true,
                    secret: false,
                },
                FormField {
                    id: "password".into(),
                    label: "Password".into(),
                    r#type: "password".into(),
                    required: true,
                    secret: false,
                },
            ],
            domain: None,
            live_host: None,
            challenge_kind: None,
            same_page: true,
            submit: true,
        };
        let values = BTreeMap::from([
            ("email".into(), "ada@example.com".into()),
            ("password".into(), "s3cret-pass".into()),
        ]);
        let outcomes = fill_into_focus(&spy, "box_1", &form, &values).await;
        assert!(outcomes.iter().all(|o| o.filled && !o.fill_failed));
        let acts = spy.acts.lock().unwrap().clone();
        assert_eq!(
            acts,
            vec![
                CuaAction::Type {
                    text: "ada@example.com".into()
                },
                CuaAction::Key { key: "Tab".into() },
                CuaAction::Type {
                    text: "s3cret-pass".into()
                },
                CuaAction::Key {
                    key: "Return".into()
                },
            ]
        );
        assert_eq!(*spy.shots.lock().unwrap(), 0);
    }

    #[test]
    fn return_is_opt_in_on_multi_field_forms() {
        let email = FormField {
            id: "email".into(),
            label: "Email".into(),
            r#type: "email".into(),
            required: true,
            secret: false,
        };
        let password = FormField {
            id: "password".into(),
            label: "Password".into(),
            r#type: "password".into(),
            required: true,
            secret: false,
        };
        let multi = FormRequest {
            fields: vec![email.clone(), password.clone()],
            ..FormRequest::default()
        };
        assert!(
            !should_press_return(&multi, 1),
            "default multi-field must not Return"
        );
        assert!(types_only_first_field(&multi));
        let single = FormRequest {
            fields: vec![password.clone()],
            challenge_kind: Some("password".into()),
            ..FormRequest::default()
        };
        assert!(should_press_return(&single, 1));
        let same_page_submit = FormRequest {
            fields: vec![email, password],
            same_page: true,
            submit: true,
            ..FormRequest::default()
        };
        assert!(should_press_return(&same_page_submit, 2));
        assert!(!types_only_first_field(&same_page_submit));
    }

    #[test]
    fn submitted_values_keep_only_this_forms_fields() {
        let form = FormRequest {
            title: "Sign in".into(),
            instruction: String::new(),
            fields: vec![FormField {
                id: "email".into(),
                label: "Email".into(),
                r#type: "email".into(),
                required: true,
                secret: false,
            }],
            domain: None,
            live_host: None,
            challenge_kind: None,
            same_page: false,
            submit: false,
        };
        let raw = json!({
            "email": "ada@example.com",
            "password": "s3cret",
            "coworker_id": "cw_x"
        });
        let values = submitted_values(&form, &raw);
        assert_eq!(
            values.get("email").map(String::as_str),
            Some("ada@example.com")
        );
        assert!(!values.contains_key("password"));
        assert!(!values.contains_key("coworker_id"));
    }

    #[test]
    fn field_outcomes_are_the_recovery_card_shape() {
        let outcomes = vec![
            FieldOutcome {
                id: "email".into(),
                filled: true,
                fill_failed: false,
            },
            FieldOutcome {
                id: "password".into(),
                filled: false,
                fill_failed: true,
            },
        ];
        assert_eq!(overall_resolution(&outcomes), FormResolution::FillFailed);
        let dumped = serde_json::to_value(&outcomes).expect("outcomes json");
        assert_eq!(dumped[0]["id"], "email");
        assert_eq!(dumped[0]["filled"], true);
        assert_eq!(dumped[0]["fillFailed"], false);
        assert_eq!(dumped[1]["fillFailed"], true);
    }

    #[test]
    fn live_handoff_holds_the_screen_after_the_form_settles() {
        let form = json!({
            "kind": "send-message",
            "id": "e_form",
            "formResolution": "escalated",
            "widgetDismissed": true,
            "message": { "type": "user-form", "formRequest": { "title": "x", "fields": [] } }
        });
        assert!(!is_unresolved(&form));
        assert!(
            !holds_the_screen(&form),
            "escalated form alone must not hold"
        );
        let live = json!({
            "kind": "send-message",
            "id": "e_hand",
            "message": { "type": "attachment", "url": "sand://box" },
            "boxRequestId": "req_1",
            "boxInstruction": "Finish this form on the computer."
        });
        assert!(is_live_handoff(&live));
        assert!(holds_the_screen(&live));
        let handed_back = json!({
            "kind": "send-message",
            "id": "e_hand",
            "message": { "type": "attachment", "url": "sand://box" },
            "boxRequestId": "req_1",
            "boxResolution": "handed_back"
        });
        assert!(!is_live_handoff(&handed_back));
        assert!(!holds_the_screen(&handed_back));
        let declined = json!({
            "kind": "send-message",
            "id": "e_hand",
            "message": { "type": "attachment", "url": "sand://box" },
            "boxRequestId": "req_1",
            "boxResolution": "declined"
        });
        assert!(!is_live_handoff(&declined));
        assert!(HAND_BACK_TOOL_RESULT.contains("Screenshot"));
        assert!(!HAND_BACK_TOOL_RESULT.to_lowercase().contains("logged in"));
    }

    #[tokio::test]
    async fn a_failed_type_is_fill_failed_and_does_not_press_return() {
        let spy = FillSpy {
            fail_type: true,
            ..FillSpy::default()
        };
        let form = FormRequest {
            title: "Sign in".into(),
            instruction: String::new(),
            fields: vec![FormField {
                id: "password".into(),
                label: "Password".into(),
                r#type: "password".into(),
                required: true,
                secret: false,
            }],
            domain: None,
            live_host: None,
            challenge_kind: None,
            same_page: false,
            submit: false,
        };
        let values = BTreeMap::from([("password".into(), "s3cret-pass".into())]);
        let outcomes = fill_into_focus(&spy, "box_1", &form, &values).await;
        assert_eq!(overall_resolution(&outcomes), FormResolution::FillFailed);
        assert_eq!(outcomes[0].id, "password");
        assert!(outcomes[0].fill_failed);
        let acts = spy.acts.lock().unwrap().clone();
        assert!(
            !acts
                .iter()
                .any(|act| matches!(act, CuaAction::Key { key } if key == "Return")),
            "partial fill must not submit the page: {acts:?}"
        );
        assert_eq!(*spy.shots.lock().unwrap(), 0);
    }

    #[test]
    fn timed_out_dismiss_short_circuits() {
        let form = FormRequest {
            title: "Sign in".into(),
            ..FormRequest::default()
        };
        let line = tool_result_content(&form, FormResolution::Dismissed, &BTreeMap::new(), true);
        assert_eq!(line, HOLD_TIMED_OUT_TOOL_RESULT);
        let settled = json!({
            "kind": "send-message",
            "id": "e_1",
            "formResolution": "dismissed",
            "timedOut": true,
            "message": { "type": "user-form", "formRequest": { "title": "Sign in", "fields": [] } }
        });
        let history = history_line(&settled).expect("timed out history");
        assert_eq!(history, HOLD_TIMED_OUT_TOOL_RESULT);
    }
}
