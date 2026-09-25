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

use std::collections::{BTreeMap, BTreeSet};

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
    /// Said in words a model can act on, because it reaches one as a tool result (#199).
    #[error("{server} did not answer {detail}")]
    TimedOut { server: String, detail: String },
}

/// A tool a plugin's server offers, as the model will be told about it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpTool {
    /// `<plugin>.<server>.<tool>` — unique across every plugin installed.
    pub qualified_name: String,
    /// What the server calls it, which is what goes back on the wire.
    pub remote_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The server's `inputSchema`, EXACTLY AS SENT. The model is shown [`McpTool::parameters`],
    /// which is derived from this; the copy here stays untouched so a change to the cleaning
    /// works from what the server said rather than from an earlier edit of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<serde_json::Map<String, serde_json::Value>>,
    /// The server's `annotations`, as sent. HINTS FROM A SERVER WE DO NOT CONTROL — rmcp's own
    /// docs say a client must never decide on them — so policy and auto-review never read them:
    /// a server claiming `readOnlyHint` must not be how a call skips its card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<serde_json::Value>,
}

impl McpTool {
    /// What the model is offered for this tool's arguments: the server's schema made safe to
    /// advertise, or the open object when it cannot be. Never drops the tool — the offered set
    /// must equal the executed set, so a schema we cannot use costs the schema, not the tool.
    pub fn parameters(&self) -> serde_json::Value {
        advertised_parameters(self.input_schema.as_ref())
            .unwrap_or_else(|_| serde_json::Value::Object(open_object()))
    }
}

/// The most of ONE plugin tool's argument schema the model is shown, serialised. A remote schema
/// is text from a server we do not control landing in every request's context; past this it is
/// not worth the prompt it costs, and the server's own validation still answers a bad call.
pub const MAX_ADVERTISED_SCHEMA_BYTES: usize = 16 * 1024;

/// The most of ALL plugin tools' schemas one request carries. Per-tool capping alone lets one
/// server with hundreds of tools spend the whole prompt on itself.
pub const MAX_ADVERTISED_SCHEMAS_BYTES: usize = 64 * 1024;

/// Removed from the ROOT of an advertised schema. The root of a function's parameters has to be
/// a plain object to be offered at all, and these either describe the document rather than the
/// arguments (`$schema`, `$id`, `$comment`) or make the root something other than one object.
/// Dropping a root combinator only LOOSENS what the model is told; the server still validates.
const ROOT_KEYWORDS_NOT_ADVERTISED: &[&str] = &[
    "$schema", "$id", "$comment", "anyOf", "oneOf", "allOf", "not", "if", "then", "else", "enum",
    "const",
];

/// `{"type":"object"}` — what a tool is offered as when its own schema cannot be. One definition
/// for the executor and the MCP door, so the two cannot disagree about the fallback.
pub fn open_object() -> serde_json::Map<String, serde_json::Value> {
    let mut object = serde_json::Map::new();
    object.insert("type".to_string(), serde_json::Value::from("object"));
    object
}

