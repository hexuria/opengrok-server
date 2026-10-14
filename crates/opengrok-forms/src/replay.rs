//! A run's replayed AG-UI frames joined to the transcript's form cards (`GET /ag-ui/threads/{id}`,
//! `GET /ag-ui/runs/{id}`), so a cold NativeChat paints each card as it now is: settled, still
//! waiting, or ended on the computer. Pure JSON in and out: the server reads the run and the
//! transcript and serves what this returns.

use std::collections::{BTreeMap, HashSet};

use serde_json::{Value, json};

use crate::user_form::{
    HANDOFF_ENDINGS, awaits_its_handoff, call_id_of, is_user_form_entry, settle_handoff,
};

/// Fold current gateway user-form state into AG-UI replay events so a cold
/// NativeChat rebuilds ✓ Submitted (and idle cards) from `GET /ag-ui/threads/{id}`
/// / `GET /ag-ui/runs/{id}` — not only from live unresolved CUSTOMs.
pub fn hydrate_agui_events(
    mut events: Vec<Value>,
    forms: &[Value],
    started_at_ms: i64,
    updated_at_ms: i64,
) -> Vec<Value> {
    let forms: Vec<Value> = forms
        .iter()
        .filter(|entry| is_user_form_entry(entry))
        .map(|entry| with_backfilled_handoff(entry, &events))
        .collect();
    if forms.is_empty() {
        return events;
    }
    let mut used = HashSet::new();
    for event in &mut events {
        if !is_user_form_agui(event) {
            continue;
        }
        if let Some(id) = user_form_event_id(event).map(str::to_string)
            && let Some(form) = forms
                .iter()
                .find(|entry| entry.get("id").and_then(Value::as_str) == Some(id.as_str()))
        {
            overlay_form(event, form);
            used.insert(id);
            continue;
        }
        // Same-completion stacked Website login cards share a fingerprint. Join
        // on `callId` so replay does not stamp the last card onto the first TOOL_CALL.
        if let Some(call_id) = event
            .get("callId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            && let Some(form) = forms.iter().find(|entry| {
                let id = entry.get("id").and_then(Value::as_str).unwrap_or("");
                !used.contains(id) && entry.get("callId").and_then(Value::as_str) == Some(call_id)
            })
        {
            overlay_form(event, form);
            if let Some(id) = form.get("id").and_then(Value::as_str) {
                used.insert(id.to_string());
            }
            continue;
        }
        if let Some(fingerprint) = event_fingerprint(event)
            && let Some(form) = forms.iter().find(|entry| {
                let id = entry.get("id").and_then(Value::as_str).unwrap_or("");
                !used.contains(id)
                    && entry_fingerprint(entry).as_deref() == Some(fingerprint.as_str())
            })
        {
            overlay_form(event, form);
            if let Some(id) = form.get("id").and_then(Value::as_str) {
                used.insert(id.to_string());
            }
        }
    }
    // A card is appended only to the run that made its call. Two runs a few seconds apart in one
    // thread both have it in their window and each hydrates against the coworker's whole
    // transcript, so the window alone painted it once per run. A card that names no call (written
    // before cards carried one) has only the window to go on.
    let calls: HashSet<String> = events
        .iter()
        .flat_map(|event| [event.get("callId"), event.get("toolCallId")])
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    const SLACK_MS: i64 = 5_000;
    for form in &forms {
        let Some(id) = form.get("id").and_then(Value::as_str) else {
            continue;
        };
        if used.contains(id) {
            continue;
        }
        // A form-mode card names no call, but keeps the model's call that asked for it
        // (`requestCallId`): it belongs to the run that made that call and to no other.
        let asked_by =
            call_id_of(form).or_else(|| form.get("requestCallId").and_then(Value::as_str));
        if asked_by.is_some_and(|call| !calls.contains(call)) {
            continue;
        }
        let at = form.get("timestampMs").and_then(Value::as_i64).unwrap_or(0);
        if at < started_at_ms.saturating_sub(SLACK_MS)
            || at > updated_at_ms.saturating_add(SLACK_MS)
        {
            continue;
        }
        // Overlaid like a matched frame, so it carries how its handoff ended too.
        let mut frame = agui_user_form_frame(form);
        overlay_form(&mut frame, form);
        events.push(frame);
        used.insert(id.to_string());
    }
    stamp_tool_calls(&mut events);
    events
}

/// NativeChat paints TOOL_CALL frames as Website login cards. Live HITL stamps
/// `entryId` on the CUSTOM; replay overlays the same id onto matching TOOL_CALLs
/// so stacked cards stay Continue-able after a reconnect.
fn stamp_tool_calls(events: &mut [Value]) {
    let mut by_call: BTreeMap<String, String> = BTreeMap::new();
    for event in events.iter() {
        if !is_user_form_agui(event) {
            continue;
        }
        // A form-mode card's frame has no `callId`; its frames are the call that asked for it.
        let Some(call_id) = event.get("callId").and_then(Value::as_str).or_else(|| {
            event
                .pointer("/value/requestCallId")
                .and_then(Value::as_str)
        }) else {
            continue;
        };
        if let Some(entry_id) = user_form_event_id(event) {
            by_call.insert(call_id.to_string(), entry_id.to_string());
        }
    }
    if by_call.is_empty() {
        return;
    }
    for event in events.iter_mut() {
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        if kind != "TOOL_CALL_START" && kind != "TOOL_CALL_ARGS" && kind != "TOOL_CALL_END" {
            continue;
        }
        let Some(call_id) = event.get("toolCallId").and_then(Value::as_str) else {
            continue;
        };
        let Some(entry_id) = by_call.get(call_id) else {
            continue;
        };
        if let Some(map) = event.as_object_mut() {
            map.insert("entryId".to_string(), json!(entry_id));
        }
    }
}

