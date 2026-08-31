//! The organization aggregate — the tenant boundary, its domains, and its invites.
//!
//! WHY ORGS EXIST: signup is not open. A person may create an account only with an invite code
//! issued by an org's admin AND an email under one of that org's registered domains
//! (`docs/identity-model.md`). Both gates, not either — a code alone would let a stranger's gmail
//! in, a domain alone would let anyone at the company in unbidden. The org is where both live.
//!
//! DOMAIN MATCHING IS HERE; DOMAIN OWNERSHIP IS NOT. This aggregate records the domains an org
//! claims and checks an email against them. Whether the org actually *controls* a domain (a DNS
//! proof) is a later slice — v1 takes the admin's word at `org create`, because the admin has
//! shell on the server and is not adversarial to their own deployment.
//!
//! INVITES LIVE ON THE ORG, not in their own aggregate: an invite has no life apart from the org
//! that issued it, and "which invites are outstanding" is exactly the org's concern.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::id::{AccountId, OrgId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum OrgEvent {
    Created {
        name: String,
        admin: AccountId,
        /// Lower-cased at the edge; compared lower-cased. `Acme.com` and `acme.com` are one domain.
        domains: Vec<String>,
        at_ms: i64,
    },
    DomainAdded {
        domain: String,
        at_ms: i64,
    },
    /// An admin minted an invite code. The code's secret is the code itself; only its presence and
    /// state live here.
    InviteIssued {
        code: String,
        at_ms: i64,
    },
    /// Somebody signed up with the code. Single-use: a redeemed code is spent.
    InviteRedeemed {
        code: String,
        account: AccountId,
        at_ms: i64,
    },
    InviteRevoked {
        code: String,
        at_ms: i64,
    },
    /// A verified, enabled account joined the org (via signup or admin mint).
    MemberJoined {
        account: AccountId,
        at_ms: i64,
    },
}

