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
        let parsed = split_frontmatter(&text);
        skills.push(Skill {
            name,
            description: parsed.description,
            body: parsed.body,
        });
    }
    // Sorted, so a coworker is given its skills in the same order every time — an unstable prompt
    // is an unreproducible run.
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(skills)
}

/// A `SKILL.md` taken apart: what its frontmatter claimed, and the instructions under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frontmatter {
    /// The frontmatter `name`. The folder loader ignores it — a skill on disk is named by its
    /// directory — but an uploaded `SKILL.md` has no directory, so this is the only name it can
    /// have arrived with.
    pub name: Option<String>,
    pub description: Option<String>,
    /// The instructions themselves, frontmatter stripped. On an unclosed fence this is the WHOLE
    /// text, fence and all: nothing is dropped on the floor.
    pub body: String,
    /// Whether the opening `---` was matched by a closing one. `true` when there was no
    /// frontmatter at all — there was nothing to close.
    ///
    /// A caller that accepts text from a person MUST check this. An unclosed fence means the
    /// whole document was read as body, and a route that ignored the flag would answer 200 to an
    /// upload it had in fact failed to understand.
    pub closed: bool,
}

/// Pull `name` and `description` out of YAML-ish frontmatter and return the body without it.
///
/// Deliberately not a YAML parser: frontmatter here is a handful of `key: value` lines, and adding
/// a YAML dependency to read two fields would be a large surface for a small gain. Anything it
/// cannot read is left in the body rather than lost.
///
/// THE ONE PARSER. The server's `/skills` upload path reads the same bytes this does, and a second
/// implementation there would mean an uploaded skill and an installed one disagreeing about where
/// a body starts — the disagreement would show up as frontmatter leaking into a system message.
///
/// NOT EVERY CALLER CHECKS `closed`, AND ONE OF THEM IS THE BOOT-TIME LOADER. `skills_in` above
/// takes `parsed.body` and never looks at the flag, so a `SKILL.md` on disk that opens with a
/// `---` it never closes is installed with its whole text as the body rather than refused the way
/// an upload would be. Since leading whitespace is stripped here, that now includes a file whose
/// first non-blank line is a `---` used as a horizontal rule: everything up to the next `---`
/// becomes frontmatter and is dropped. Both are consequences of the loader ignoring the flag, not
/// of the parse — a caller that accepts text from a person or a disk MUST read `closed`.
pub fn split_frontmatter(text: &str) -> Frontmatter {
    // LEADING WHITESPACE GOES BEFORE THE FENCE IS LOOKED FOR, and it is not tidiness. The test
    // below is `starts_with("---")`, so ONE blank line in front of the fence made the whole
    // frontmatter block body — and a body is what a model is later handed as instructions. Two
    // documents one newline apart were then parsed in opposite ways: `---\nname: x` with no
    // closing fence was refused as unreadable, and `\n---\nname: x` was stored with its `name:`
    // line as the first line of the instructions. A model asked for a `SKILL.md` puts a newline
    // after its opening marker as often as not, so this was reachable without anybody trying.
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    if !trimmed.starts_with("---") {
        return Frontmatter {
            name: None,
            description: None,
            body: trimmed.to_string(),
            closed: true,
        };
    }

    let mut name = None;
    let mut description = None;
    let mut consumed = None;
    let mut seen = 0usize;
    // OFFSETS COME OFF THE SLICES THEMSELVES, never rebuilt as `line.len() + 1`.
    //
    // `str::lines()` strips `\r\n` as one, so the rebuilt offset was a byte short for every CRLF
    // line and the body slice started INSIDE the closing fence: `---\r\nname: a\r\n---\r\nBody`
    // came back as a body of `-\r\nBody`, and the error grew a byte per frontmatter line until
    // the tail of the last `key: value` line landed in the body — which is then stored as
    // version 1 and read out into a system message. A file written on Windows, or checked out
    // with `core.autocrlf=true`, is all it takes.
    for (index, line) in trimmed.split_inclusive('\n').enumerate() {
        seen += line.len();
        if index == 0 {
            continue; // the opening ---
        }
        let content = line.trim_end();
        if content == "---" {
            consumed = Some(seen);
            break;
        }
        if let Some(value) = content.strip_prefix("name:") {
            name = Some(unquote(value));
        }
        if let Some(value) = content.strip_prefix("description:") {
            description = Some(unquote(value));
        }
    }

    let Some(consumed) = consumed else {
        // NO CLOSING FENCE. Everything is returned as body — the doc comment above promises that
        // what cannot be read is left in the body rather than lost — and `closed` is false so a
        // caller can refuse instead of storing a document it only half understood. Returning an
        // empty body here (which is what this used to do) made a mistyped fence look like a
        // successful upload of a skill with nothing in it.
        return Frontmatter {
            name: None,
            description: None,
            body: trimmed.to_string(),
            closed: false,
        };
    };

    let body = trimmed
        .get(consumed..)
        .unwrap_or("")
        .trim_start()
        .to_string();
    Frontmatter {
        name: name.filter(|value| !value.is_empty()),
        description: description.filter(|value| !value.is_empty()),
        body,
        closed: true,
    }
}

