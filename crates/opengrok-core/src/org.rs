//! The organization aggregate — the tenant boundary, its domains, and its invites.
//!
//! WHY ORGS EXIST: signup is not open. A person may create an account only with an invite code
//! issued by an org's admin AND an email under one of that org's registered domains
//! (`docs/identity-model.md`). Both gates, not either — a code alone would let a stranger's gmail
//! in, a domain alone would let anyone at the company in unbidden. The org is where both live.
//!
//! TWO WAYS A DOMAIN GETS IN, ONE MEANING ONCE IT IS. `domains` is the list that admits signups.
//! The operator's shell puts a domain there directly (`Created`, `DomainAdded`): whoever can run
//! `opengrok admin` on the box already owns the deployment, so their word is the proof. An org
//! admin at the web console has no shell and is not the operator — they may CLAIM a domain
//! (`DomainClaimed`), which gates nothing, and it reaches `domains` only when a DNS TXT record
//! carrying the claim's token resolves (`DomainVerified`). Without that split a console admin
//! could claim `gmail.com` and invite the world. The lookup itself is I/O and lives in the server;
//! this aggregate only knows whether a claim is outstanding and what token would settle it.
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
    /// The operator vouched for a domain from the shell. Admits signups at once.
    DomainAdded {
        domain: String,
        at_ms: i64,
    },
    /// An org admin claimed a domain over HTTP. Admits nothing until `DomainVerified`: the token
    /// must first appear in a TXT record under the domain (`challenge_record_name`).
    DomainClaimed {
        domain: String,
        token: String,
        at_ms: i64,
    },
    /// The TXT challenge resolved. From here the domain admits signups like a vouched one.
    DomainVerified {
        domain: String,
        at_ms: i64,
    },
    /// A pending claim withdrawn before it verified — a typo, or a domain the org gave up on.
    DomainClaimWithdrawn {
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
            Self::DomainClaimed { .. } => "org-domain-claimed",
            Self::DomainVerified { .. } => "org-domain-verified",
            Self::DomainClaimWithdrawn { .. } => "org-domain-claim-withdrawn",
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
    /// The domains that admit signups — vouched by the operator or proven by DNS. Nothing else.
    pub domains: Vec<String>,
    /// Claims awaiting their TXT record: domain → the token that must appear. A domain is never
    /// in both lists; verifying moves it, withdrawing drops it.
    pub pending_domains: BTreeMap<String, String>,
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
    #[error("that is not a domain name")]
    InvalidDomain,
    #[error("that domain is already verified for this organization")]
    DomainAlreadyVerified,
    #[error("that domain is already claimed and waiting for its DNS record")]
    DomainAlreadyClaimed,
    #[error("that domain has not been claimed")]
    DomainNotClaimed,
}

#[derive(Debug, Clone)]
pub enum OrgCommand {
    Create {
        name: String,
        admin: AccountId,
        domains: Vec<String>,
        at_ms: i64,
    },
    /// The operator's shell vouches for a domain. No proof step.
    AddDomain {
        domain: String,
        at_ms: i64,
    },
    /// A console admin claims a domain. `token` is minted by the caller (the aggregate has no
    /// randomness) and becomes the TXT challenge.
    ClaimDomain {
        domain: String,
        token: String,
        at_ms: i64,
    },
    /// The caller has SEEN the TXT record resolve (`challenge_satisfied`) and now records it.
    /// The aggregate does not re-check — it cannot — so nothing but the DNS path may send this.
    VerifyDomain {
        domain: String,
        at_ms: i64,
    },
    WithdrawDomainClaim {
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

/// Is this (already normalized) string shaped like a registrable domain? Two or more labels of
/// letters, digits and inner hyphens. Deliberately strict: a claim on something that is not a
/// domain could never verify and would sit pending forever, so refuse it up front.
pub fn is_domain_name(domain: &str) -> bool {
    if domain.is_empty() || domain.len() > 253 || !domain.contains('.') {
        return false;
    }
    domain.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    })
}

