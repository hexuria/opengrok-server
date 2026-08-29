//! Reaching an MCP server over HTTP.
//!
//! THE CREDENTIAL IS INJECTED HERE AND NOWHERE ELSE. A plugin's `mcp.json` declares placeholders
//! like `"authorization": "Bearer ${TOKEN}"`; the token that fills them is resolved from the
//! coworker's connections and put on the transport at connect time. So it exists in this file, on
//! the wire, and in nothing else — not in a plugin file on disk, not in an event, not in the
//! model's context, not in a log (CLAUDE.md #4).
//!
//! HTTP AND SSE ONLY, AND `stdio` IS REFUSED WITH A REASON. A stdio server is a process launched on
//! whatever machine reaches it, which for a shared instance means handing unreviewed code our
//! filesystem, our network and our environment. Running one inside a coworker's own container is
//! the right answer and is the next slice; until then a plugin carrying one still loads, its skills
//! still work, and the server says plainly why it is unavailable.
//!
//! TOOL NAMES ARE NAMESPACED `<plugin>.<server>.<tool>`. Two plugins bringing a `search` would
//! otherwise become one tool nobody can tell apart, and the model would call whichever won.

use std::collections::BTreeMap;

use opengrok_plugins::{McpServer, Plugin};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("{server} speaks stdio, which this server does not launch; {advice}")]
    UnsupportedTransport { server: String, advice: String },
    #[error("{server} is unreachable: {detail}")]
    Unreachable { server: String, detail: String },
    #[error("{server} refused: {detail}")]
    Refused { server: String, detail: String },
    #[error("{tool} is not a tool this server offers")]
    NoSuchTool { tool: String },
}

/// A tool a plugin's server offers, as the model will be told about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpTool {
    /// `<plugin>.<server>.<tool>` — unique across every plugin installed.
    pub qualified_name: String,
    /// What the server calls it, which is what goes back on the wire.
    pub remote_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Everything needed to reach one server, with its credential already resolved.
#[derive(Clone)]
pub struct Endpoint {
    pub plugin: String,
    pub server: String,
    pub url: String,
    /// Headers with placeholders already filled. Contains a bearer token, so this type has a
    /// hand-written `Debug`.
    pub headers: BTreeMap<String, String>,
}