/// The server's `inputSchema`, cleaned to be offered to a model, or the reason it cannot be.
///
/// THE IDENTITY KEYS ARE TAKEN OUT OF `properties` AND `required`. `strip_identity` removes them
/// from every call before it leaves, so a remote schema that asks for `coworker_id` would have the
/// model fill a value the server never receives — and a server that REQUIRES one would refuse
/// every call for a reason the model cannot fix (CLAUDE.md #7: the model gets no say in identity).
///
/// Everything else passes through verbatim, unknown keywords included: the schema is the server's
/// contract, and tidying it is how a tool ends up advertised with arguments it does not take.
pub fn advertised_parameters(
    raw: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    use serde_json::Value;

    let Some(raw) = raw else {
        return Err("the server sent no input schema".to_string());
    };
    let mut schema = raw.clone();
    match schema.get("type") {
        // `{}` is how many servers spell "takes no arguments".
        None => {}
        Some(Value::String(kind)) if kind == "object" => {}
        // `["object", "null"]` from a generator that marks everything nullable.
        Some(Value::Array(kinds)) if kinds.iter().any(|kind| kind == "object") => {}
        Some(other) => {
            return Err(format!(
                "its input schema has type {other}, and a tool's arguments must be an object"
            ));
        }
    }
    schema.insert("type".to_string(), Value::String("object".to_string()));
    for keyword in ROOT_KEYWORDS_NOT_ADVERTISED {
        schema.remove(*keyword);
    }
    match schema.get_mut("properties") {
        None => {}
        Some(Value::Object(properties)) => {
            for key in crate::review::IDENTITY_KEYS {
                properties.remove(*key);
            }
        }
        Some(_) => return Err("its `properties` is not an object".to_string()),
    }
    let required = match schema.remove("required") {
        Some(Value::Array(names)) => names
            .into_iter()
            .filter(|name| {
                !name
                    .as_str()
                    .is_some_and(|name| crate::review::IDENTITY_KEYS.contains(&name))
            })
            .collect(),
        // A `required` that is not a list says nothing a model can act on; the server still checks.
        _ => Vec::new(),
    };
    // An EMPTY list is left out rather than sent: draft-04 requires at least one entry, and
    // "nothing is required" is what its absence already means.
    if !required.is_empty() {
        schema.insert("required".to_string(), Value::Array(required));
    }

    let size = serialised_len(&Value::Object(schema.clone()));
    if size > MAX_ADVERTISED_SCHEMA_BYTES {
        return Err(format!(
            "its input schema is {size} bytes, over the {MAX_ADVERTISED_SCHEMA_BYTES}-byte cap"
        ));
    }
    Ok(Value::Object(schema))
}

/// Spend `budget` on `parameters`, or offer the open object once it is spent. The tool keeps its
/// place either way: dropping it here would leave a tool that runs but that nobody was told about.
pub fn within_budget(parameters: serde_json::Value, budget: &mut usize) -> serde_json::Value {
    let size = serialised_len(&parameters);
    if size <= *budget {
        *budget -= size;
        parameters
    } else {
        tracing::debug!(
            size,
            left = *budget,
            "the plugin schema budget for this request is spent; offering an open object"
        );
        serde_json::Value::Object(open_object())
    }
}

fn serialised_len(value: &serde_json::Value) -> usize {
    serde_json::to_string(value).map_or(usize::MAX, |text| text.len())
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

    /// `<plugin>.<server>` — how sessions are keyed and how a server is named to a person.
    pub fn key(&self) -> String {
        format!("{}.{}", self.plugin, self.server)
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

/// OpenAI `function.name` must match `^[a-zA-Z0-9_-]{1,64}$`. MCP qualify is
/// `{plugin}.{server}.{remote}` with dots (`gmail.api.send`); advertising that
/// as the wire name is a 400 from OpenAI. Internal `qualified_name` stays dotted
/// for `split_qualified` / sessions. Never empty: a name of only illegal chars
/// becomes `_`.
#[must_use]
pub fn openai_safe_tool_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len().min(64));
    for c in name.chars() {
        if out.len() >= 64 {
            break;
        }
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// Unique OpenAI-safe names for `qualified` against `reserved` (builtins already
/// on the wire). First claim keeps the plain sanitised form; later collisions
/// get `_2`, `_3`, … still capped at 64 so the mapping round-trips.
#[must_use]
pub fn openai_unique_tool_names(
    reserved: impl IntoIterator<Item = impl AsRef<str>>,
    qualified: impl IntoIterator<Item = impl AsRef<str>>,
) -> Vec<(String, String)> {
    let mut used: BTreeSet<String> = reserved
        .into_iter()
        .map(|name| openai_safe_tool_name(name.as_ref()))
        .collect();
    let mut out = Vec::new();
    for name in qualified {
        let original = name.as_ref().to_string();
        let wire = unique_openai_name(&mut used, &original);
        out.push((original, wire));
    }
    out
}

fn unique_openai_name(used: &mut BTreeSet<String>, name: &str) -> String {
    let base = openai_safe_tool_name(name);
    if used.insert(base.clone()) {
        return base;
    }
    let mut n: u32 = 2;
    loop {
        let suffix = format!("_{n}");
        let keep = 64usize.saturating_sub(suffix.len()).max(1);
        let mut candidate = base.clone();
        candidate.truncate(keep);
        if candidate.is_empty() {
            candidate.push('_');
        }
        candidate.push_str(&suffix);
        if candidate.len() > 64 {
            candidate.truncate(64);
        }
        if used.insert(candidate.clone()) {
            return candidate;
        }
        if n == u32::MAX {
            return candidate;
        }
        n += 1;
    }
}

// ---------------------------------------------------------------------------
// The live client.
//
// Kept below the pure part on purpose: everything above is testable without a network, and this is
// the thin layer that actually speaks to a server.
// ---------------------------------------------------------------------------

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rmcp::model::{CallToolRequest, CallToolRequestParams, ClientRequest, ServerResult};
use rmcp::service::PeerRequestOptions;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use rmcp::{ServiceError, ServiceExt};

/// How long a plugin server gets before it is treated as unavailable.
///
/// EVERY WAIT ON A REMOTE SERVER IS BOUNDED. `initialize`, `tools/list` and `tools/call` were
/// awaited with nothing around them, and every turn, resume, autonomy run and MCP-door call waits
/// on the listing before its first model call — so one server that accepted and never answered
/// hung every chat on the deployment (#199).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadlines {
    /// `initialize` and `tools/list` TOGETHER: the most a turn waits before starting without the
    /// server. `OG_PLUGIN_CONNECT_TIMEOUT_MS`.
    pub connect: Duration,
    /// One `tools/call`. `OG_PLUGIN_CALL_TIMEOUT_MS`.
    pub call: Duration,
}

impl Default for Deadlines {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(5),
            call: Duration::from_secs(60),
        }
    }
}

