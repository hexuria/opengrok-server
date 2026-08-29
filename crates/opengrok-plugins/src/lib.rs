//! Agent Plugins — the bundle, not the protocol.
//!
//! THREE LAYERS THAT ARE EASY TO CONFLATE, AND ARE NOT THE SAME THING:
//!
//! | Layer | What it is | Where |
//! |---|---|---|
//! | **Agent Plugin** | a folder you install: identity, skills, and a list of MCP servers | this crate |
//! | **MCP** | the protocol one of those servers speaks | the servers themselves |
//! | **rmcp** | a Rust client for that protocol | a dependency, used to *reach* a server |
//!
//! So `rmcp` is a part **inside** a plugin implementation, never an alternative to one. A plugin
//! typically brings several MCP servers *and* skills; loading one means reading its manifest,
//! reading its skills as text a coworker can be given, and connecting to each server it declares.
//!
//! TRANSCRIBED FROM THE PUBLISHED SCHEMAS, NOT REMEMBERED.
//! `https://agent-plugins.org/schemas/1.0.0/plugin.schema.json` and `…/mcp.schema.json`, read on
//! 29 Aug 2026. `plugin.json` requires `$schema` and `name`; `mcp.json` requires `$schema` and
//! `mcpServers`; a server is one of `stdio`, `streamable-http` or `sse`. The specification is
//! authoritative where it and the schema disagree, so anything below that the schema alone could
//! not tell us is marked.
//!
//! UNKNOWN FIELDS ARE KEPT. The format has a declared `extensions` object for client-specific data
//! and will grow; a loader that dropped what it did not recognise would quietly discard the half
//! of a plugin meant for somebody else.

pub mod catalogue;

pub use catalogue::{Admission, Catalogue, Entry, InstallError, Policy, Trust};

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const PLUGIN_SCHEMA: &str = "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json";
pub const MCP_SCHEMA: &str = "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json";

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("{0} could not be read: {1}")]
    Unreadable(PathBuf, String),
    #[error("{0} is not valid JSON: {1}")]
    Malformed(PathBuf, String),
    #[error("a plugin needs a plugin.json; none at {0}")]
    NoManifest(PathBuf),
    #[error("plugin name {0:?} is not allowed by the spec's pattern")]
    BadName(String),
}

/// `plugin.json`. Only `$schema` and `name` are required.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(rename = "$schema", default)]
    pub schema: Option<String>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<Author>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    /// Client-specific data, keyed by reverse-domain namespace. Carried whole: it is somebody
    /// else's half of the plugin, and dropping it is how a bundle silently loses features when it
    /// passes through us.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Author {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// `mcp.json`. A map of name → server; three transports.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(rename = "$schema", default)]
    pub schema: Option<String>,
    #[serde(rename = "mcpServers", default)]
    pub servers: BTreeMap<String, McpServer>,
}

/// One MCP server, as the schema's `oneOf` declares it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum McpServer {
    /// A process we launch and talk to over its stdin/stdout.
    Stdio {
        command: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        /// Values here are frequently `${SOME_TOKEN}` placeholders — which is exactly where a
        /// connector's credential is injected, at the edge, without the plugin ever holding one.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    StreamableHttp {
        url: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
    },
    /// The legacy transport. Carried because plugins in the wild still declare it.
    Sse {
        url: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
    },
}

impl McpServer {
    /// Whether reaching this server means launching a process on this machine.
    ///
    /// Load-bearing for us: a `stdio` server runs *here*, with our filesystem and our network, so
    /// it is a far bigger grant than an HTTP one and must be a deliberate choice rather than a
    /// side effect of installing a plugin.
    pub fn is_local_process(&self) -> bool {
        matches!(self, Self::Stdio { .. })
    }
}

/// One skill: `skills/<name>/SKILL.md`, plus whatever sits beside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    /// The frontmatter `description`, when present. It is what a coworker reads to decide whether
    /// a skill is relevant, so it is worth surfacing separately from the body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The instructions themselves, frontmatter stripped.
    pub body: String,
}

/// A loaded bundle.
#[derive(Debug, Clone)]
pub struct Plugin {
    pub root: PathBuf,
    pub manifest: Manifest,
    pub mcp: McpConfig,
    pub skills: Vec<Skill>,
    /// Whether anybody here has read this. Decided at install by the catalogue, carried with the
    /// plugin, and read by the policy layer — an unverified plugin's tools ask before each use.
    pub trust: Trust,
}