impl std::fmt::Debug for Endpoint {
    /// Header VALUES are redacted while the names survive: knowing an `authorization` header was
    /// sent is useful when something 401s, and knowing its contents is a leak.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("plugin", &self.plugin)
            .field("server", &self.server)
            .field("url", &self.url)
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Endpoint {
    pub fn qualify(&self, tool: &str) -> String {
        format!("{}.{}.{tool}", self.plugin, self.server)
    }
}

/// Substitute `${NAME}` placeholders from a plugin's declared headers.
///
/// Anything unresolved is DROPPED rather than sent literally: a header reading
/// `Bearer ${GITHUB_TOKEN}` is not a credential, and sending it produces a confusing 401 instead of
/// the honest "this connector is not connected" the caller can act on.
pub fn fill_placeholders(
    declared: &BTreeMap<String, String>,
    values: &BTreeMap<String, String>,
) -> (BTreeMap<String, String>, Vec<String>) {
    let mut filled = BTreeMap::new();
    let mut unresolved = Vec::new();

    for (name, template) in declared {
        let mut value = template.clone();
        let mut missing = false;

        // A small scanner rather than a regex dependency: the syntax is one shape.
        while let Some(start) = value.find("${") {
            let Some(end) = value[start..].find('}').map(|offset| start + offset) else {
                break;
            };
            let key = &value[start + 2..end];
            match values.get(key) {
                Some(resolved) => {
                    value.replace_range(start..=end, resolved);
                }
                None => {
                    unresolved.push(key.to_string());
                    missing = true;
                    break;
                }
            }
        }

        if !missing {
            filled.insert(name.clone(), value);
        }
    }

    (filled, unresolved)
}

/// Turn a plugin's declared servers into endpoints we can reach.
///
/// Returns the reachable ones and, separately, the reasons the rest are not — because "this plugin
/// contributed nothing" and "this plugin needs a credential you have not connected" are different
/// things to tell somebody.
pub fn endpoints_for(
    plugin: &Plugin,
    values: &BTreeMap<String, String>,
) -> (Vec<Endpoint>, Vec<McpError>) {
    let mut endpoints = Vec::new();
    let mut problems = Vec::new();

    for (name, server) in plugin.servers() {
        match server {
            McpServer::Stdio { .. } => {
                problems.push(McpError::UnsupportedTransport {
                    server: format!("{}.{name}", plugin.manifest.name),
                    advice: "it must declare a streamable-http or sse url, or wait for \
                             per-coworker containers"
                        .to_string(),
                });
            }
            McpServer::StreamableHttp { url, headers } | McpServer::Sse { url, headers } => {
                let (filled, unresolved) = fill_placeholders(headers, values);
                if !unresolved.is_empty() {
                    problems.push(McpError::Refused {
                        server: format!("{}.{name}", plugin.manifest.name),
                        detail: format!(
                            "not connected: {} has no value yet",
                            unresolved.join(", ")
                        ),
                    });
                    continue;
                }
                endpoints.push(Endpoint {
                    plugin: plugin.manifest.name.clone(),
                    server: name.clone(),
                    url: url.clone(),
                    headers: filled,
                });
            }
        }
    }

    (endpoints, problems)
}

/// Split a qualified name back into the endpoint it belongs to and the remote tool.
///
/// Returns `None` for anything not carrying two dots, which is how a built-in tool like `shell`
/// stays distinguishable from a plugin's.
pub fn split_qualified(name: &str) -> Option<(String, String, String)> {
    let mut parts = name.splitn(3, '.');
    let plugin = parts.next()?.to_string();
    let server = parts.next()?.to_string();
    let tool = parts.next()?.to_string();
    if plugin.is_empty() || server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((plugin, server, tool))
}

// ---------------------------------------------------------------------------
// The live client.
//
// Kept below the pure part on purpose: everything above is testable without a network, and this is
// the thin layer that actually speaks to a server.
// ---------------------------------------------------------------------------

use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};