impl OrgEvent {
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::Created { .. } => "org-created",
            Self::DomainAdded { .. } => "org-domain-added",
            Self::InviteIssued { .. } => "org-invite-issued",
            Self::InviteRedeemed { .. } => "org-invite-redeemed",
            Self::InviteRevoked { .. } => "org-invite-revoked",
            Self::MemberJoined { .. } => "org-member-joined",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InviteState {
    Open,
    Redeemed(AccountId),
    Revoked,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Org {
    pub created: bool,
    pub name: String,
    pub admin: Option<AccountId>,
    pub domains: Vec<String>,
    pub invites: BTreeMap<String, InviteState>,
    pub members: Vec<AccountId>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OrgError {
    #[error("no such organization")]
    NotCreated,
    #[error("that organization already exists")]
    AlreadyCreated,
    #[error("an organization needs a name")]
    EmptyName,
    #[error("an organization needs at least one domain")]
    NoDomains,
    #[error("that invite code is already in use")]
    DuplicateInvite,
    #[error("no such invite code")]
    UnknownInvite,
    #[error("that invite code has already been used")]
    InviteSpent,
    #[error("that invite code was revoked")]
    InviteRevoked,
    #[error("this email's domain is not one this organization has registered")]
    DomainNotAllowed,
}

#[derive(Debug, Clone)]
pub enum OrgCommand {
    Create {
        name: String,
        admin: AccountId,
        domains: Vec<String>,
        at_ms: i64,
    },
    AddDomain {
        domain: String,
        at_ms: i64,
    },
    IssueInvite {
        code: String,
        at_ms: i64,
    },
    RedeemInvite {
        code: String,
        email_domain: String,
        account: AccountId,
        at_ms: i64,
    },
    RevokeInvite {
        code: String,
        at_ms: i64,
    },
}

/// Normalize a domain for storage and comparison: lower-case, trimmed, no leading `@`.
pub fn normalize_domain(raw: &str) -> String {
    raw.trim().trim_start_matches('@').to_lowercase()
}

/// The domain part of an email, normalized — or `None` if it has no `@`.
pub fn email_domain(email: &str) -> Option<String> {
    email
        .rsplit_once('@')
        .map(|(_, domain)| normalize_domain(domain))
}

impl Org {
    pub fn replay<'a>(events: impl IntoIterator<Item = &'a OrgEvent>) -> Self {
        let mut state = Self::default();
        for event in events {
            state.apply(event);
        }
        state
    }

    pub fn apply(&mut self, event: &OrgEvent) {
        match event {
            OrgEvent::Created {
                name,
                admin,
                domains,
                ..
            } => {
                self.created = true;
                self.name = name.clone();
                self.admin = Some(admin.clone());
                self.domains = domains.iter().map(|d| normalize_domain(d)).collect();
            }
            OrgEvent::DomainAdded { domain, .. } => {
                let domain = normalize_domain(domain);
                if !self.domains.contains(&domain) {
                    self.domains.push(domain);
                }
            }
            OrgEvent::InviteIssued { code, .. } => {
                self.invites.insert(code.clone(), InviteState::Open);
            }
            OrgEvent::InviteRedeemed { code, account, .. } => {
                self.invites
                    .insert(code.clone(), InviteState::Redeemed(account.clone()));
            }
            OrgEvent::InviteRevoked { code, .. } => {
                self.invites.insert(code.clone(), InviteState::Revoked);
            }
            OrgEvent::MemberJoined { account, .. } => {
                if !self.members.contains(account) {
                    self.members.push(account.clone());
                }
            }
        }
    }

    fn alive(&self) -> Result<(), OrgError> {
        if self.created {
            Ok(())
        } else {
            Err(OrgError::NotCreated)
        }
    }

    /// Does this email's domain belong to the org? The signup gate's domain half.
    pub fn allows_email(&self, email: &str) -> bool {
        email_domain(email)
            .map(|domain| self.domains.contains(&domain))
            .unwrap_or(false)
    }

    pub fn decide(&self, command: OrgCommand) -> Result<Vec<OrgEvent>, OrgError> {
        match command {
            OrgCommand::Create {
                name,
                admin,
                domains,
                at_ms,
            } => {
                if self.created {
                    return Err(OrgError::AlreadyCreated);
                }
                if name.trim().is_empty() {
                    return Err(OrgError::EmptyName);
                }
                let domains: Vec<String> = domains
                    .iter()
                    .map(|d| normalize_domain(d))
                    .filter(|d| !d.is_empty())
                    .collect();
                if domains.is_empty() {
                    return Err(OrgError::NoDomains);
                }
                Ok(vec![OrgEvent::Created {
                    name,
                    admin,
                    domains,
                    at_ms,
                }])
            }
            OrgCommand::AddDomain { domain, at_ms } => {
                self.alive()?;
                Ok(vec![OrgEvent::DomainAdded { domain, at_ms }])
            }
            OrgCommand::IssueInvite { code, at_ms } => {
                self.alive()?;
                if self.invites.contains_key(&code) {
                    return Err(OrgError::DuplicateInvite);
                }
                Ok(vec![OrgEvent::InviteIssued { code, at_ms }])
            }
            OrgCommand::RedeemInvite {
                code,
                email_domain,
                account,
                at_ms,
            } => {
                self.alive()?;
                // BOTH GATES, checked here where both facts live. The code must be open, and the
                // email's domain must be one the org registered — either failing is a distinct,
                // client-readable refusal, not a generic "no".
                match self.invites.get(&code) {
                    None => return Err(OrgError::UnknownInvite),
                    Some(InviteState::Redeemed(_)) => return Err(OrgError::InviteSpent),
                    Some(InviteState::Revoked) => return Err(OrgError::InviteRevoked),
                    Some(InviteState::Open) => {}
                }
                if !self.domains.contains(&normalize_domain(&email_domain)) {
                    return Err(OrgError::DomainNotAllowed);
                }
                Ok(vec![
                    OrgEvent::InviteRedeemed {
                        code,
                        account: account.clone(),
                        at_ms,
                    },
                    OrgEvent::MemberJoined { account, at_ms },
                ])
            }
            OrgCommand::RevokeInvite { code, at_ms } => {
                self.alive()?;
                match self.invites.get(&code) {
                    None => Err(OrgError::UnknownInvite),
                    Some(InviteState::Redeemed(_)) => Err(OrgError::InviteSpent),
                    _ => Ok(vec![OrgEvent::InviteRevoked { code, at_ms }]),
                }
            }
        }
    }
}

/// One row of `org_view`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrgView {
    pub id: OrgId,
    pub name: String,
    pub admin: AccountId,
    pub domains: Vec<String>,
    pub updated_at_ms: i64,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)]

    use super::*;

    fn org() -> Org {
        Org::replay(&[OrgEvent::Created {
            name: "Acme".to_string(),
            admin: AccountId::from_stored("acct_admin"),
            domains: vec!["acme.com".to_string(), "Acme.io".to_string()],
            at_ms: 1,
        }])
    }

    #[test]
    fn domains_are_normalized_and_matched_case_insensitively() {
        let org = org();
        assert!(org.allows_email("jo@acme.com"));
        assert!(org.allows_email("jo@ACME.COM"));
        assert!(org.allows_email("jo@acme.io"));
        assert!(!org.allows_email("jo@gmail.com"));
        assert!(!org.allows_email("no-at-sign"));
    }

    #[test]
    fn redeem_requires_both_an_open_code_and_a_matching_domain() {
        let mut org = org();
        for event in org
            .decide(OrgCommand::IssueInvite {
                code: "WELCOME".to_string(),
                at_ms: 2,
            })
            .expect("issue")
        {
            org.apply(&event);
        }

        // Wrong domain, right code.
        assert!(matches!(
            org.decide(OrgCommand::RedeemInvite {
                code: "WELCOME".to_string(),
                email_domain: "gmail.com".to_string(),
                account: AccountId::from_stored("acct_1"),
                at_ms: 3,
            }),
            Err(OrgError::DomainNotAllowed)
        ));

        // Right domain, unknown code.
        assert!(matches!(
            org.decide(OrgCommand::RedeemInvite {
                code: "NOPE".to_string(),
                email_domain: "acme.com".to_string(),
                account: AccountId::from_stored("acct_1"),
                at_ms: 3,
            }),
            Err(OrgError::UnknownInvite)
        ));

        // Both right: redeems, and the account joins.
        let events = org
            .decide(OrgCommand::RedeemInvite {
                code: "WELCOME".to_string(),
                email_domain: "acme.com".to_string(),
                account: AccountId::from_stored("acct_1"),
                at_ms: 3,
            })
            .expect("redeem");
        for event in &events {
            org.apply(event);
        }
        assert!(org.members.contains(&AccountId::from_stored("acct_1")));

        // Single-use: a second redeem of the same code is spent.
        assert!(matches!(
            org.decide(OrgCommand::RedeemInvite {
                code: "WELCOME".to_string(),
                email_domain: "acme.com".to_string(),
                account: AccountId::from_stored("acct_2"),
                at_ms: 4,
            }),
            Err(OrgError::InviteSpent)
        ));
    }
}