impl Plugin {
    /// Read a plugin from a directory.
    ///
    /// `mcp.json` and `skills/` are both OPTIONAL: a plugin may be only skills (words, no tools) or
    /// only servers (tools, no words), and both are useful. Requiring either would reject half the
    /// plugins that exist.
    pub fn load(root: impl AsRef<Path>) -> Result<Self, PluginError> {
        let root = root.as_ref().to_path_buf();

        let manifest_path = root.join("plugin.json");
        if !manifest_path.is_file() {
            return Err(PluginError::NoManifest(manifest_path));
        }
        let manifest: Manifest = read_json(&manifest_path)?;
        if !is_valid_name(&manifest.name) {
            return Err(PluginError::BadName(manifest.name));
        }

        let mcp_path = root.join("mcp.json");
        let mcp = if mcp_path.is_file() {
            read_json(&mcp_path)?
        } else {
            McpConfig::default()
        };

        Ok(Self {
            skills: load_skills(&root.join("skills"))?,
            root,
            manifest,
            mcp,
            // Unverified until a catalogue says otherwise. Loading a folder is not a review, and
            // the safe reading has to be the one you get by default.
            trust: Trust::Unverified,
        })
    }

    /// Record what the catalogue decided about this plugin at install time.
    #[must_use]
    pub fn with_trust(mut self, trust: Trust) -> Self {
        self.trust = trust;
        self
    }

    /// The tools this plugin contributes that must ask a person first.
    ///
    /// Named by `<plugin>.<server>` so two plugins bringing a server called `search` do not become
    /// one tool nobody can tell apart.
    pub fn tools_needing_approval(&self) -> Vec<String> {
        if !self.trust.requires_approval() {
            return Vec::new();
        }
        self.mcp
            .servers
            .keys()
            .map(|server| format!("{}.{server}", self.manifest.name))
            .collect()
    }

    /// Every MCP server this plugin brings, in a stable order.
    pub fn servers(&self) -> impl Iterator<Item = (&String, &McpServer)> {
        self.mcp.servers.iter()
    }

    /// Servers that would run as processes on this machine.
    pub fn local_processes(&self) -> Vec<&String> {
        self.mcp
            .servers
            .iter()
            .filter(|(_, server)| server.is_local_process())
            .map(|(name, _)| name)
            .collect()
    }
}

/// The spec's own name pattern, transcribed: lowercase alphanumerics, dots and dashes, starting and
/// ending alphanumeric, and never containing `--` or `..`.
///
/// Checked rather than trusted because the name reaches a filesystem path and a tool prefix; a name
/// with `..` in it is a directory traversal wearing a plugin's clothes.
pub fn is_valid_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    if name.contains("--") || name.contains("..") {
        return false;
    }
    let bytes = name.as_bytes();
    let alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let Some(&first) = bytes.first() else {
        return false;
    };
    let Some(&last) = bytes.last() else {
        return false;
    };
    if !alnum(first) || !alnum(last) {
        return false;
    }
    name.bytes().all(|b| alnum(b) || b == b'.' || b == b'-')
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, PluginError> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| PluginError::Unreadable(path.to_path_buf(), error.to_string()))?;
    serde_json::from_str(&text)
        .map_err(|error| PluginError::Malformed(path.to_path_buf(), error.to_string()))
}

fn load_skills(dir: &Path) -> Result<Vec<Skill>, PluginError> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let entries = std::fs::read_dir(dir)
        .map_err(|error| PluginError::Unreadable(dir.to_path_buf(), error.to_string()))?;

    let mut skills = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let skill_file = path.join("SKILL.md");
        if !skill_file.is_file() {
            // A directory without SKILL.md is not a skill. Skipped rather than refused: a stray
            // folder must not stop a plugin's other skills from loading.
            continue;
        }
        let text = std::fs::read_to_string(&skill_file)
            .map_err(|error| PluginError::Unreadable(skill_file.clone(), error.to_string()))?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();
        let (description, body) = split_frontmatter(&text);
        skills.push(Skill {
            name,
            description,
            body,
        });
    }
    // Sorted, so a coworker is given its skills in the same order every time — an unstable prompt
    // is an unreproducible run.
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(skills)
}