/// A live session with one MCP server.
pub struct Session {
    endpoint: Endpoint,
    service: rmcp::service::RunningService<rmcp::RoleClient, ()>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

impl Session {
    /// Connect, performing the MCP initialize handshake.
    ///
    /// The credential goes on the transport here — every request the session makes carries it, and
    /// nothing else in the process needs to know it.
    pub async fn connect(endpoint: Endpoint) -> Result<Self, McpError> {
        let mut config = StreamableHttpClientTransportConfig::with_uri(endpoint.url.clone());

        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in &endpoint.headers {
            // `authorization` has its own slot on the transport; everything else is a custom
            // header. Both end up on the wire, but the split is what rmcp expects.
            if name.eq_ignore_ascii_case("authorization") {
                config.auth_header = Some(value.clone());
                continue;
            }
            let (Ok(name), Ok(value)) = (
                reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                reqwest::header::HeaderValue::from_str(value),
            ) else {
                // A header a plugin declared that cannot be sent is skipped rather than failing the
                // connection: the server may not need it, and a refusal here would be less useful
                // than whatever the server says about its absence.
                continue;
            };
            headers.insert(name, value);
        }
        config.custom_headers = headers
            .into_iter()
            .filter_map(|(n, v)| n.map(|n| (n, v)))
            .collect();

        let transport = StreamableHttpClientTransport::with_client(reqwest::Client::new(), config);

        // `()` is the client handler: we consume tools and offer the server nothing back.
        let service =
            ().serve(transport)
                .await
                .map_err(|error| McpError::Unreachable {
                    server: format!("{}.{}", endpoint.plugin, endpoint.server),
                    detail: error.to_string(),
                })?;

        Ok(Self { endpoint, service })
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Every tool this server offers, namespaced.
    pub async fn tools(&self) -> Result<Vec<McpTool>, McpError> {
        let tools = self
            .service
            .list_all_tools()
            .await
            .map_err(|error| McpError::Refused {
                server: format!("{}.{}", self.endpoint.plugin, self.endpoint.server),
                detail: error.to_string(),
            })?;

        Ok(tools
            .into_iter()
            .map(|tool| McpTool {
                qualified_name: self.endpoint.qualify(&tool.name),
                remote_name: tool.name.to_string(),
                description: tool.description.map(|text| text.to_string()),
            })
            .collect())
    }

    /// Call a tool by its REMOTE name — the qualified name is ours, and the server has never heard
    /// of it.
    pub async fn call(
        &self,
        remote_name: &str,
        arguments: serde_json::Value,
    ) -> Result<String, McpError> {
        let arguments = match arguments {
            serde_json::Value::Object(map) => Some(map),
            // A non-object argument is not something MCP can carry; sending nothing lets the
            // server answer with its own schema error, which is more useful than ours.
            _ => None,
        };

        // Built through the constructor rather than a literal: the params struct is
        // `#[non_exhaustive]`, so a literal would break on every rmcp release that adds a field.
        let mut params = CallToolRequestParams::new(remote_name.to_string());
        if let Some(arguments) = arguments {
            params = params.with_arguments(arguments);
        }

        let result = self
            .service
            .call_tool(params)
            .await
            .map_err(|error| McpError::Refused {
                server: format!("{}.{}", self.endpoint.plugin, self.endpoint.server),
                detail: error.to_string(),
            })?;

        Ok(render(&result))
    }

    /// Close the session politely, so the server can drop its state rather than time it out.
    pub async fn close(self) {
        let _ = self.service.cancel().await;
    }
}

/// Flatten a tool result into text the model can read.
///
/// Non-text content is NAMED rather than dropped: a model told nothing about an image concludes
/// the tool returned nothing, which is a different and wrong thing.
fn render(result: &rmcp::model::CallToolResult) -> String {
    let mut parts = Vec::new();
    for content in &result.content {
        match content.as_text() {
            Some(text) => parts.push(text.text.clone()),
            None => parts.push(format!("[{} content]", kind_of(content))),
        }
    }
    if let Some(structured) = &result.structured_content {
        parts.push(structured.to_string());
    }
    if parts.is_empty() {
        // An empty success is a real answer and must not read as a failure.
        return "(the tool returned no content)".to_string();
    }
    parts.join("\n")
}

fn kind_of(content: &rmcp::model::ContentBlock) -> &'static str {
    match content {
        rmcp::model::ContentBlock::Text(_) => "text",
        rmcp::model::ContentBlock::Image(_) => "image",
        rmcp::model::ContentBlock::Audio(_) => "audio",
        rmcp::model::ContentBlock::Resource(_) => "resource",
        rmcp::model::ContentBlock::ResourceLink(_) => "resource link",
        // The enum is non_exhaustive: a content kind added later is named generically rather than
        // dropped, so a model is never told a tool returned nothing when it returned something.
        _ => "other",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn declared(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn a_placeholder_is_filled_from_the_resolved_value() {
        let (filled, unresolved) = fill_placeholders(
            &declared(&[("authorization", "Bearer ${GITHUB_TOKEN}")]),
            &declared(&[("GITHUB_TOKEN", "gho_realtoken")]),
        );
        assert_eq!(filled.get("authorization").unwrap(), "Bearer gho_realtoken");
        assert!(unresolved.is_empty());
    }

    #[test]
    fn several_placeholders_in_one_value_all_resolve() {
        let (filled, _) = fill_placeholders(
            &declared(&[("x", "${A}-${B}")]),
            &declared(&[("A", "one"), ("B", "two")]),
        );
        assert_eq!(filled.get("x").unwrap(), "one-two");
    }

    /// A header reading `Bearer ${TOKEN}` is not a credential. Sending it produces a confusing 401
    /// instead of the honest "not connected" a person can act on.
    #[test]
    fn an_unresolved_placeholder_is_dropped_not_sent_literally() {
        let (filled, unresolved) = fill_placeholders(
            &declared(&[("authorization", "Bearer ${MISSING}")]),
            &declared(&[]),
        );
        assert!(filled.is_empty(), "{filled:?}");
        assert_eq!(unresolved, vec!["MISSING".to_string()]);
    }

    #[test]
    fn a_header_with_no_placeholder_passes_through() {
        let (filled, unresolved) =
            fill_placeholders(&declared(&[("x-client", "opengrok")]), &declared(&[]));
        assert_eq!(filled.get("x-client").unwrap(), "opengrok");
        assert!(unresolved.is_empty());
    }

    /// An unterminated `${` must not loop forever or panic.
    #[test]
    fn a_malformed_placeholder_does_not_hang() {
        let (filled, _) = fill_placeholders(&declared(&[("x", "Bearer ${OPEN")]), &declared(&[]));
        assert_eq!(filled.get("x").unwrap(), "Bearer ${OPEN");
    }

    /// Header names survive redaction and values do not: a 401 is much easier to debug when you
    /// can see that an `authorization` header was sent at all.
    #[test]
    fn an_endpoint_does_not_print_its_token() {
        let endpoint = Endpoint {
            plugin: "github".to_string(),
            server: "api".to_string(),
            url: "https://mcp.example/".to_string(),
            headers: declared(&[("authorization", "Bearer gho_verysecret")]),
        };
        let printed = format!("{endpoint:?}");
        assert!(!printed.contains("gho_verysecret"), "{printed}");
        assert!(printed.contains("authorization"), "{printed}");
    }

    #[test]
    fn tools_are_namespaced_by_plugin_and_server() {
        let endpoint = Endpoint {
            plugin: "github".to_string(),
            server: "api".to_string(),
            url: "https://x/".to_string(),
            headers: BTreeMap::new(),
        };
        assert_eq!(endpoint.qualify("search"), "github.api.search");
    }

    /// Two plugins bringing a `search` must stay distinguishable, or the model calls whichever won.
    #[test]
    fn two_plugins_with_the_same_tool_do_not_collide() {
        let first = Endpoint {
            plugin: "github".to_string(),
            server: "api".to_string(),
            url: "https://x/".to_string(),
            headers: BTreeMap::new(),
        };
        let second = Endpoint {
            plugin: "gdrive".to_string(),
            server: "api".to_string(),
            url: "https://y/".to_string(),
            headers: BTreeMap::new(),
        };
        assert_ne!(first.qualify("search"), second.qualify("search"));
    }

    #[test]
    fn a_qualified_name_splits_back_into_its_parts() {
        let (plugin, server, tool) = split_qualified("github.api.search").unwrap();
        assert_eq!(
            (plugin.as_str(), server.as_str(), tool.as_str()),
            ("github", "api", "search")
        );
    }

    /// A built-in tool must not be mistaken for a plugin's.
    #[test]
    fn a_builtin_tool_name_is_not_a_qualified_name() {
        assert!(split_qualified("shell").is_none());
        assert!(split_qualified("read_file").is_none());
        assert!(split_qualified("github.api").is_none());
    }

    /// A remote tool whose own name contains a dot must still round-trip.
    #[test]
    fn a_remote_tool_name_may_contain_dots() {
        let (plugin, server, tool) = split_qualified("gh.api.repos.list").unwrap();
        assert_eq!(plugin, "gh");
        assert_eq!(server, "api");
        assert_eq!(tool, "repos.list", "only the first two dots are separators");
    }
}