impl Deadlines {
    /// The defaults, overridden by the environment where it says something usable.
    pub fn from_env() -> Self {
        let defaults = Self::default();
        Self {
            connect: millis_from_env("OG_PLUGIN_CONNECT_TIMEOUT_MS").unwrap_or(defaults.connect),
            call: millis_from_env("OG_PLUGIN_CALL_TIMEOUT_MS").unwrap_or(defaults.call),
        }
    }
}

/// A positive number of milliseconds, or `None` with a warning when the value was there but
/// unusable. Zero is refused rather than read as "no deadline": an unbounded wait is the bug.
fn millis_from_env(name: &str) -> Option<Duration> {
    let raw = std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())?;
    match raw.trim().parse::<u64>() {
        Ok(ms) if ms > 0 => Some(Duration::from_millis(ms)),
        _ => {
            tracing::warn!(
                variable = name,
                value = raw,
                "not a positive number of milliseconds; using the default"
            );
            None
        }
    }
}

/// How long `tools/call` is given past its deadline to deliver the cancellation. rmcp sends
/// `notifications/cancelled` when its own timeout fires; this outer bound only exists so a wedged
/// send queue cannot turn the deadline back into a hang.
const CANCEL_GRACE: Duration = Duration::from_secs(2);

/// A live session with one MCP server.
pub struct Session {
    endpoint: Endpoint,
    service: rmcp::service::RunningService<rmcp::RoleClient, ()>,
    deadlines: Deadlines,
    /// Set when the transport failed under a request. A pooled session in this state is dialled
    /// again rather than handed to the next turn, which would fail the same way.
    broken: AtomicBool,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

impl Session {
    /// Connect with the default [`Deadlines`].
    pub async fn connect(endpoint: Endpoint) -> Result<Self, McpError> {
        Self::connect_within(endpoint, Deadlines::default()).await
    }