/// CUSTOM NativeChat already hydrates for settled / cold-load cards. Not live HITL.
pub fn agui_user_form_frame(entry: &Value) -> Value {
    let entry_id = entry.get("id").and_then(Value::as_str).unwrap_or("");
    let message = entry
        .get("message")
        .cloned()
        .unwrap_or_else(|| json!({ "type": "user-form" }));
    let form_request = message.get("formRequest").cloned().unwrap_or(Value::Null);
    let resolution = entry.get("formResolution").cloned().unwrap_or(Value::Null);
    let call_id = entry.get("callId").cloned().unwrap_or(Value::Null);
    json!({
        "type": "CUSTOM",
        "name": "user-form",
        "entryId": entry_id,
        "callId": call_id,
        "formRequest": form_request,
        "formResolution": resolution,
        "message": message,
        "value": entry,
    })
}

fn overlay_form(event: &mut Value, form: &Value) {
    let Some(map) = event.as_object_mut() else {
        return;
    };
    if let Some(id) = form.get("id") {
        map.insert("entryId".to_string(), id.clone());
    }
    if map
        .get("callId")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
        && let Some(call_id) = form.get("callId")
    {
        map.insert("callId".to_string(), call_id.clone());
    }
    if let Some(message) = form.get("message") {
        map.insert("message".to_string(), message.clone());
        if let Some(request) = message.get("formRequest") {
            map.insert("formRequest".to_string(), request.clone());
        }
    }
    if let Some(resolution) = form.get("formResolution") {
        map.insert("formResolution".to_string(), resolution.clone());
    }
    // HOW ITS HANDOFF ENDED, beside `formResolution` (agreed with nativechat for #143): until it
    // is there an escalated form is live. On a park as much as on a `user-form` frame — a form
    // settled after its run stopped waiting has no `user-form` frame, only its park.
    if let Some(ended) = form.get("boxResolution") {
        map.insert("boxResolution".to_string(), ended.clone());
        if let Some(timed_out) = form.get("timedOut") {
            map.insert("timedOut".to_string(), timed_out.clone());
        }
    }
    map.insert("value".to_string(), form.clone());
}

/// A form escalated before its handoff's end was stamped on it (#143), with that end read back
/// from its run's answer to its call. ONLY THE SERVER'S OWN SENTENCES COUNT: any other result is
/// not a handoff's ending, and a word read into it would be invented. None found leaves the form
/// as it was, which reads as still live.
fn with_backfilled_handoff(form: &Value, events: &[Value]) -> Value {
    let ended = call_id_of(form)
        .filter(|_| awaits_its_handoff(form))
        .and_then(|call| {
            events.iter().find_map(|event| {
                let answer = (event.get("type").and_then(Value::as_str)
                    == Some("TOOL_CALL_RESULT")
                    && event.get("toolCallId").and_then(Value::as_str) == Some(call))
                .then(|| event.get("content").and_then(Value::as_str))
                .flatten()?;
                HANDOFF_ENDINGS
                    .iter()
                    .find(|(_, sentence)| *sentence == answer)
            })
        });
    match ended {
        Some((word, _)) => settle_handoff(form.clone(), word, *word == "timed_out"),
        None => form.clone(),
    }
}

pub fn user_form_event_id(event: &Value) -> Option<&str> {
    event
        .get("entryId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .or_else(|| event.pointer("/value/id").and_then(Value::as_str))
        .or_else(|| event.pointer("/value/entryId").and_then(Value::as_str))
}

pub fn is_user_form_agui(event: &Value) -> bool {
    if event.get("type").and_then(Value::as_str) != Some("CUSTOM") {
        return false;
    }
    let name = event.get("name").and_then(Value::as_str).unwrap_or("");
    if name == "user-form" {
        return true;
    }
    if name == "run-awaiting-approval"
        && event.get("reason").and_then(Value::as_str) == Some("user-form")
    {
        return true;
    }
    event
        .get("message")
        .and_then(|message| message.get("type"))
        .and_then(Value::as_str)
        == Some("user-form")
}

fn form_fingerprint(request: &Value) -> String {
    let title = request.get("title").and_then(Value::as_str).unwrap_or("");
    let ids = request
        .get("fields")
        .and_then(Value::as_array)
        .map(|fields| {
            fields
                .iter()
                .filter_map(|field| field.get("id").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    format!("{title}|{ids}")
}

fn event_fingerprint(event: &Value) -> Option<String> {
    let request = event
        .get("formRequest")
        .or_else(|| event.get("arguments"))
        .or_else(|| event.pointer("/message/formRequest"))
        .or_else(|| event.pointer("/value/message/formRequest"))?;
    Some(form_fingerprint(request))
}

fn entry_fingerprint(entry: &Value) -> Option<String> {
    entry.pointer("/message/formRequest").map(form_fingerprint)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "../tests/unit/replay.rs"]
mod tests;