/// Pull `description` out of YAML-ish frontmatter and return the body without it.
///
/// Deliberately not a YAML parser: frontmatter here is a handful of `key: value` lines, and adding
/// a YAML dependency to read one field would be a large surface for a small gain. Anything it
/// cannot read is left in the body rather than lost.
fn split_frontmatter(text: &str) -> (Option<String>, String) {
    let trimmed = text.trim_start_matches('\u{feff}');
    if !trimmed.starts_with("---") {
        return (None, trimmed.to_string());
    }
    let mut lines = trimmed.lines();
    lines.next(); // the opening ---

    let mut description = None;
    let mut consumed = trimmed.len();
    let mut seen = trimmed.lines().next().map_or(0, |l| l.len() + 1);

    for line in lines {
        let line_len = line.len() + 1;
        if line.trim_end() == "---" {
            consumed = seen + line_len;
            break;
        }
        if let Some(value) = line.strip_prefix("description:") {
            description = Some(
                value
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string(),
            );
        }
        seen += line_len;
    }

    let body = trimmed
        .get(consumed..)
        .unwrap_or("")
        .trim_start()
        .to_string();
    (description.filter(|d| !d.is_empty()), body)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::fs;

    fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    fn a_plugin() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "plugin.json",
            r#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json",
                "name":"gmail","version":"1.2.0","description":"Read and send mail",
                "keywords":["mail"],
                "extensions":{"com.example.client":{"hooks":["on-send"]}}}"#,
        );
        write(
            dir.path(),
            "mcp.json",
            r#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/mcp.schema.json",
                "mcpServers":{
                  "gmail":{"type":"stdio","command":"gmail-mcp","args":["--stdio"],
                           "env":{"GMAIL_TOKEN":"${GMAIL_TOKEN}"}},
                  "hosted":{"type":"streamable-http","url":"https://mcp.example.com",
                            "headers":{"authorization":"Bearer ${TOKEN}"}}}}"#,
        );
        write(
            dir.path(),
            "skills/writing-replies/SKILL.md",
            "---\nname: writing-replies\ndescription: How to draft a good reply\n---\n\nBe brief.\n",
        );
        write(
            dir.path(),
            "skills/triage/SKILL.md",
            "No frontmatter at all, just instructions.\n",
        );
        dir
    }

    /// A plugin is a BUNDLE: identity, words and tools together.
    #[test]
    fn a_bundle_loads_its_manifest_skills_and_servers() {
        let dir = a_plugin();
        let plugin = Plugin::load(dir.path()).expect("should load");

        assert_eq!(plugin.manifest.name, "gmail");
        assert_eq!(plugin.manifest.version.as_deref(), Some("1.2.0"));
        assert_eq!(plugin.mcp.servers.len(), 2, "two servers");
        assert_eq!(plugin.skills.len(), 2, "two skills");
    }

    /// The three transports, exactly as the schema declares them.
    #[test]
    fn each_transport_parses_as_its_own_shape() {
        let dir = a_plugin();
        let plugin = Plugin::load(dir.path()).unwrap();

        match plugin.mcp.servers.get("gmail").unwrap() {
            McpServer::Stdio { command, env, .. } => {
                assert_eq!(command, "gmail-mcp");
                // The placeholder is where a credential is injected at the edge; the plugin never
                // holds one itself.
                assert_eq!(env.get("GMAIL_TOKEN").unwrap(), "${GMAIL_TOKEN}");
            }
            other => panic!("expected stdio, got {other:?}"),
        }
        match plugin.mcp.servers.get("hosted").unwrap() {
            McpServer::StreamableHttp { url, .. } => assert_eq!(url, "https://mcp.example.com"),
            other => panic!("expected streamable-http, got {other:?}"),
        }
    }

    #[test]
    fn the_legacy_sse_transport_still_parses() {
        let config: McpConfig =
            serde_json::from_str(r#"{"mcpServers":{"old":{"type":"sse","url":"https://x/sse"}}}"#)
                .unwrap();
        assert!(matches!(
            config.servers.get("old").unwrap(),
            McpServer::Sse { .. }
        ));
    }

    /// A stdio server runs on THIS machine. Knowing which ones do is what lets that be a decision
    /// rather than a side effect of installing something.
    #[test]
    fn local_processes_are_identified_separately() {
        let dir = a_plugin();
        let plugin = Plugin::load(dir.path()).unwrap();
        assert_eq!(plugin.local_processes(), vec!["gmail"]);
    }

    #[test]
    fn frontmatter_becomes_a_description_and_leaves_the_body() {
        let dir = a_plugin();
        let plugin = Plugin::load(dir.path()).unwrap();
        let skill = plugin
            .skills
            .iter()
            .find(|skill| skill.name == "writing-replies")
            .unwrap();
        assert_eq!(
            skill.description.as_deref(),
            Some("How to draft a good reply")
        );
        assert_eq!(skill.body.trim(), "Be brief.");
        assert!(
            !skill.body.contains("---"),
            "frontmatter should be stripped"
        );
    }

    /// A skill without frontmatter is still a skill; its body must survive whole.
    #[test]
    fn a_skill_without_frontmatter_keeps_all_of_its_text() {
        let dir = a_plugin();
        let plugin = Plugin::load(dir.path()).unwrap();
        let skill = plugin
            .skills
            .iter()
            .find(|skill| skill.name == "triage")
            .unwrap();
        assert_eq!(skill.description, None);
        assert!(skill.body.contains("just instructions"));
    }

    /// Skills arrive in a stable order: an unstable prompt is an unreproducible run.
    #[test]
    fn skills_are_ordered_the_same_way_every_time() {
        let dir = a_plugin();
        let names: Vec<_> = Plugin::load(dir.path())
            .unwrap()
            .skills
            .into_iter()
            .map(|skill| skill.name)
            .collect();
        assert_eq!(names, vec!["triage", "writing-replies"]);
    }

    /// Client-specific data belongs to somebody else and must pass through untouched.
    #[test]
    fn client_extensions_are_carried_not_dropped() {
        let dir = a_plugin();
        let plugin = Plugin::load(dir.path()).unwrap();
        let extensions = plugin.manifest.extensions.expect("extensions kept");
        assert!(
            extensions.get("com.example.client").is_some(),
            "{extensions:?}"
        );
    }

    /// Both halves are optional, because plugins exist that are only one of them.
    #[test]
    fn a_plugin_may_be_only_skills_or_only_servers() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "plugin.json", r#"{"name":"words-only"}"#);
        write(dir.path(), "skills/a/SKILL.md", "just words");
        let plugin = Plugin::load(dir.path()).unwrap();
        assert!(plugin.mcp.servers.is_empty());
        assert_eq!(plugin.skills.len(), 1);

        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "plugin.json", r#"{"name":"tools-only"}"#);
        write(
            dir.path(),
            "mcp.json",
            r#"{"mcpServers":{"a":{"type":"stdio","command":"x"}}}"#,
        );
        let plugin = Plugin::load(dir.path()).unwrap();
        assert!(plugin.skills.is_empty());
        assert_eq!(plugin.mcp.servers.len(), 1);
    }

    #[test]
    fn a_folder_without_a_manifest_is_not_a_plugin() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            Plugin::load(dir.path()),
            Err(PluginError::NoManifest(_))
        ));
    }

    /// A name reaches a filesystem path and a tool prefix, so `..` is traversal in a costume.
    #[test]
    fn a_name_that_could_escape_a_directory_is_refused() {
        for bad in [
            "../etc",
            "a..b",
            "a--b",
            "-leading",
            "trailing-",
            "Upper",
            "",
        ] {
            assert!(!is_valid_name(bad), "{bad:?} should be refused");
        }
        for good in ["gmail", "acme.gmail", "my-plugin", "a1"] {
            assert!(is_valid_name(good), "{good:?} should be allowed");
        }
    }

    #[test]
    fn a_malformed_manifest_says_which_file_and_why() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "plugin.json", "{not json");
        match Plugin::load(dir.path()) {
            Err(PluginError::Malformed(path, _)) => {
                assert!(path.ends_with("plugin.json"), "{path:?}");
            }
            other => panic!("expected a malformed error, got {other:?}"),
        }
    }

    /// Loading a folder is not a review: the safe reading is what you get by default.
    #[test]
    fn a_freshly_loaded_plugin_is_unverified_until_told_otherwise() {
        let dir = a_plugin();
        let plugin = Plugin::load(dir.path()).unwrap();
        assert_eq!(plugin.trust, Trust::Unverified);
        assert!(plugin.trust.requires_approval());
    }

    /// An unverified plugin's tools ask first — that is what "unverified" *does*.
    #[test]
    fn an_unverified_plugins_tools_all_need_a_human_yes() {
        let dir = a_plugin();
        let plugin = Plugin::load(dir.path()).unwrap();
        let gated = plugin.tools_needing_approval();
        // Namespaced, so two plugins with a `search` server stay distinguishable.
        assert!(gated.contains(&"gmail.gmail".to_string()), "{gated:?}");
        assert!(gated.contains(&"gmail.hosted".to_string()), "{gated:?}");
    }

    #[test]
    fn a_verified_plugin_gates_nothing_extra() {
        let dir = a_plugin();
        let plugin = Plugin::load(dir.path())
            .unwrap()
            .with_trust(Trust::Verified);
        assert!(plugin.tools_needing_approval().is_empty());
    }

    /// A stray folder must not stop the rest of a plugin's skills from loading.
    #[test]
    fn a_directory_without_a_skill_file_is_skipped_quietly() {
        let dir = a_plugin();
        fs::create_dir_all(dir.path().join("skills/not-a-skill")).unwrap();
        assert_eq!(Plugin::load(dir.path()).unwrap().skills.len(), 2);
    }
}