fn unquote(value: &str) -> String {
    value
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .to_string()
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

    /// The upload path has no folder to take a name from, so the frontmatter `name` has to
    /// survive the parse even though the folder loader above does not use it.
    #[test]
    fn frontmatter_carries_the_name_for_a_skill_that_arrived_without_a_folder() {
        let parsed = split_frontmatter(
            "---\nname: writing-replies\ndescription: \"How to draft a good reply\"\n---\nBe brief.\n",
        );
        assert_eq!(parsed.name.as_deref(), Some("writing-replies"));
        assert_eq!(
            parsed.description.as_deref(),
            Some("How to draft a good reply")
        );
        assert_eq!(parsed.body.trim(), "Be brief.");
    }

    /// A file written on Windows parses to the same body as one written on a Mac. The offsets
    /// used to be rebuilt as `line.len() + 1`, one byte short per CRLF line, so the body started
    /// inside the closing fence and the error grew with every frontmatter line.
    #[test]
    fn a_crlf_skill_parses_exactly_like_an_lf_one() {
        let lf = split_frontmatter("---\nname: a\ndescription: d\n---\nBody\n");
        let crlf = split_frontmatter("---\r\nname: a\r\ndescription: d\r\n---\r\nBody\r\n");
        assert_eq!(crlf.name, lf.name);
        assert_eq!(crlf.description, lf.description);
        assert_eq!(crlf.body.trim_end(), "Body");
        assert!(crlf.closed);

        // The error used to compound, so a long frontmatter is the case that proves the fix:
        // with eight lines the tail of the last one landed in the body.
        let many = "---\r\n".to_string()
            + &(1..=8)
                .map(|n| format!("key{n}: value{n}\r\n"))
                .collect::<String>()
            + "description: kept\r\n---\r\nThe body, whole.\r\n";
        let parsed = split_frontmatter(&many);
        assert_eq!(parsed.description.as_deref(), Some("kept"));
        assert_eq!(parsed.body.trim_end(), "The body, whole.");
        assert!(
            !parsed.body.contains("value8") && !parsed.body.contains('-'),
            "no frontmatter leaked into the body: {:?}",
            parsed.body
        );
    }

    /// A mistyped closing fence must not look like a successful parse of an empty skill.
    #[test]
    fn an_unclosed_fence_keeps_every_byte_and_says_it_is_unclosed() {
        let text = "---\nname: a\ndescription: d\nBody with no closing fence.\n";
        let parsed = split_frontmatter(text);
        assert!(!parsed.closed, "the fence was never closed");
        assert_eq!(parsed.body, text, "nothing is dropped on the floor");
        assert_eq!(parsed.name, None, "an unread block claims nothing");
        assert_eq!(parsed.description, None);

        // A document with no frontmatter at all has nothing to close, so it is not "unclosed".
        assert!(split_frontmatter("Just instructions.\n").closed);
    }

    /// ONE BLANK LINE was enough to smuggle a whole frontmatter block into a body, and the body
    /// is what a coworker is given as instructions. Reproduced by a reviewer on the from-tape
    /// route, where the writer is a model rather than a person and puts one there by habit.
    #[test]
    fn a_fence_after_a_blank_line_is_still_frontmatter() {
        for text in [
            "\n---\nname: a\ndescription: d\n---\nBody\n",
            "  \n\t---\nname: a\ndescription: d\n---\nBody\n",
            "\u{feff}\n---\nname: a\ndescription: d\n---\nBody\n",
        ] {
            let parsed = split_frontmatter(text);
            assert_eq!(parsed.name.as_deref(), Some("a"), "{text:?}");
            assert_eq!(parsed.description.as_deref(), Some("d"), "{text:?}");
            assert_eq!(parsed.body.trim(), "Body", "{text:?}");
            assert!(parsed.closed, "{text:?}");
        }
        // And its unclosed twin is refusable rather than storable: the pair used to disagree.
        assert!(!split_frontmatter("\n---\nname: a\nBody\n").closed);
        assert!(!split_frontmatter("---\nname: a\nBody\n").closed);
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
