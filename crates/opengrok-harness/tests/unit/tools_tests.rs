use super::*;
use crate::model::ModelDelta;
use crate::projection::Projection;

fn events_for(deltas: Vec<ModelDelta>) -> Vec<Event> {
    let mut projection = Projection::new("t1", "r1", 1);
    let mut events = Vec::new();
    for delta in deltas {
        events.extend(projection.push(delta));
    }
    events
}

#[test]
fn fragments_are_reassembled_into_one_call() {
    let events = events_for(vec![
        ModelDelta::ToolCallStart {
            id: "c1".to_string(),
            name: "shell".to_string(),
        },
        ModelDelta::ToolCallArgs {
            id: "c1".to_string(),
            delta: "{\"command\":".to_string(),
        },
        ModelDelta::ToolCallArgs {
            id: "c1".to_string(),
            delta: "\"ls -la\"}".to_string(),
        },
        ModelDelta::ToolCallEnd {
            id: "c1".to_string(),
        },
    ]);
    let calls = collect_tool_calls(&events);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "shell");
    assert_eq!(calls[0].arguments["command"], "ls -la");
}

/// A truncated stream leaves partial JSON. Running it would be acting on half a sentence.
#[test]
fn an_unterminated_call_is_not_run() {
    let events = events_for(vec![
        ModelDelta::ToolCallStart {
            id: "c1".to_string(),
            name: "shell".to_string(),
        },
        ModelDelta::ToolCallArgs {
            id: "c1".to_string(),
            delta: "{\"command\": \"rm -r".to_string(),
        },
    ]);
    assert!(collect_tool_calls(&events).is_empty());
}

#[test]
fn several_calls_are_kept_apart_and_in_order() {
    let events = events_for(vec![
        ModelDelta::ToolCallStart {
            id: "c1".to_string(),
            name: "shell".to_string(),
        },
        ModelDelta::ToolCallArgs {
            id: "c1".to_string(),
            delta: "{\"command\":\"one\"}".to_string(),
        },
        ModelDelta::ToolCallEnd {
            id: "c1".to_string(),
        },
        ModelDelta::ToolCallStart {
            id: "c2".to_string(),
            name: "read_file".to_string(),
        },
        ModelDelta::ToolCallArgs {
            id: "c2".to_string(),
            delta: "{\"path\":\"/tmp/a\"}".to_string(),
        },
        ModelDelta::ToolCallEnd {
            id: "c2".to_string(),
        },
    ]);
    let calls = collect_tool_calls(&events);
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].arguments["command"], "one");
    assert_eq!(calls[1].name, "read_file");
}

/// Unparseable arguments must still produce a call, so the executor can refuse it with a
/// reason. Dropping it would leave the model waiting for a result that never comes.
#[test]
fn unparseable_arguments_still_produce_a_call_to_refuse() {
    let events = events_for(vec![
        ModelDelta::ToolCallStart {
            id: "c1".to_string(),
            name: "shell".to_string(),
        },
        ModelDelta::ToolCallArgs {
            id: "c1".to_string(),
            delta: "not json at all".to_string(),
        },
        ModelDelta::ToolCallEnd {
            id: "c1".to_string(),
        },
    ]);
    let calls = collect_tool_calls(&events);
    assert_eq!(calls.len(), 1);
    assert!(calls[0].arguments.is_null());
}

#[test]
fn a_run_with_no_tool_calls_yields_none() {
    let events = events_for(vec![ModelDelta::Text("just talking".to_string())]);
    assert!(collect_tool_calls(&events).is_empty());
}

#[test]
fn request_user_form_arguments_drop_smuggled_values() {
    let events = events_for(vec![
        ModelDelta::ToolCallStart {
            id: "c1".to_string(),
            name: opengrok_tools::REQUEST_USER_FORM.to_string(),
        },
        ModelDelta::ToolCallArgs {
            id: "c1".to_string(),
            delta: serde_json::json!({
                "title": "Sign in",
                "fields": [{ "id": "password", "label": "Password", "type": "password" }],
                "values": { "password": "s3cret-should-never-land" }
            })
            .to_string(),
        },
        ModelDelta::ToolCallEnd {
            id: "c1".to_string(),
        },
    ]);
    let calls = collect_tool_calls(&events);
    assert_eq!(calls.len(), 1);
    let dumped = calls[0].arguments.to_string();
    assert!(!dumped.contains("s3cret-should-never-land"), "{dumped}");
    assert!(calls[0].arguments.get("values").is_none(), "{dumped}");
    assert_eq!(calls[0].arguments["title"], "Sign in");
}
