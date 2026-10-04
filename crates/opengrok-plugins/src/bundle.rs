//! A registry snapshot is data, never executable install hooks. Unknown parts stay visible.
use crate::skill::{MAX_SKILL_BODY_CHARS, MAX_SKILL_DESCRIPTION_CHARS};
use crate::{Manifest, McpConfig, McpServer, Plugin, Trust};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::IpAddr;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Part {
    pub kind: String,
    pub name: String,
    pub supported: bool,
    pub reason: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bundle {
    pub manifest: Manifest,
    pub mcp: McpConfig,
    pub skills: BTreeMap<String, String>,
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    pub parts: Vec<Part>,
}

impl Bundle {
    /// Files have already been limited and paths checked by the registry boundary.
    pub fn from_files(files: &BTreeMap<String, String>) -> Result<Self, String> {
        let manifest_text = [
            "plugin.json",
            ".grok-plugin/plugin.json",
            ".claude-plugin/plugin.json",
        ]
        .iter()
        .find_map(|path| files.get(*path))
        .ok_or("no plugin manifest")?;
        let raw: serde_json::Value =
            serde_json::from_str(manifest_text).map_err(|_| "invalid manifest JSON")?;
        // xAI permits an object-valued author; other marketplaces also use a display string.
        let mut adapted = raw.clone();
        if adapted["author"].is_string() {
            adapted["author"] = serde_json::json!({"name": adapted["author"]});
        }
        let manifest: Manifest =
            serde_json::from_value(adapted).map_err(|_| "invalid plugin manifest")?;
        if !crate::is_valid_name(&manifest.name) || manifest.name.contains('.') {
            return Err("plugin names must be unambiguous tool prefixes".into());
        }
        let mut parts = Vec::new();
        let mut mcp = McpConfig::default();
        if let Some(text) = files.get("mcp.json").or_else(|| files.get(".mcp.json")) {
            let config: serde_json::Value =
                serde_json::from_str(text).map_err(|_| "invalid MCP JSON")?;
            let servers = config
                .get("mcpServers")
                .and_then(|v| v.as_object())
                .ok_or("MCP config needs mcpServers")?;
            for (name, server) in servers {
                let mut server = server.clone();
                // Provenance: hexuria/plugin-marketplace .mcp.json entries use `http`; the
                // agent-plugins schema calls the same transport `streamable-http`.
                if server["type"] == "http" {
                    server["type"] = "streamable-http".into();
                }
                if server.get("type").is_none() && server.get("command").is_some() {
                    server["type"] = "stdio".into();
                }
                let parsed = serde_json::from_value::<McpServer>(server.clone());
                let reason = match &parsed {
                    Ok(McpServer::StreamableHttp { url, headers }) => {
                        hosted_refusal(name, url, headers, &server)
                    }
                    Ok(McpServer::Stdio { .. }) => Some(
                        "stdio MCP is refused: installing a bundle cannot launch a process".into(),
                    ),
                    Ok(McpServer::Sse { .. }) => {
                        Some("legacy SSE MCP has no client transport".into())
                    }
                    Err(_) => Some("MCP server shape is unsupported".into()),
                };
                parts.push(Part {
                    kind: "mcp".into(),
                    name: name.clone(),
                    supported: reason.is_none(),
                    reason: reason.clone(),
                });
                if reason.is_none()
                    && let Ok(server) = parsed
                {
                    mcp.servers.insert(name.clone(), server);
                }
            }
        }
        let mut skills = BTreeMap::new();
        for (path, text) in files {
            if let Some(name) = path
                .strip_prefix("skills/")
                .and_then(|p| p.strip_suffix("/SKILL.md"))
            {
                let parsed = crate::split_frontmatter(text);
                let (body_cap, description_cap) =
                    (MAX_SKILL_BODY_CHARS, MAX_SKILL_DESCRIPTION_CHARS);
                // One reason each: a single catch-all left the one real plugin probed (Exa)
                // reporting "invalid" with no way to tell which rule it broke.
                let reason = if !crate::is_valid_name(name)
                    || !crate::is_valid_name(&format!("{}.{name}", manifest.name))
                {
                    Some("skill name is not a valid name".to_string())
                } else if !parsed.closed {
                    Some("SKILL.md frontmatter has no closing fence".into())
                } else if parsed.body.chars().count() > body_cap {
                    Some(format!("skill body exceeds {body_cap} characters"))
                } else if parsed
                    .description
                    .as_ref()
                    .is_some_and(|d| d.chars().count() > description_cap)
                {
                    Some(format!(
                        "skill description exceeds {description_cap} characters"
                    ))
                } else {
                    None
                };
                let good = reason.is_none();
                parts.push(Part {
                    kind: "skill".into(),
                    name: name.into(),
                    supported: good,
                    reason,
                });
                if good {
                    skills.insert(name.into(), text.clone());
                }
            }
        }
        for kind in ["commands", "agents", "hooks", "lsp"] {
            if raw.get(kind).is_some()
                || files
                    .keys()
                    .any(|p| p.starts_with(&format!("{kind}/")) || p == &format!(".{kind}.json"))
            {
                parts.push(Part {
                    kind: kind.into(),
                    name: kind.into(),
                    supported: false,
                    reason: Some(format!("{kind} has no loader (#361)")),
                });
            }
        }
        Ok(Self {
            manifest,
            mcp,
            skills,
            files: files
                .iter()
                .filter(|(p, _)| p.starts_with("skills/") && !p.ends_with("/SKILL.md"))
                .map(|(p, t)| (p.clone(), t.clone()))
                .collect(),
            parts,
        })
    }
    pub fn plugin(&self) -> Plugin {
        Plugin {
            root: Default::default(),
            manifest: self.manifest.clone(),
            mcp: self.mcp.clone(),
            trust: Trust::Unverified,
        }
    }
    /// A hosted server with no auth declaration can run keylessly. If its owner explicitly
    /// supplies a bearer token, attach it to that server only; never infer one from global keys.
    pub fn plugin_with_values(&self, values: &BTreeMap<String, String>) -> Plugin {
        let mut plugin = self.plugin();
        for (name, server) in &mut plugin.mcp.servers {
            if let McpServer::StreamableHttp { headers, .. } = server {
                let key = crate::token_key(name);
                if declares_no_auth(headers) && values.contains_key(&key) {
                    headers.insert("Authorization".into(), format!("Bearer ${{{key}}}"));
                }
            }
        }
        plugin
    }
    pub fn connectors(&self) -> Vec<String> {
        let mut names = std::collections::BTreeSet::new();
        for (server_name, server) in &self.mcp.servers {
            if let McpServer::StreamableHttp { headers, .. } = server {
                if declares_no_auth(headers) {
                    names.insert(server_name.clone());
                }
                names.extend(headers.values().flat_map(|v| placeholders(v)).flatten());
            }
        }
        names.into_iter().collect()
    }
}

/// Neither an auth header nor a placeholder: the server may be keyless, or take a bearer its
/// owner supplies under the server's own name.
fn declares_no_auth(headers: &BTreeMap<String, String>) -> bool {
    !headers.values().any(|value| value.contains("${"))
        && !headers.keys().any(|name| {
            name.eq_ignore_ascii_case("authorization") || name.eq_ignore_ascii_case("x-api-key")
        })
}

/// Every `${...}` in a header value, as the connector it names (`${DEMO_TOKEN}` is `demo`), or
/// `None` for one outside the `<CONNECTOR>_TOKEN` convention. The one scanner both the parse-time
/// refusal and `connectors` read, so the two can never disagree about what a placeholder is.
fn placeholders(value: &str) -> impl Iterator<Item = Option<String>> + '_ {
    value.split("${").skip(1).map(|rest| {
        let key = rest.split_once('}')?.0;
        let connector = key.strip_suffix("_TOKEN")?.to_lowercase();
        (crate::is_valid_name(&connector) && crate::token_key(&connector) == key)
            .then_some(connector)
    })
}