    /// Connect, performing the MCP initialize handshake, within `deadlines.connect`.
    ///
    /// The credential goes on the transport here — every request the session makes carries it, and
    /// nothing else in the process needs to know it.
    pub async fn connect_within(
        endpoint: Endpoint,
        deadlines: Deadlines,
    ) -> Result<Self, McpError> {
        let mut config = StreamableHttpClientTransportConfig::with_uri(endpoint.url.clone());

        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in &endpoint.headers {
            // `auth_header` WANTS THE BARE TOKEN. rmcp calls `bearer_auth()` on it, which prepends
            // the scheme — passing the whole header value produces `Bearer Bearer ghp_…`, which a
            // server rejects with a 401 that says nothing about why.
            //
            // A plugin declares the whole value (`Bearer ${TOKEN}`), so the scheme is stripped
            // here. Any OTHER scheme — Basic, a vendor's own — is sent verbatim as a custom
            // header instead, because `bearer_auth` would rewrite it into something the server
            // never agreed to.
            if name.eq_ignore_ascii_case("authorization") {
                match value
                    .strip_prefix("Bearer ")
                    .or_else(|| value.strip_prefix("bearer "))
                {
                    Some(token) => {
                        config.auth_header = Some(token.to_string());
                        continue;
                    }
                    None if !value.contains(' ') => {
                        // A bare token with no scheme at all.
                        config.auth_header = Some(value.clone());
                        continue;
                    }
                    None => {}
                }
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

        // A CONNECT timeout and never a whole-request one: the transport holds a long-lived
        // stream open for server messages, and a request timeout would cut it mid-session. The
        // handshake, the listing and each call are bounded one by one instead.
        let client = reqwest::Client::builder()
            .connect_timeout(deadlines.connect)
            .build()
            .map_err(|error| McpError::Unreachable {
                server: endpoint.key(),
                detail: error.to_string(),
            })?;
        let transport = StreamableHttpClientTransport::with_client(client, config);

        // `()` is the client handler: we consume tools and offer the server nothing back.
        let service = match tokio::time::timeout(deadlines.connect, ().serve(transport)).await {
            Ok(Ok(service)) => service,
            Ok(Err(error)) => {
                return Err(McpError::Unreachable {
                    server: endpoint.key(),
                    detail: error.to_string(),
                });
            }
            Err(_) => {
                return Err(McpError::TimedOut {
                    server: endpoint.key(),
                    detail: format!("initialize within {:?}", deadlines.connect),
                });
            }
        };

        Ok(Self {
            endpoint,
            service,
            deadlines,
            broken: AtomicBool::new(false),
        })
    }

    /// Whether a pool may hand this session to another turn.
    pub fn is_usable(&self) -> bool {
        !self.broken.load(Ordering::Relaxed)
            && !self.service.is_closed()
            && !self.service.peer().is_transport_closed()
    }

    /// Refused, and remembered as broken when the failure was the transport's rather than the
    /// server's own answer.
    fn refused(&self, error: ServiceError) -> McpError {
        if !matches!(error, ServiceError::McpError(_)) {
            self.broken.store(true, Ordering::Relaxed);
        }
        McpError::Refused {
            server: self.endpoint.key(),
            detail: error.to_string(),
        }
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Every tool this server offers, namespaced.
    ///
    /// Bounded by the connect deadline: a server that pages its cursor forever, or answers
    /// `initialize` and then goes quiet, is as unavailable to a turn as one that never answered.
    pub async fn tools(&self) -> Result<Vec<McpTool>, McpError> {
        let within = self.deadlines.connect;
        let tools = match tokio::time::timeout(within, self.service.list_all_tools()).await {
            Ok(Ok(tools)) => tools,
            Ok(Err(error)) => return Err(self.refused(error)),
            Err(_) => {
                self.broken.store(true, Ordering::Relaxed);
                return Err(McpError::TimedOut {
                    server: self.endpoint.key(),
                    detail: format!("tools/list within {within:?}"),
                });
            }
        };

        let tools: Vec<McpTool> = tools
            .into_iter()
            .map(|tool| McpTool {
                qualified_name: self.endpoint.qualify(&tool.name),
                remote_name: tool.name.to_string(),
                description: tool.description.map(|text| text.to_string()),
                input_schema: Some(std::sync::Arc::unwrap_or_clone(tool.input_schema)),
                annotations: tool
                    .annotations
                    .and_then(|annotations| serde_json::to_value(annotations).ok()),
            })
            .collect();
        // Said once per listing rather than once per request: the listing is where the schema
        // arrived, and a warning on every model round would bury the one that matters.
        for tool in &tools {
            if let Err(reason) = advertised_parameters(tool.input_schema.as_ref()) {
                tracing::warn!(
                    tool = tool.qualified_name,
                    reason,
                    "a plugin tool is offered as an open object instead of its own schema"
                );
            }
        }
        Ok(tools)
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

        // Sent with rmcp's own timeout rather than only dropped at ours: on its timeout rmcp
        // sends `notifications/cancelled`, so the server stops the work instead of finishing an
        // action nobody is waiting for.
        let within = self.deadlines.call;
        let peer = self.service.peer();
        let answered = tokio::time::timeout(within + CANCEL_GRACE, async {
            peer.send_request_with_option(
                ClientRequest::from(CallToolRequest::new(params)),
                PeerRequestOptions::with_timeout(within),
            )
            .await?
            .await_response()
            .await
        })
        .await;

        match answered {
            Ok(Ok(ServerResult::CallToolResult(result))) => Ok(render(&result)),
            // Asking us for input mid-call, or handing back a task to poll, is something this
            // client cannot answer — and saying so beats rendering a result that is not one.
            Ok(Ok(_)) => Err(McpError::Refused {
                server: self.endpoint.key(),
                detail: format!(
                    "{remote_name} answered with something other than a result (a request for \
                     input, or a task to poll), which this server cannot follow up"
                ),
            }),
            // A TIMED-OUT CALL MAY STILL HAVE HAPPENED. The email may have gone; the model must
            // be told to check rather than to send it again.
            //
            // And the session is not pooled again: rmcp sends each request's POST in turn, so a
            // server that never answers one holds every later request on this session behind it.
            Ok(Err(ServiceError::Timeout { .. })) | Err(_) => {
                self.broken.store(true, Ordering::Relaxed);
                Err(McpError::TimedOut {
                    server: self.endpoint.key(),
                    detail: format!(
                        "`{remote_name}` within {within:?}. It was cancelled, but it may already \
                         have taken effect — check before trying it again"
                    ),
                })
            }
            Ok(Err(error)) => Err(self.refused(error)),
        }
    }

    /// Close the session politely, so the server can drop its state rather than time it out —
    /// within the connect deadline, because a session wedged on an unanswered request cannot
    /// finish closing until that request does.
    pub async fn close(self) {
        let _ = tokio::time::timeout(self.deadlines.connect, self.service.cancel()).await;
    }
}

/// What reaching a turn's plugin servers produced.
#[derive(Debug, Default)]
pub struct Dialled {
    /// By `<plugin>.<server>`.
    pub sessions: BTreeMap<String, Arc<Session>>,
    /// Only the tools the caller's `permitted` let through, in server order.
    pub tools: Vec<McpTool>,
    /// By `<plugin>.<server>`: why a server that should have been reached was not, in words fit
    /// for the model — so a turn can say "GitHub is down right now" rather than nothing.
    pub unavailable: BTreeMap<String, String>,
}

/// One server as reached for one principal and coworker with one set of credentials.
///
/// KEYED BY THE FILLED HEADERS THEMSELVES, never a hash of them: a rotated or revoked token is a
/// different slot, so no session outlives the credential it was opened with, and two slots can
/// never collide into sharing one. Scoped to the principal and coworker as well, because an MCP
/// session can hold state, and a session one coworker opened must not be where another's calls
/// land. Deliberately not `Debug`: the headers carry tokens (CLAUDE.md #4).
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Slot {
    scope: String,
    url: String,
    server: String,
    headers: BTreeMap<String, String>,
}

#[derive(Clone)]
enum Pooled {
    Ready {
        session: Arc<Session>,
        tools: Vec<McpTool>,
        at: Instant,
    },
    /// Remembered briefly so a dead server costs one deadline per window, not one per turn.
    Failed { reason: String, at: Instant },
}

/// Sessions and their tool lists, reused across turns for a short while (#199).
///
/// A CACHE, NOT STATE. Nothing depends on an entry being here — losing one costs a handshake — so
/// it lives in process memory and a replica without it is merely slower (CLAUDE.md #5). The
/// UNFILTERED list is kept: the ceiling is applied on every dial, cached or not, because a grant
/// revoked a second ago must stop this turn (CLAUDE.md #6).
///
/// A server that restarted under a pooled session answers its next request with a 404, and rmcp's
/// transport re-initializes once and replays that request (`reinit_on_expired_session`, on by
/// default) — so reuse does not turn a server restart into a failed call.
pub struct Pool {
    deadlines: Deadlines,
    ttl: Duration,
    retry_after: Duration,
    slots: std::sync::Mutex<BTreeMap<Slot, Pooled>>,
}

impl Pool {
    /// A listed server is reused for this long before it is listed again.
    pub const TTL: Duration = Duration::from_secs(60);
    /// A server that failed is left alone for this long before it is tried again.
    pub const RETRY_AFTER: Duration = Duration::from_secs(30);

    pub fn new(deadlines: Deadlines) -> Self {
        Self {
            deadlines,
            ttl: Self::TTL,
            retry_after: Self::RETRY_AFTER,
            slots: std::sync::Mutex::new(BTreeMap::new()),
        }
    }

    #[must_use]
    pub fn with_reuse(mut self, ttl: Duration, retry_after: Duration) -> Self {
        self.ttl = ttl;
        self.retry_after = retry_after;
        self
    }

    /// The process's pool, with deadlines from the environment.
    pub fn global() -> &'static Pool {
        static POOL: std::sync::OnceLock<Pool> = std::sync::OnceLock::new();
        POOL.get_or_init(|| Pool::new(Deadlines::from_env()))
    }

    pub fn deadlines(&self) -> Deadlines {
        self.deadlines
    }

    /// Reach every endpoint at once, each within the connect deadline, reusing what is fresh.
    ///
    /// CONCURRENT, SO THE SLOWEST SERVER SETS THE WAIT, NOT THE SUM. And bounded, so the slowest
    /// is at most the deadline: a server that does not answer in time is left out of this turn
    /// and named in `unavailable`, and the others' tools are offered as usual.
    pub async fn dial(
        &self,
        scope: &str,
        endpoints: Vec<Endpoint>,
        permitted: impl Fn(&str) -> bool,
    ) -> Dialled {
        enum Outcome {
            Pooled(Pooled),
            Dialling(tokio::task::JoinHandle<Result<(Session, Vec<McpTool>), McpError>>),
        }

        let mut pending = Vec::with_capacity(endpoints.len());
        for endpoint in endpoints {
            let slot = Slot {
                scope: scope.to_string(),
                url: endpoint.url.clone(),
                server: endpoint.key(),
                headers: endpoint.headers.clone(),
            };
            let key = endpoint.key();
            let outcome = match self.fresh(&slot) {
                Some(pooled) => Outcome::Pooled(pooled),
                // Spawned rather than joined in place, so one server's handshake never waits on
                // another's; each task is bounded by the deadline on its own.
                None => Outcome::Dialling(tokio::spawn(connect_and_list(endpoint, self.deadlines))),
            };
            pending.push((key, slot, outcome));
        }

        // Collected in the order given, so the tools the model sees do not reorder from turn to
        // turn with whichever server happened to answer first.
        let mut dialled = Dialled::default();
        for (key, slot, outcome) in pending {
            let pooled = match outcome {
                Outcome::Pooled(pooled) => pooled,
                Outcome::Dialling(task) => {
                    let result = task.await.unwrap_or_else(|error| {
                        Err(McpError::Unreachable {
                            server: key.clone(),
                            detail: error.to_string(),
                        })
                    });
                    let pooled = match result {
                        Ok((session, tools)) => Pooled::Ready {
                            session: Arc::new(session),
                            tools,
                            at: Instant::now(),
                        },
                        Err(error) => {
                            tracing::warn!(%error, server = key, "a plugin server is unavailable; the turn goes on without it");
                            Pooled::Failed {
                                reason: error.to_string(),
                                at: Instant::now(),
                            }
                        }
                    };
                    self.remember(slot, pooled.clone());
                    pooled
                }
            };
            match pooled {
                Pooled::Ready { session, tools, .. } => {
                    dialled.tools.extend(
                        tools
                            .into_iter()
                            .filter(|tool| permitted(&tool.qualified_name)),
                    );
                    dialled.sessions.insert(key, session);
                }
                Pooled::Failed { reason, .. } => {
                    dialled.unavailable.insert(key, reason);
                }
            }
        }
        dialled
    }

    /// A pooled entry still worth using, sweeping out every one that is not.
    fn fresh(&self, slot: &Slot) -> Option<Pooled> {
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (ttl, retry_after) = (self.ttl, self.retry_after);
        // Dropping an evicted session cancels its service (rmcp's drop guard). A transport still
        // waiting on an unanswered request lets go when that request does, not before.
        slots.retain(|_, pooled| match pooled {
            Pooled::Ready { session, at, .. } => at.elapsed() < ttl && session.is_usable(),
            Pooled::Failed { at, .. } => at.elapsed() < retry_after,
        });
        slots.get(slot).cloned()
    }

    fn remember(&self, slot: Slot, pooled: Pooled) {
        self.slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(slot, pooled);
    }
}

/// `initialize` and `tools/list` under ONE deadline — the wait a turn can be made to sit through.
async fn connect_and_list(
    endpoint: Endpoint,
    deadlines: Deadlines,
) -> Result<(Session, Vec<McpTool>), McpError> {
    let server = endpoint.key();
    let listed = tokio::time::timeout(deadlines.connect, async {
        let session = Session::connect_within(endpoint, deadlines).await?;
        let tools = session.tools().await?;
        Ok((session, tools))
    })
    .await;
    listed.unwrap_or_else(|_| {
        Err(McpError::TimedOut {
            server,
            detail: format!("initialize and tools/list within {:?}", deadlines.connect),
        })
    })
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

    #[test]
    fn openai_safe_tool_name_maps_dots_and_never_goes_empty() {
        assert_eq!(openai_safe_tool_name("gmail.api.send"), "gmail_api_send");
        assert_eq!(openai_safe_tool_name("shell"), "shell");
        assert_eq!(
            openai_safe_tool_name("user_machine_shell"),
            "user_machine_shell"
        );
        assert_eq!(openai_safe_tool_name(""), "_");
        assert_eq!(openai_safe_tool_name("..."), "___");
        let long = format!("{}.api.{}", "p".repeat(40), "t".repeat(40));
        let safe = openai_safe_tool_name(&long);
        assert_eq!(safe.len(), 64);
        assert!(
            safe.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        );
        assert!(!safe.contains('.'));
    }

    #[test]
    fn colliding_sanitised_names_get_a_numeric_suffix() {
        let names = openai_unique_tool_names(
            ["shell"],
            ["gmail.api.send", "gmail_api.send", "foo.bar.baz"],
        );
        assert_eq!(
            names,
            vec![
                ("gmail.api.send".to_string(), "gmail_api_send".to_string()),
                ("gmail_api.send".to_string(), "gmail_api_send_2".to_string()),
                ("foo.bar.baz".to_string(), "foo_bar_baz".to_string()),
            ]
        );
        // A plugin that sanitises to a builtin does not steal the builtin wire name.
        let stolen = openai_unique_tool_names(["shell"], ["shell"]);
        assert_eq!(stolen[0].1, "shell_2");
        let long_a = "x".repeat(64);
        let long_b = "x".repeat(70);
        let longs =
            openai_unique_tool_names(Vec::<&str>::new(), [long_a.as_str(), long_b.as_str()]);
        assert_eq!(longs[0].1.len(), 64);
        assert!(longs[1].1.ends_with("_2"), "{:?}", longs[1]);
        assert!(longs[1].1.len() <= 64);
        assert_ne!(longs[0].1, longs[1].1);
    }
}
