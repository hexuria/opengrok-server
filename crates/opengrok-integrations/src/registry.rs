//! GitHub registry adapter. Sources are read at immutable commits, with bounded bytes and time.
use futures::StreamExt;
use opengrok_plugins::bundle::{Bundle, Part};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct Registry {
    client: reqwest::Client,
    api: String,
    raw: String,
    repo: String,
    /// Sent to `api` only, never to `raw` or a source URL: anonymous reads share one 60-an-hour
    /// budget per server address, which one person browsing could spend for everyone.
    token: Option<String>,
    cache: Arc<Mutex<Cache>>,
    head_ttl: Duration,
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
/// Which side is at fault decides the status a route answers: a request it should not have sent,
/// a registry that did not answer, or a bundle this server will not take.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Request(String),
    #[error("{0}")]
    Upstream(String),
    #[error("{0}")]
    Refused(String),
}
/// A 404: an answer about the commit, which `on_branch` reads as "not on the branch".
const NOT_FOUND: &str = "registry file not found at that commit";
fn upstream(why: &str) -> Error {
    Error::Upstream(why.into())
}
fn refused(why: &str) -> Error {
    Error::Refused(why.into())
}

/// Commits never change, so what was read at one is kept; only "which commit is HEAD" expires.
/// Bounded by clearing when full: the cache is an optimisation, and a miss only costs a fetch.
#[derive(Default)]
struct Cache {
    head: Option<(Instant, String)>,
    reachable: BTreeMap<String, ()>,
    catalogs: BTreeMap<String, Catalog>,
    bundles: BTreeMap<(String, String, String, String), Bundle>,
}
const CACHED: usize = 64;
fn keep<K: Ord, V>(map: &mut BTreeMap<K, V>, key: K, value: V) {
    if map.len() >= CACHED {
        map.clear();
    }
    map.insert(key, value);
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
/// A source path as marketplaces write it (`./plugins/x`, `plugins/x/`, `.`), as the tree names
/// it; `None` when it is not a plain relative path. Local and external sources alike: only local
/// ones had `./` stripped, so `"./bundle"` on an external source failed the whole catalog.
fn bundle_path(raw: &str) -> Option<String> {
    let path = raw.strip_prefix("./").unwrap_or(raw).trim_end_matches('/');
    let path = if path == "." { "" } else { path };
    (path.is_empty() || path_ok(path)).then(|| path.into())
}
/// A skipped file stays visible as a part, so the person sees what the bundle carried and why it
/// was left behind, rather than the whole plugin failing over one PNG.
fn skipped(path: &str, why: &str) -> Part {
    Part {
        kind: "file".into(),
        name: path.chars().take(200).collect(),
        supported: false,
        reason: Some(why.into()),
    }
}
impl Registry {
    /// The deployment's registry: `OG_PLUGIN_REGISTRY` (an `owner/repository`, by default
    /// `hexuria/plugin-marketplace`) and, optionally, `OG_PLUGIN_REGISTRY_TOKEN` for GitHub's API.
    /// An unusable value is said once, at boot, by name: swallowed, it switched the whole feature
    /// off and every `/plugins` route answered 503 with nothing in the log to say why.
    pub fn from_env() -> Option<Self> {
        let repo = std::env::var("OG_PLUGIN_REGISTRY")
            .unwrap_or_else(|_| "hexuria/plugin-marketplace".into());
        let token = std::env::var("OG_PLUGIN_REGISTRY_TOKEN").ok();
        Self::github(repo.clone(), token)
            .inspect_err(|error| {
                tracing::error!(%error, registry = repo, "OG_PLUGIN_REGISTRY is unusable; /plugins answers 503");
            })
            .ok()
    }
    pub fn github(repo: String, token: Option<String>) -> Result<Self, Error> {
        let mut registry = Self::new(
            "https://api.github.com".into(),
            "https://raw.githubusercontent.com".into(),
            repo,
        )?;
        registry.token = token.filter(|t| !t.is_empty());
        Ok(registry)
    }
    /// Separate roots permit a local recording server in integration tests. Deployment routes
    /// use `github`, never roots or source URLs from a request.
    pub fn new(api: String, raw: String, repo: String) -> Result<Self, Error> {
        if !repo_ok(&repo) {
            return Err(Error::Request(
                "registry must be a GitHub owner/repository".into(),
            ));
        }
        let client = reqwest::Client::builder()
            .user_agent("opengrok-plugin-registry")
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| upstream("registry client unavailable"))?;
        Ok(Self {
            client,
            api,
            raw,
            repo,
            token: None,
            cache: Arc::default(),
            head_ttl: Duration::from_secs(300),
        })
    }
    /// How long "this commit is HEAD" is believed. Five minutes keeps browsing to a dozen API
    /// calls an hour; a test that moves HEAD under a running server sets zero.
    #[must_use]
    pub fn with_head_ttl(mut self, ttl: Duration) -> Self {
        self.head_ttl = ttl;
        self
    }
    fn cached<T>(&self, read: impl FnOnce(&mut Cache) -> T) -> Option<T> {
        // A poisoned lock is a cache that cannot be trusted, never a request that fails.
        self.cache.lock().ok().map(|mut cache| read(&mut cache))
    }
    async fn bytes(&self, url: String) -> Result<Vec<u8>, Error> {
        self.bytes_within(url, 2 * 1024 * 1024).await
    }
    async fn bytes_within(&self, url: String, cap: usize) -> Result<Vec<u8>, Error> {
        let mut request = self.client.get(&url);
        if let Some(token) = self.token.as_ref().filter(|_| url.starts_with(&self.api)) {
            request = request.bearer_auth(token);
        }
        let mut response = request
            .send()
            .await
            .map_err(|_| upstream("registry could not be reached"))?;
        // Not there is an answer about the commit; anything else is the registry not answering.
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(refused(NOT_FOUND));
        }
        if !response.status().is_success() {
            return Err(upstream("registry file unavailable"));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| upstream("registry read failed"))?
        {
            if bytes.len() + chunk.len() > cap {
                return Err(refused(&format!("registry file exceeds {} MiB", cap >> 20)));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
    async fn json(&self, url: String) -> Result<serde_json::Value, Error> {
        serde_json::from_slice(&self.bytes(url).await?)
            .map_err(|_| upstream("invalid registry JSON"))
    }
    async fn file(&self, repo: &str, sha: &str, path: &str) -> Result<Vec<u8>, Error> {
        self.bytes(format!("{}/{repo}/{sha}/{path}", self.raw))
            .await
    }
    async fn head(&self) -> Result<String, Error> {
        let fresh = self.cached(|c| c.head.clone()).flatten();
        if let Some((_, sha)) = fresh.filter(|(at, _)| at.elapsed() < self.head_ttl) {
            return Ok(sha);
        }
        self.head_now().await
    }
    async fn head_now(&self) -> Result<String, Error> {
        let sha: String = self
            .json(format!("{}/repos/{}/commits/HEAD", self.api, self.repo))
            .await?["sha"]
            .as_str()
            .filter(|s| revision_ok(s))
            .ok_or_else(|| upstream("registry commit missing"))?
            .to_lowercase();
        self.cached(|c| c.head = Some((Instant::now(), sha.clone())));
        Ok(sha)
    }
    /// A pinned registry commit must be one the registry's own default branch reached. Any 40 hex
    /// characters would otherwise do, including a commit from a fork, which GitHub serves under
    /// the parent's path, carrying a marketplace nobody here curated.
    ///
    /// A NEWER PIN THAN THE CACHED HEAD IS ASKED ABOUT AGAIN. Against a stale HEAD it compares as
    /// `behind`, and with two replicas the one that served the catalog had already moved on: a
    /// valid install was refused for up to the cache's five minutes. Only the fresh answer refuses.
    async fn reachable(&self, sha: &str) -> Result<(), Error> {
        if self.cached(|c| c.reachable.contains_key(sha)) == Some(true) {
            return Ok(());
        }
        let cached = self.head().await?;
        if sha == cached || self.on_branch(sha, &cached).await? {
            return self.remember_reachable(sha);
        }
        let head = self.head_now().await?;
        if head != cached && (sha == head || self.on_branch(sha, &head).await?) {
            return self.remember_reachable(sha);
        }
        Err(refused(
            "that revision is not on the registry's default branch",
        ))
    }
    fn remember_reachable(&self, sha: &str) -> Result<(), Error> {
        self.cached(|c| keep(&mut c.reachable, sha.to_string(), ()));
        Ok(())
    }
    /// GitHub's compare, asked for one commit: the answer also lists changed files, so a pin far
    /// behind HEAD can still be large, and is read up to 16 MiB rather than the 2 MiB a bundle file
    /// gets. Only a 404, a commit GitHub cannot place, means "not ours"; a reply too large to read
    /// or a registry that did not answer says so instead of calling the pin a fork's.
    async fn on_branch(&self, sha: &str, head: &str) -> Result<bool, Error> {
        let url = format!(
            "{}/repos/{}/compare/{sha}...{head}?per_page=1",
            self.api, self.repo
        );
        let bytes = match self.bytes_within(url, 16 * 1024 * 1024).await {
            Ok(bytes) => bytes,
            Err(Error::Refused(why)) if why == NOT_FOUND => return Ok(false),
            Err(error) => return Err(error),
        };
        let compared: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| upstream("invalid registry JSON"))?;
        let status = compared["status"].as_str().unwrap_or_default();
        Ok(["ahead", "identical"].contains(&status))
    }
    pub async fn catalog(&self, revision: Option<&str>) -> Result<Catalog, Error> {
        let sha = match revision {
            Some(sha) if revision_ok(sha) => sha.to_lowercase(),
            Some(_) => {
                return Err(Error::Request(
                    "revision must be a full 40-character commit SHA".into(),
                ));
            }
            None => self.head().await?,
        };
        // Only a reachable commit's catalog is ever kept, so a hit needs no second look.
        if let Some(catalog) = self.cached(|c| c.catalogs.get(&sha).cloned()).flatten() {
            return Ok(catalog);
        }
        self.reachable(&sha).await?;
        let bytes = self
            .file(&self.repo, &sha, ".grok-plugin/marketplace.json")
            .await?;
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| upstream("invalid marketplace JSON"))?;
        let plugins = value["plugins"]
            .as_array()
            .ok_or_else(|| upstream("marketplace needs plugins"))?;
        if plugins.len() > 256 {
            return Err(refused("registry exceeds 256 entries"));
        }
        let mut entries: BTreeMap<String, Entry> = BTreeMap::new();
        // ONE BAD ROW IS ONE UNAVAILABLE ENTRY, never a catalog that fails for every person. A
        // row without a name at all cannot be shown, so only it is left out.
        for row in plugins {
            // The whole name is judged: cut first, a 70-character name read as a valid 64, and two
            // names sharing their first 64 characters collided into one.
            let Some(name) = row["name"].as_str().map(str::to_string) else {
                continue;
            };
            let source = &row["source"];
            let (repo, pinned, path, mut why) = if let Some(local) = source.as_str().or_else(|| {
                source["path"]
                    .as_str()
                    .filter(|_| source["type"] == "local")
            }) {
                (self.repo.clone(), sha.clone(), bundle_path(local), None)
            } else if let Some(repo) = source["url"].as_str().and_then(github_repo) {
                let pinned = source["sha"].as_str().unwrap_or_default().to_lowercase();
                let why =
                    (!revision_ok(&pinned)).then(|| "external source has no pinned commit".into());
                let path = bundle_path(source["path"].as_str().unwrap_or_default());
                (repo, pinned, path, why)
            } else {
                let why = Some("source is not a supported GitHub bundle".into());
                (String::new(), String::new(), Some(String::new()), why)
            };
            if !opengrok_plugins::is_valid_name(&name) || name.contains('.') {
                why = Some("plugin name is not a valid tool prefix".into());
            } else if path.is_none() {
                why = why.or(Some("registry bundle path is unsafe".into()));
            }
            let entry = Entry {
                name: name.clone(),
                description: row["description"].as_str().unwrap_or_default().into(),
                repository: repo,
                revision: pinned,
                path: path.unwrap_or_default(),
                unavailable_reason: why,
            };
            match entries.get_mut(&name) {
                // Neither twin is installable: which one a name meant cannot be told.
                Some(first) => {
                    first.unavailable_reason = Some("two registry entries share this name".into())
                }
                None => {
                    entries.insert(name, entry);
                }
            }
        }
        let catalog = Catalog {
            registry: self.repo.clone(),
            revision: sha.clone(),
            plugins: entries.into_values().collect(),
        };
        self.cached(|c| keep(&mut c.catalogs, sha, catalog.clone()));
        Ok(catalog)
    }
    /// What an install may take: the entry at the pinned commit, provided the registry still lists
    /// it as installable now. A plugin the maintainers pulled is not installable at an old pin.
    /// `None` when the pinned catalog has no such plugin.
    pub async fn installable(
        &self,
        revision: &str,
        name: &str,
    ) -> Result<Option<(Catalog, Entry)>, Error> {
        let pinned = self.catalog(Some(revision)).await?;
        let Some(entry) = pinned.plugins.iter().find(|e| e.name == name).cloned() else {
            return Ok(None);
        };
        let current = self.catalog(None).await?;
        let listed = current.plugins.iter().find(|e| e.name == name);
        if !listed.is_some_and(|e| e.unavailable_reason.is_none()) {
            return Err(refused("the registry no longer lists this plugin"));
        }
        Ok(Some((pinned, entry)))
    }
    pub async fn bundle(&self, entry: &Entry) -> Result<Bundle, Error> {
        // BEFORE THE CACHE. The same source can be fine at one registry commit and unavailable at
        // another (a duplicate name, a pulled entry); a cached bundle must not answer for it.
        if let Some(why) = &entry.unavailable_reason {
            return Err(Error::Refused(why.clone()));
        }
        let key = (
            entry.repository.clone(),
            entry.revision.clone(),
            entry.path.clone(),
            entry.name.clone(),
        );
        if let Some(bundle) = self.cached(|c| c.bundles.get(&key).cloned()).flatten() {
            return Ok(bundle);
        }
        let bundle = tokio::time::timeout(Duration::from_secs(60), self.fetch_bundle(entry))
            .await
            .map_err(|_| upstream("bundle fetch exceeded 60 seconds"))??;
        self.cached(|c| keep(&mut c.bundles, key, bundle.clone()));
        Ok(bundle)
    }
    async fn fetch_bundle(&self, entry: &Entry) -> Result<Bundle, Error> {
        if !repo_ok(&entry.repository)
            || !revision_ok(&entry.revision)
            || (!entry.path.is_empty() && !path_ok(&entry.path))
        {
            return Err(refused("this source cannot be installed"));
        }
        let tree = self
            .json(format!(
                "{}/repos/{}/git/trees/{}?recursive=1",
                self.api, entry.repository, entry.revision
            ))
            .await?;
        if tree["truncated"] == true {
            return Err(refused("repository tree is truncated"));
        }
        let rows = tree["tree"]
            .as_array()
            .ok_or_else(|| upstream("repository tree missing"))?;
        let prefix = if entry.path.is_empty() {
            String::new()
        } else {
            format!("{}/", entry.path)
        };
        let mut wanted = Vec::new();
        let mut parts = Vec::new();
        for row in rows {
            let Some(path) = row["path"].as_str().and_then(|p| p.strip_prefix(&prefix)) else {
                continue;
            };
            let named = [
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
            if !named || row["type"] != "blob" {
                continue;
            }
            if wanted.len() + parts.len() >= 128 {
                return Err(refused("bundle exceeds 128 files"));
            }
            if row["mode"] == "120000" {
                parts.push(skipped(path, "symlinks are not followed"));
            } else if !path_ok(path) {
                parts.push(skipped(path, "file name is not a plain path"));
            } else {
                wanted.push(path.to_string());
            }
        }
        // Presence is enough for an unsupported component; do not fetch executable hooks. Eight at
        // a time: one by one, a skills-heavy bundle on a slow link ran out of its 60 seconds.
        let prefix = &prefix;
        let fetched = futures::stream::iter(wanted.into_iter().map(|path| async move {
            let fetch = path.starts_with("skills/") || path.ends_with(".json");
            let bytes = if fetch {
                let full = format!("{prefix}{path}");
                self.file(&entry.repository, &entry.revision, &full).await?
            } else {
                Vec::new()
            };
            Ok::<_, Error>((path, bytes))
        }))
        .buffer_unordered(8);
        futures::pin_mut!(fetched);
        let mut files = BTreeMap::new();
        let mut total = 0;
        while let Some(fetched) = fetched.next().await {
            let (path, bytes) = fetched?;
            total += bytes.len();
            if total > 2 * 1024 * 1024 {
                return Err(refused("bundle exceeds 2 MiB"));
            }
            match String::from_utf8(bytes) {
                Ok(text) => {
                    files.insert(path, text);
                }
                Err(_) => parts.push(skipped(&path, "not a UTF-8 text file")),
            }
        }
        let mut bundle = Bundle::from_files(&files).map_err(Error::Refused)?;
        if bundle.manifest.name != entry.name {
            return Err(refused("bundle name disagrees with registry"));
        }
        parts.sort_by(|a, b| a.name.cmp(&b.name));
        bundle.parts.extend(parts);
        Ok(bundle)
    }
}

#[cfg(test)]
#[path = "../tests/unit/registry.rs"]
mod tests;