/// The fields a hosted entry may carry. Anything else is somebody's instruction we would drop:
/// `disabled`, an `oauth` block, a tool allow-list. Dialling the server as though it were not
/// there turned on servers their author switched off.
const HOSTED_FIELDS: [&str; 3] = ["type", "url", "headers"];

fn hosted_refusal(
    name: &str,
    url: &str,
    headers: &BTreeMap<String, String>,
    raw: &serde_json::Value,
) -> Option<String> {
    let extra = raw
        .as_object()
        .into_iter()
        .flat_map(|o| o.keys())
        .find(|key| !HOSTED_FIELDS.contains(&key.as_str()));
    if !crate::is_valid_name(name) || name.contains('.') {
        Some("MCP server name is not a valid tool prefix".into())
    } else if !url.starts_with("https://") {
        Some("remote MCP requires HTTPS".into())
    } else if !public_https(url) {
        Some("remote MCP must name a public host".into())
    } else if raw["disabled"] == true || raw["enabled"] == false {
        Some("its author switched this server off".into())
    } else if raw.get("oauth").is_some() {
        Some("this server needs OAuth, which an installed plugin cannot do yet (#364)".into())
    } else if let Some(field) = extra {
        Some(format!(
            "MCP field `{}` is not supported",
            field.chars().take(40).collect::<String>()
        ))
    } else if headers
        .values()
        .flat_map(|v| placeholders(v))
        .any(|c| c.is_none())
    {
        Some("credential placeholders must use the CONNECTOR_TOKEN convention".into())
    } else {
        None
    }
}

