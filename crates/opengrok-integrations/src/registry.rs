//! GitHub registry adapter. Sources are read at immutable commits, with bounded bytes and time.
use opengrok_plugins::bundle::Bundle;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Clone)]
pub struct Registry {
    client: reqwest::Client,
    api: String,
    raw: String,
    repo: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub name: String,
    pub description: String,
    pub repository: String,
    pub revision: String,
    pub path: String,
    pub unavailable_reason: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    pub registry: String,
    pub revision: String,
    pub plugins: Vec<Entry>,
}
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Error(pub String);
fn bad(why: &str) -> Error {
    Error(why.into())
}
pub fn revision_ok(sha: &str) -> bool {
    sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit())
}
pub fn path_ok(path: &str) -> bool {
    !path.starts_with('/')
        && !path.contains('\\')
        && !path.contains(['?', '#', '%'])
        && path.split('/').all(|p| {
            !p.is_empty()
                && p != "."
                && p != ".."
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        })
}
fn repo_ok(repo: &str) -> bool {
    repo.split('/').count() == 2 && path_ok(repo)
}
fn github_repo(url: &str) -> Option<String> {
    let repo = url
        .strip_prefix("https://github.com/")?
        .trim_end_matches(".git");
    repo_ok(repo).then(|| repo.into())
}
impl Registry {
    pub fn github(repo: String) -> Result<Self, Error> {
        Self::new(
            "https://api.github.com".into(),
            "https://raw.githubusercontent.com".into(),
            repo,
        )
    }
    /// Separate roots permit a local recording server in integration tests. Deployment routes
    /// use `github`, never roots or source URLs from a request.
    pub fn new(api: String, raw: String, repo: String) -> Result<Self, Error> {
        if !repo_ok(&repo) {
            return Err(bad("registry must be a GitHub owner/repository"));
        }
        let client = reqwest::Client::builder()
            .user_agent("opengrok-plugin-registry")
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| bad("registry client unavailable"))?;
        Ok(Self {
            client,
            api,
            raw,
            repo,
        })
    }
    async fn bytes(&self, url: String) -> Result<Vec<u8>, Error> {
        let mut response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|_| bad("registry could not be reached"))?;
        if !response.status().is_success() {
            return Err(bad("registry file unavailable"));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| bad("registry read failed"))?
        {
            if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
                return Err(bad("registry file exceeds 2 MiB"));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
    async fn json(&self, url: String) -> Result<serde_json::Value, Error> {
        serde_json::from_slice(&self.bytes(url).await?).map_err(|_| bad("invalid registry JSON"))
    }
    async fn file(&self, repo: &str, sha: &str, path: &str) -> Result<String, Error> {
        String::from_utf8(
            self.bytes(format!("{}/{repo}/{sha}/{path}", self.raw))
                .await?,
        )
        .map_err(|_| bad("bundle file is not UTF-8"))
    }
    pub async fn catalog(&self, revision: Option<&str>) -> Result<Catalog, Error> {
        let sha = match revision {
            Some(sha) if revision_ok(sha) => sha.to_lowercase(),
            Some(_) => return Err(bad("revision must be a full 40-character commit SHA")),
            None => self
                .json(format!("{}/repos/{}/commits/HEAD", self.api, self.repo))
                .await?["sha"]
                .as_str()
                .filter(|s| revision_ok(s))
                .ok_or_else(|| bad("registry commit missing"))?
                .into(),
        };
        let text = self
            .file(&self.repo, &sha, ".grok-plugin/marketplace.json")
            .await?;
        let value: serde_json::Value =
            serde_json::from_str(&text).map_err(|_| bad("invalid marketplace JSON"))?;
        let plugins = value["plugins"]
            .as_array()
            .ok_or_else(|| bad("marketplace needs plugins"))?;
        if plugins.len() > 256 {
            return Err(bad("registry exceeds 256 entries"));
        }
        let mut entries = BTreeMap::new();
        for row in plugins {
            let name = row["name"]
                .as_str()
                .filter(|s| opengrok_plugins::is_valid_name(s) && !s.contains('.'))
                .ok_or_else(|| bad("invalid registry plugin name"))?
                .to_string();
            let source = &row["source"];
            let (repo, pinned, path, why) = if let Some(local) = source.as_str().or_else(|| {
                source["path"]
                    .as_str()
                    .filter(|_| source["type"] == "local")
            }) {
                let path = local.strip_prefix("./").unwrap_or(local);
                (self.repo.clone(), sha.clone(), path.into(), None)
            } else if let Some(repo) = source["url"].as_str().and_then(github_repo) {
                let pinned = source["sha"].as_str().unwrap_or_default().to_lowercase();
                let why =
                    (!revision_ok(&pinned)).then(|| "external source has no pinned commit".into());
                (
                    repo,
                    pinned,
                    source["path"].as_str().unwrap_or_default().into(),
                    why,
                )
            } else {
                (
                    String::new(),
                    String::new(),
                    String::new(),
                    Some("source is not a supported GitHub bundle".into()),
                )
            };
            if !path.is_empty() && !path_ok(&path) {
                return Err(bad("registry bundle path is unsafe"));
            }
            let entry = Entry {
                name: name.clone(),
                description: row["description"].as_str().unwrap_or_default().into(),
                repository: repo,
                revision: pinned,
                path,
                unavailable_reason: why,
            };
            if entries.insert(name, entry).is_some() {
                return Err(bad("duplicate plugin name in registry"));
            }
        }
        Ok(Catalog {
            registry: self.repo.clone(),
            revision: sha,
            plugins: entries.into_values().collect(),
        })
    }
    pub async fn bundle(&self, entry: &Entry) -> Result<Bundle, Error> {
        tokio::time::timeout(Duration::from_secs(60), self.fetch_bundle(entry))
            .await
            .map_err(|_| bad("bundle fetch exceeded 60 seconds"))?
    }
    async fn fetch_bundle(&self, entry: &Entry) -> Result<Bundle, Error> {
        if entry.unavailable_reason.is_some()
            || !repo_ok(&entry.repository)
            || !revision_ok(&entry.revision)
            || (!entry.path.is_empty() && !path_ok(&entry.path))
        {
            return Err(bad("this source cannot be installed"));
        }
        let tree = self
            .json(format!(
                "{}/repos/{}/git/trees/{}?recursive=1",
                self.api, entry.repository, entry.revision
            ))
            .await?;
        if tree["truncated"] == true {
            return Err(bad("repository tree is truncated"));
        }
        let rows = tree["tree"]
            .as_array()
            .ok_or_else(|| bad("repository tree missing"))?;
        let prefix = if entry.path.is_empty() {
            String::new()
        } else {
            format!("{}/", entry.path)
        };
        let mut files = BTreeMap::new();
        let mut total = 0;
        for row in rows {
            let Some(full) = row["path"].as_str() else {
                continue;
            };
            let Some(path) = full.strip_prefix(&prefix) else {
                continue;
            };
            let wanted = [
                "plugin.json",
                ".grok-plugin/plugin.json",
                ".claude-plugin/plugin.json",
                "mcp.json",
                ".mcp.json",
                ".lsp.json",
            ]
            .contains(&path)
                || ["skills/", "commands/", "agents/", "hooks/", "lsp/"]
                    .iter()
                    .any(|p| path.starts_with(p));
            if !wanted || row["type"] != "blob" {
                continue;
            }
            if !path_ok(path) || row["mode"] == "120000" {
                return Err(bad("bundle contains unsafe path or symlink"));
            }
            if files.len() >= 128 {
                return Err(bad("bundle exceeds 128 files"));
            }
            // Presence is enough for an unsupported component; do not fetch executable hooks.
            let text = if path.starts_with("skills/") || path.ends_with(".json") {
                self.file(&entry.repository, &entry.revision, full).await?
            } else {
                String::new()
            };
            total += text.len();
            if total > 2 * 1024 * 1024 {
                return Err(bad("bundle exceeds 2 MiB"));
            }
            files.insert(path.to_string(), text);
        }
        let bundle = Bundle::from_files(&files).map_err(Error)?;
        if bundle.manifest.name != entry.name {
            return Err(bad("bundle name disagrees with registry"));
        }
        Ok(bundle)
    }
}

#[cfg(test)]
#[path = "../tests/unit/registry.rs"]
mod tests;
