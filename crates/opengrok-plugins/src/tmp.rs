//! OpenGrok TMP client extension (`app.opengrok.tmp`).
//!
//! Agent Plugins v1 portable components are skills and MCP servers only. Grounded chat tokens
//! are client-specific, so they live under `extensions` as required by the spec. The host
//! snapshots candidates from the declared resolver **before** any model call. MCP servers on
//! the same plugin are not lookup tools.

use serde_json::Value;

/// Reverse-domain namespace for OpenGrok TMP token declarations.
pub const TMP_EXTENSION_NAMESPACE: &str = "app.opengrok.tmp";

/// One token a plugin asks the host to ground.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmpToken {
    /// Catalog name (`user`).
    pub name: String,
    /// Picker label.
    pub display_name: String,
    /// Participate in implicit catalog scan (bare `Uriah`).
    pub implicit: bool,
    /// Bare `@Uriah` / `#Uriah` binds against this schema.
    pub bare_at: bool,
    /// Host resolver id (`org-accounts`, `plugin-file`, `value`). Never an MCP tool name.
    pub resolver: String,
    /// Composer widget: `list`, `choice`, `number`, `text`, `multi`.
    pub ui: String,
    /// Missing this token on send should prompt, not guess.
    pub required: bool,
    /// Plugin-relative file for `plugin-file` resolvers.
    pub source: String,
    /// Validation object from the plugin (`min`, `max`, `pattern`, …).
    pub validate: Value,
}

/// Read TMP tokens from a `plugin.json` `extensions` object. Unknown namespaces are ignored.
/// Invalid token rows are skipped (fail closed for that row, not the plugin).
#[must_use]
pub fn tmp_tokens_from_extensions(extensions: Option<&Value>) -> Vec<TmpToken> {
    let Some(extensions) = extensions else {
        return Vec::new();
    };
    let Some(tmp) = extensions.get(TMP_EXTENSION_NAMESPACE) else {
        return Vec::new();
    };
    let Some(rows) = tmp.get("tokens").and_then(Value::as_array) else {
        return Vec::new();
    };
    rows.iter().filter_map(parse_token).collect()
}

fn parse_token(value: &Value) -> Option<TmpToken> {
    let name = value.get("name")?.as_str()?.trim().to_string();
    if !is_token_name(&name) {
        return None;
    }
    let resolver = value.get("resolver")?.as_str()?.trim().to_string();
    if resolver.is_empty() {
        return None;
    }
    let display_name = value
        .get("displayName")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .unwrap_or(&name)
        .to_string();
    let implicit = value
        .get("implicit")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let bare_at = value
        .get("bareAt")
        .and_then(Value::as_bool)
        .unwrap_or(implicit);
    let ui = value
        .get("ui")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|kind| matches!(*kind, "list" | "choice" | "number" | "text" | "multi"))
        .unwrap_or(if resolver == "value" { "text" } else { "list" })
        .to_string();
    let required = value
        .get("required")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let source = value
        .get("source")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let validate = value
        .get("validate")
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    Some(TmpToken {
        name,
        display_name,
        implicit,
        bare_at,
        resolver,
        ui,
        required,
        source,
        validate,
    })
}

fn is_token_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {
            chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
        }
        _ => false,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_plugin_without_the_tmp_extension_declares_no_tokens() {
        let extensions = json!({"com.example.client": {"hooks": ["on-send"]}});
        assert!(tmp_tokens_from_extensions(Some(&extensions)).is_empty());
        assert!(tmp_tokens_from_extensions(None).is_empty());
    }

    #[test]
    fn org_users_declares_a_user_token_with_a_host_resolver() {
        let extensions = json!({
            "app.opengrok.tmp": {
                "tokens": [{
                    "name": "user",
                    "displayName": "User",
                    "implicit": true,
                    "bareAt": true,
                    "resolver": "org-accounts"
                }]
            }
        });
        let tokens = tmp_tokens_from_extensions(Some(&extensions));
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].name, "user");
        assert_eq!(tokens[0].resolver, "org-accounts");
        assert!(tokens[0].implicit);
        assert!(tokens[0].bare_at);
        assert_eq!(tokens[0].ui, "list");
        assert!(!tokens[0].required);
    }

    #[test]
    fn a_scalar_token_can_use_the_value_resolver() {
        let extensions = json!({
            "app.opengrok.tmp": {
                "tokens": [{
                    "name": "years",
                    "ui": "number",
                    "resolver": "value",
                    "required": false,
                    "validate": { "min": 1, "max": 120 }
                }]
            }
        });
        let tokens = tmp_tokens_from_extensions(Some(&extensions));
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].ui, "number");
        assert_eq!(tokens[0].resolver, "value");
        assert_eq!(tokens[0].validate["max"], 120);
    }

    #[test]
    fn a_token_without_a_resolver_is_skipped() {
        let extensions = json!({
            "app.opengrok.tmp": {
                "tokens": [
                    {"name": "user"},
                    {"name": "repo", "resolver": "http"}
                ]
            }
        });
        let tokens = tmp_tokens_from_extensions(Some(&extensions));
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].name, "repo");
    }

    #[test]
    fn an_uppercase_token_name_is_skipped() {
        let extensions = json!({
            "app.opengrok.tmp": {
                "tokens": [{"name": "User", "resolver": "org-accounts"}]
            }
        });
        assert!(tmp_tokens_from_extensions(Some(&extensions)).is_empty());
    }
}
