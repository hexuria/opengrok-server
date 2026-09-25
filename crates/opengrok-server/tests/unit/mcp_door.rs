//! The door's conversion of an advertised tool into an MCP `Tool`, without a store.

use super::*;
use serde_json::json;

/// #196: THE DOOR RE-EXPORTS WHAT THE EXECUTOR ADVERTISES. Claude Code reading this door was told
/// every plugin tool took an open object, and guessed its arguments like any other model.
#[test]
fn a_plugin_tools_required_arguments_survive_the_door() {
    let listed = opengrok_tools::mcp::McpTool {
        qualified_name: "github.api.create_issue".to_string(),
        remote_name: "create_issue".to_string(),
        description: Some("Open an issue".to_string()),
        input_schema: match json!({
            "type": "object",
            "properties": {
                "owner": { "type": "string" },
                "repo": { "type": "string" },
                "title": { "type": "string" },
                "coworker_id": { "type": "string" }
            },
            "required": ["owner", "repo", "title"]
        }) {
            serde_json::Value::Object(map) => Some(map),
            _ => None,
        },
        annotations: None,
    };
    let advertised = json!({
        "type": "function",
        "function": {
            "name": "github_api_create_issue",
            "description": "Open an issue",
            "parameters": listed.parameters(),
        },
    });

    let tool = to_mcp_tool(&advertised).expect("a well-formed schema is exported");

    assert_eq!(
        tool.input_schema.get("required"),
        Some(&json!(["owner", "repo", "title"]))
    );
    assert_eq!(tool.input_schema["properties"]["owner"]["type"], "string");
    assert!(
        tool.input_schema["properties"].get("coworker_id").is_none(),
        "identity is the door's to fill, never the caller's: {:?}",
        tool.input_schema
    );
}

/// A tool advertised as the open object still exports a valid `inputSchema`, never `{}`.
#[test]
fn an_open_object_stays_an_object() {
    let advertised = json!({
        "type": "function",
        "function": { "name": "x", "description": "", "parameters": {} },
    });
    let tool = to_mcp_tool(&advertised).expect("exported");
    assert_eq!(tool.input_schema.get("type"), Some(&json!("object")));
}