/// Where the challenge is published: a TXT record on `_opengrok-verify.<domain>`. Our own
/// contract, modelled on how every hosted product proves domain ownership; the underscore label
/// keeps it out of the way of real hosts.
pub fn challenge_record_name(domain: &str) -> String {
    format!("_opengrok-verify.{domain}")
}

/// What the TXT record must say, verbatim.
pub fn challenge_record_value(token: &str) -> String {
    format!("opengrok-verify={token}")
}

/// Does any resolved TXT value carry the token? Exact match after trimming — a record that says
/// something adjacent is not proof.
pub fn challenge_satisfied(token: &str, txt_values: &[String]) -> bool {
    let expected = challenge_record_value(token);
    txt_values
        .iter()
        .any(|value| value.trim().trim_matches('"') == expected)
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
            OrgEvent::DomainAdded { domain, .. } | OrgEvent::DomainVerified { domain, .. } => {
                let domain = normalize_domain(domain);
                self.pending_domains.remove(&domain);
                if !self.domains.contains(&domain) {
                    self.domains.push(domain);
                }
            }
            OrgEvent::DomainClaimed { domain, token, .. } => {
                self.pending_domains
                    .insert(normalize_domain(domain), token.clone());
            }
            OrgEvent::DomainClaimWithdrawn { domain, .. } => {
                self.pending_domains.remove(&normalize_domain(domain));
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
                // The shell vouches for these, but a typo is still a typo: refuse it loudly at
                // bootstrap rather than store a domain no email can ever match.
                if domains.iter().any(|d| !is_domain_name(d)) {
                    return Err(OrgError::InvalidDomain);
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
                let domain = normalize_domain(&domain);
                if !is_domain_name(&domain) {
                    return Err(OrgError::InvalidDomain);
                }
                Ok(vec![OrgEvent::DomainAdded { domain, at_ms }])
            }
            OrgCommand::ClaimDomain {
                domain,
                token,
                at_ms,
            } => {
                self.alive()?;
                let domain = normalize_domain(&domain);
                if !is_domain_name(&domain) {
                    return Err(OrgError::InvalidDomain);
                }
                if self.domains.contains(&domain) {
                    return Err(OrgError::DomainAlreadyVerified);
                }
                if self.pending_domains.contains_key(&domain) {
                    return Err(OrgError::DomainAlreadyClaimed);
                }
                Ok(vec![OrgEvent::DomainClaimed {
                    domain,
                    token,
                    at_ms,
                }])
            }
            OrgCommand::VerifyDomain { domain, at_ms } => {
                self.alive()?;
                let domain = normalize_domain(&domain);
                if self.domains.contains(&domain) {
                    return Err(OrgError::DomainAlreadyVerified);
                }
                if !self.pending_domains.contains_key(&domain) {
                    return Err(OrgError::DomainNotClaimed);
                }
                Ok(vec![OrgEvent::DomainVerified { domain, at_ms }])
            }
            OrgCommand::WithdrawDomainClaim { domain, at_ms } => {
                self.alive()?;
                let domain = normalize_domain(&domain);
                if !self.pending_domains.contains_key(&domain) {
                    return Err(OrgError::DomainNotClaimed);
                }
                Ok(vec![OrgEvent::DomainClaimWithdrawn { domain, at_ms }])
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
    /// Verified (or operator-vouched) domains only — the ones that admit signups.
    pub domains: Vec<String>,
    /// Claims still waiting for their TXT record: domain → token.
    #[serde(default)]
    pub pending_domains: BTreeMap<String, String>,
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

    fn apply_all(org: &mut Org, events: Vec<OrgEvent>) {
        for event in &events {
            org.apply(event);
        }
    }

    #[test]
    fn a_claimed_domain_admits_nobody_until_dns_proves_it() {
        let mut org = org();
        let events = org
            .decide(OrgCommand::ClaimDomain {
                domain: "Acme.dev".to_string(),
                token: "dv_abc".to_string(),
                at_ms: 2,
            })
            .expect("claim");
        apply_all(&mut org, events);
        assert_eq!(
            org.pending_domains.get("acme.dev").map(String::as_str),
            Some("dv_abc")
        );
        assert!(!org.allows_email("jo@acme.dev"), "a claim is not a proof");

        // Claiming again while pending, or claiming a domain already in, is refused distinctly.
        assert!(matches!(
            org.decide(OrgCommand::ClaimDomain {
                domain: "acme.dev".to_string(),
                token: "dv_other".to_string(),
                at_ms: 3,
            }),
            Err(OrgError::DomainAlreadyClaimed)
        ));
        assert!(matches!(
            org.decide(OrgCommand::ClaimDomain {
                domain: "acme.com".to_string(),
                token: "dv_other".to_string(),
                at_ms: 3,
            }),
            Err(OrgError::DomainAlreadyVerified)
        ));

        let events = org
            .decide(OrgCommand::VerifyDomain {
                domain: "acme.dev".to_string(),
                at_ms: 4,
            })
            .expect("verify");
        apply_all(&mut org, events);
        assert!(org.pending_domains.is_empty());
        assert!(org.allows_email("jo@acme.dev"));

        // Verifying what is not pending is refused — the DNS path is the only way in.
        assert!(matches!(
            org.decide(OrgCommand::VerifyDomain {
                domain: "nobody.example".to_string(),
                at_ms: 5,
            }),
            Err(OrgError::DomainNotClaimed)
        ));
    }

    #[test]
    fn a_withdrawn_claim_is_gone_and_a_bad_domain_is_never_claimed() {
        let mut org = org();
        for bad in [
            "",
            "acme",
            "-acme.com",
            "acme..com",
            "acme .com",
            "ac_me.com",
        ] {
            assert!(
                matches!(
                    org.decide(OrgCommand::ClaimDomain {
                        domain: bad.to_string(),
                        token: "dv_x".to_string(),
                        at_ms: 2,
                    }),
                    Err(OrgError::InvalidDomain)
                ),
                "{bad:?} should not be claimable"
            );
        }
        let events = org
            .decide(OrgCommand::ClaimDomain {
                domain: "typo.example".to_string(),
                token: "dv_x".to_string(),
                at_ms: 2,
            })
            .expect("claim");
        apply_all(&mut org, events);
        let events = org
            .decide(OrgCommand::WithdrawDomainClaim {
                domain: "typo.example".to_string(),
                at_ms: 3,
            })
            .expect("withdraw");
        apply_all(&mut org, events);
        assert!(org.pending_domains.is_empty());
        assert!(matches!(
            org.decide(OrgCommand::VerifyDomain {
                domain: "typo.example".to_string(),
                at_ms: 4,
            }),
            Err(OrgError::DomainNotClaimed)
        ));
    }

    #[test]
    fn creating_an_org_refuses_a_domain_that_is_not_one() {
        assert!(matches!(
            Org::default().decide(OrgCommand::Create {
                name: "Acme".to_string(),
                admin: AccountId::from_stored("acct_admin"),
                domains: vec!["acme.com".to_string(), "not a domain".to_string()],
                at_ms: 1,
            }),
            Err(OrgError::InvalidDomain)
        ));
    }

    #[test]
    fn the_challenge_record_is_matched_exactly() {
        assert_eq!(
            challenge_record_name("acme.dev"),
            "_opengrok-verify.acme.dev"
        );
        assert_eq!(challenge_record_value("dv_abc"), "opengrok-verify=dv_abc");
        let found = vec![
            "v=spf1 -all".to_string(),
            "\"opengrok-verify=dv_abc\"".to_string(),
        ];
        assert!(challenge_satisfied("dv_abc", &found));
        assert!(!challenge_satisfied(
            "dv_abc",
            &["opengrok-verify=dv_abcd".to_string()]
        ));
        assert!(!challenge_satisfied("dv_abc", &[]));
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
