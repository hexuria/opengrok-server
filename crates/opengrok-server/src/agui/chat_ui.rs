//! Client-rendered chat widgets. The model is offered these as tools; NativeChat
//! paints them from the TOOL_CALL frames. The server does not draw anything —
//! the handler only acknowledges so the model does not retry.

use opengrok_harness::ToolRunner;
use opengrok_tools::ToolResult;
use serde_json::{json, Value};
use std::sync::Arc;

pub fn attach(runner: Option<ToolRunner>) -> ToolRunner {
    runner
        .unwrap_or_else(ToolRunner::local_only)
        .with_local(
            bar_chart_schema(),
            Arc::new(|call| {
                ToolResult::ok(
                    &call.id,
                    "The chart is on screen. Do not call bar_chart again unless the user asks for different data.",
                )
            }),
        )
        .with_local(
            form_schema(),
            Arc::new(|call| {
                ToolResult::ok(
                    &call.id,
                    "The form is on screen. Do not call form again unless the user asks for a new form.",
                )
            }),
        )
}

pub fn bar_chart_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "bar_chart",
            "description": "Render an interactive bar chart in the user's chat. When the user asks for a chart, graph, bar chart, or numeric comparison, you MUST call this tool. Invent reasonable sample bars if they did not provide numbers. Do not search the computer, do not write files, and do not print the bars as markdown or ASCII.",
            "parameters": {
                "type": "object",
                "properties": {
                    "title": { "type": "string" },
                    "bars": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "label": { "type": "string" },
                                "value": { "type": "number" }
                            },
                            "required": ["label", "value"]
                        }
                    }
                },
                "required": ["bars"]
            }
        }
    })
}

pub fn form_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "form",
            "description": "Render an interactive choice form in the user's chat. When the user should pick an option, you MUST call this tool. Do not search the computer and do not list the options as markdown.",
            "parameters": {
                "type": "object",
                "properties": {
                    "title": { "type": "string" },
                    "prompt": { "type": "string" },
                    "submit": { "type": "string" },
                    "fields": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string" },
                                "label": { "type": "string" },
                                "options": {
                                    "type": "array",
                                    "items": { "type": "string" }
                                }
                            },
                            "required": ["label", "options"]
                        }
                    }
                },
                "required": ["fields"]
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agui_turns_offer_bar_chart_and_form() {
        let runner = attach(None);
        let names: Vec<String> = runner
            .tool_schemas()
            .iter()
            .filter_map(|schema| schema["function"]["name"].as_str().map(str::to_string))
            .collect();
        assert!(names.contains(&"bar_chart".to_string()), "{names:?}");
        assert!(names.contains(&"form".to_string()), "{names:?}");
    }
}
