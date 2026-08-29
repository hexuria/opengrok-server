//! What we vouch for, and what we merely allow.
//!
//! WE DO NOT WRITE PLUGINS. Gmail, GitHub, Drive, mem0 — these exist, maintained by people who are
//! not us, and reimplementing them would be worse work done twice. So the job is not authorship, it
//! is **curation**: a short list we have actually looked at, and an honest posture toward everything
//! else.
//!
//! TRUST IS NOT A BADGE, IT CHANGES WHAT HAPPENS. A label that only appears in a UI is a label
//! people stop reading. So `Unverified` does something: its tools arrive needing a human yes, every
//! time, using the approval machinery that already exists. The person who brought the plugin keeps
//! their plugin; what they give up is the part where nobody has to look.
//!
//! AND SAYING NO IS ALSO A POSITION. A deployment that wants only the vetted list sets
//! `Policy::CuratedOnly` and anything else is refused outright — which is the right default for a
//! shared instance, and the wrong one for somebody's laptop. Both are supported because both are
//! real; what is not supported is quietly pretending an unreviewed plugin is reviewed.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Where a plugin came from, and how much that is worth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Trust {
    /// On our list. Somebody read it, and we stand behind the version pinned here.
    Verified,
    /// Brought by whoever installed it. It works; nobody vetted it; its tools ask first.
    Unverified,
}

impl Trust {
    /// Whether this plugin's tools need a person to say yes each time.
    ///
    /// The whole point of the distinction. An unverified plugin can do everything a verified one
    /// can — after somebody looks at the actual command.
    pub fn requires_approval(self) -> bool {
        matches!(self, Self::Unverified)
    }
}

/// What a deployment allows to be installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Policy {
    /// Only what we vouch for. The right default for an instance several people share.
    #[default]
    CuratedOnly,
    /// Anything, with unvetted plugins gated behind approval. The right setting for a laptop.
    AllowOthers,
}

/// One entry on the list we vouch for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    /// Where it comes from — a registry id, a git URL, a path.
    pub source: String,
    /// PINNED, NOT FLOATING. "Verified" is a statement about a specific version; a range would
    /// mean vouching for code nobody has read yet.
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// The list, and the rule for everything not on it.
#[derive(Debug, Clone, Default)]
pub struct Catalogue {
    entries: BTreeMap<String, Entry>,
    policy: Policy,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InstallError {
    #[error("{0} is not on this deployment's list, and this deployment installs only what is")]
    NotCurated(String),
    #[error("{name} is on the list at {expected}, not {found}")]
    WrongVersion {
        name: String,
        expected: String,
        found: String,
    },
}

/// The outcome of asking to install something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admission {
    pub name: String,
    pub trust: Trust,
    /// Said plainly, for a person to read before they agree to it.
    pub note: String,
}

impl Catalogue {
    pub fn new(policy: Policy) -> Self {
        Self {
            entries: BTreeMap::new(),
            policy,
        }
    }

    pub fn with(mut self, entry: Entry) -> Self {
        self.entries.insert(entry.name.clone(), entry);
        self
    }

    pub fn policy(&self) -> Policy {
        self.policy
    }

    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.values()
    }

    pub fn get(&self, name: &str) -> Option<&Entry> {
        self.entries.get(name)
    }

    /// May this be installed, and on what terms?
    ///
    /// The version is checked, not just the name. A curated name at an uncurated version is the
    /// most dangerous shape available — it looks vouched-for and is not — so it is refused under
    /// `CuratedOnly` and demoted to `Unverified` otherwise, rather than passing as verified.
    pub fn admit(&self, name: &str, version: &str) -> Result<Admission, InstallError> {
        match self.entries.get(name) {
            Some(entry) if entry.version == version => Ok(Admission {
                name: name.to_string(),
                trust: Trust::Verified,
                note: format!("{name} {version} is on this deployment's reviewed list"),
            }),

            Some(entry) => match self.policy {
                Policy::CuratedOnly => Err(InstallError::WrongVersion {
                    name: name.to_string(),
                    expected: entry.version.clone(),
                    found: version.to_string(),
                }),
                Policy::AllowOthers => Ok(Admission {
                    name: name.to_string(),
                    trust: Trust::Unverified,
                    note: format!(
                        "{name} is reviewed at {}, but you are installing {version}; \
                         its tools will ask before each use",
                        entry.version
                    ),
                }),
            },

            None => match self.policy {
                Policy::CuratedOnly => Err(InstallError::NotCurated(name.to_string())),
                Policy::AllowOthers => Ok(Admission {
                    name: name.to_string(),
                    trust: Trust::Unverified,
                    note: format!(
                        "{name} has not been reviewed by anyone here; \
                         its tools will ask before each use"
                    ),
                }),
            },
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn entry(name: &str, version: &str) -> Entry {
        Entry {
            name: name.to_string(),
            source: format!("registry:{name}"),
            version: version.to_string(),
            summary: None,
        }
    }

    fn curated(policy: Policy) -> Catalogue {
        Catalogue::new(policy)
            .with(entry("gmail", "1.2.0"))
            .with(entry("github", "3.0.1"))
    }

    #[test]
    fn something_on_the_list_at_the_right_version_is_verified() {
        let admitted = curated(Policy::CuratedOnly)
            .admit("gmail", "1.2.0")
            .unwrap();
        assert_eq!(admitted.trust, Trust::Verified);
        assert!(!admitted.trust.requires_approval());
    }

    /// The point of the whole design: an unreviewed plugin still works, and asks first.
    #[test]
    fn something_off_the_list_is_allowed_but_asks_first() {
        let admitted = curated(Policy::AllowOthers)
            .admit("somebodys-plugin", "0.1.0")
            .unwrap();
        assert_eq!(admitted.trust, Trust::Unverified);
        assert!(admitted.trust.requires_approval());
        // And the person is told what they are agreeing to, in words.
        assert!(
            admitted.note.contains("not been reviewed"),
            "{}",
            admitted.note
        );
        assert!(
            admitted.note.contains("ask before each use"),
            "{}",
            admitted.note
        );
    }

    /// A shared instance can simply say no.
    #[test]
    fn a_curated_only_deployment_refuses_the_rest() {
        assert_eq!(
            curated(Policy::CuratedOnly).admit("somebodys-plugin", "0.1.0"),
            Err(InstallError::NotCurated("somebodys-plugin".to_string()))
        );
    }

    /// THE DANGEROUS SHAPE: a name we vouch for, at a version nobody read. It looks trustworthy and
    /// is not, so it never passes as verified.
    #[test]
    fn a_curated_name_at_an_unreviewed_version_is_not_verified() {
        let strict = curated(Policy::CuratedOnly).admit("gmail", "9.9.9");
        match strict {
            Err(InstallError::WrongVersion { expected, .. }) => assert_eq!(expected, "1.2.0"),
            other => panic!("expected a version refusal, got {other:?}"),
        }

        let lenient = curated(Policy::AllowOthers)
            .admit("gmail", "9.9.9")
            .unwrap();
        assert_eq!(
            lenient.trust,
            Trust::Unverified,
            "a familiar name must not lend its reputation to an unread version"
        );
        assert!(lenient.note.contains("1.2.0"), "{}", lenient.note);
    }

    /// The safe posture is what you get by default.
    #[test]
    fn the_default_policy_is_the_careful_one() {
        assert_eq!(Policy::default(), Policy::CuratedOnly);
    }

    #[test]
    fn an_empty_curated_deployment_admits_nothing() {
        let empty = Catalogue::new(Policy::CuratedOnly);
        assert!(empty.admit("anything", "1.0.0").is_err());
    }
}