/// Whether an `https://` URL's host may be dialled for an account's bundle: never a loopback,
/// private, link-local or otherwise non-public address literal, never `localhost`, and never with
/// userinfo, which only serves to make the host hard to read. A NAME is checked again when it is
/// resolved, address by address (`opengrok_integrations::net`), since DNS can answer anything.
pub fn public_https(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return false;
    }
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host),
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() || host == "localhost" || host.ends_with(".localhost") {
        return false;
    }
    // A URL parser reads a host whose last label is a number (`127.1`, `0x7f.1`, `2130706433`) as
    // an IPv4 address, and an address literal is never put to the resolver that would refuse it.
    // So a numeric host must be an address `IpAddr` can read and judge, or it is refused.
    let last = host.rsplit('.').next().unwrap_or_default();
    let numeric = last.starts_with("0x") || last.bytes().all(|b| b.is_ascii_digit());
    match host.parse::<IpAddr>() {
        Ok(ip) => is_public_ip(ip),
        Err(_) => !numeric,
    }
}

/// Whether an address is reachable on the public internet. `IpAddr::is_global` is still unstable,
/// so this is its ranges, transcribed from the IANA special-purpose registries. Fails closed: a
/// range missing from here is refused, never dialled.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            !(a == 0
                || a == 10
                || a == 127
                || a >= 224
                || (a == 100 && (64..128).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..32).contains(&b))
                || (a == 192 && b == 168)
                || (a == 192 && b == 0 && (c == 0 || c == 2))
                || (a == 198 && (b == 18 || b == 19))
                || (a == 198 && b == 51 && c == 100)
                || (a == 203 && b == 0 && c == 113))
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_ip(IpAddr::V4(v4));
            }
            let s = v6.segments();
            // Only global unicast (2000::/3) is public; inside it, documentation, ORCHID, 6to4
            // and Teredo either are not reachable or wrap an address this check cannot see.
            (s[0] & 0xe000) == 0x2000
                && !(s[0] == 0x2001 && (s[1] == 0x0db8 || s[1] < 0x0200))
                && s[0] != 0x2002
                && !(s[0] == 0x3fff && s[1] < 0x1000)
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/bundle.rs"]
mod tests;
