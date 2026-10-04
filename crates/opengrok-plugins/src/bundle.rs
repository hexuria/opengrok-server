//! A registry snapshot is data, never executable install hooks. Unknown parts stay visible.
use crate::{Manifest, McpConfig, McpServer, Plugin, Trust};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
                let parsed = serde_json::from_value::<McpServer>(server);
                let keys_supported = |headers: &BTreeMap<String, String>| {
                    headers.values().all(|v| {
                        v.split("${").skip(1).all(|p| {
                            p.split_once('}')
                                .and_then(|(key, _)| key.strip_suffix("_TOKEN"))
                                .is_some_and(|name| {
                                    crate::is_valid_name(&name.to_lowercase())
                                        && crate::token_key(&name.to_lowercase())
                                            == format!("{name}_TOKEN")
                                })
                        })
                    })
                };
                let reason = match &parsed {
                    Ok(McpServer::StreamableHttp { url, .. }) if !url.starts_with("https://") => {
                        Some("remote MCP requires HTTPS")
                    }
                    Ok(McpServer::StreamableHttp { headers, .. }) if !keys_supported(headers) => {
                        Some("credential placeholders must use the CONNECTOR_TOKEN convention")
                    }
                    Ok(McpServer::StreamableHttp { .. })
                        if crate::is_valid_name(name) && !name.contains('.') =>
                    {
                        None
                    }
                    Ok(McpServer::Stdio { .. }) => {
                        Some("stdio MCP is refused: installing a bundle cannot launch a process")
                    }
                    Ok(McpServer::Sse { .. }) => Some("legacy SSE MCP has no client transport"),
                    _ => Some("MCP server shape or name is unsupported"),
                };
                parts.push(Part {
                    kind: "mcp".into(),
                    name: name.clone(),
                    supported: reason.is_none(),
                    reason: reason.map(str::to_string),
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
                let good = crate::is_valid_name(name)
                    && crate::is_valid_name(&format!("{}.{name}", manifest.name))
                    && parsed.closed
                    && parsed.body.chars().count() <= 8000
                    && parsed
                        .description
                        .as_ref()
                        .is_none_or(|d| d.chars().count() <= 300);
                parts.push(Part { kind: "skill".into(), name: name.into(), supported: good,
                    reason: (!good).then(|| "skill name or frontmatter is invalid, body exceeds 8000 characters, or description exceeds 300".into()) });
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
                if headers.is_empty() && values.contains_key(&key) {
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
                if headers.is_empty() {
                    names.insert(server_name.clone());
                }
                for value in headers.values() {
                    for part in value.split("${").skip(1) {
                        if let Some(key) = part
                            .split('}')
                            .next()
                            .and_then(|k| k.strip_suffix("_TOKEN"))
                        {
                            let key = key.to_lowercase();
                            if crate::is_valid_name(&key) {
                                names.insert(key);
                            }
                        }
                    }
                }
            }
        }
        names.into_iter().collect()
    }
}

#[cfg(test)]
#[path = "../tests/unit/bundle.rs"]
mod tests;
