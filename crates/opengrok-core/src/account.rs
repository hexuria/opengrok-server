//! The account aggregate — who is signed in, and on what plan.
//!
//! The first domain in the system, and the template for the ones after it: state is a fold over
//! events, commands are *decisions* that either produce events or refuse, and nothing here knows
//! that Postgres or Axum exist. `decide` is pure and total, which is what makes the behaviour
//! testable without a database and replayable from the log.
//!
//! Provenance for the vocabulary: `grok-bot/source/electron-main/account/cursor-auth.ts:76-85`
//! (`resolveDevLoginPlan` — the five plan strings and which of them are trials) and
//! `source/shared/cursor-session-policy.ts:cursorSessionPresent` (a session exists when BOTH an
//! access and a refresh token are held; that pair is what the client calls `logged-in`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::id::{AccountId, SessionId};

/// The plans the client can ask for, spelled as it spells them on the wire.
///
/// The client sends a *tier* (`"ProTrial"`) and maps it to a plan + trial flag before the request
/// leaves it (`cursor-auth.ts:76-85`), so what arrives here is already the plan. `Ultra` is the
/// default for an unrecognised tier — the client's own `default:` arm, not a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Plan {
    Free,
    Pro,
    ProPlus,
    Enterprise,
    Ultra,
}

impl Plan {
    /// Parse the `plan` query parameter. Unknown values become `Ultra`, matching the client's
    /// `default:` arm — refusing here would strand a client we do not compile.
    pub fn from_wire(value: &str) -> Self {
        match value {
            "free" => Self::Free,
            "pro" => Self::Pro,
            "pro_plus" => Self::ProPlus,
            "enterprise" => Self::Enterprise,
            _ => Self::Ultra,
        }
    }

    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Free => "free",
            Self::Pro => "pro",
            Self::ProPlus => "pro_plus",
            Self::Enterprise => "enterprise",
            Self::Ultra => "ultra",
        }
    }
}

/// What happened to an account. The log is the truth; every read model is derived from these.
///
/// A refresh token is recorded as a HASH, never in the clear: the log is durable, replayable and
/// exportable, and a bearer credential written into it would outlive the session it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum AccountEvent {
    Registered {
        email: String,
        plan: Plan,
        trial: bool,
        at_ms: i64,
    },
    SessionIssued {
        session_id: SessionId,
        refresh_token_hash: String,
        at_ms: i64,
    },
    /// A refresh rotated the pair. The old hash stops being accepted at this moment.
    SessionRefreshed {
        session_id: SessionId,
        refresh_token_hash: String,
        at_ms: i64,
    },
    SessionRevoked {
        session_id: SessionId,
        at_ms: i64,
    },
    /// A later dev-login for the same email asked for a different plan.
    PlanChanged {
        plan: Plan,
        trial: bool,
        at_ms: i64,
    },
}

impl AccountEvent {
    /// The stored `event_type` column. Kept explicit rather than derived from the serde tag so a
    /// rename in the enum cannot silently orphan the rows already written under the old name.
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::Registered { .. } => "account-registered",
            Self::SessionIssued { .. } => "session-issued",
            Self::SessionRefreshed { .. } => "session-refreshed",
            Self::SessionRevoked { .. } => "session-revoked",
            Self::PlanChanged { .. } => "plan-changed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub refresh_token_hash: String,
    pub revoked: bool,
}

/// The aggregate: a fold over the account's own events.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Account {
    pub registered: bool,
    pub email: String,
    pub plan: Option<Plan>,
    pub trial: bool,
    pub sessions: BTreeMap<SessionId, Session>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AccountError {
    #[error("that refresh token is not valid for this account")]
    UnknownRefreshToken,
    #[error("that session has been revoked")]
    SessionRevoked,
    #[error("the account does not exist")]
    NotRegistered,
}

/// What a caller wants to happen. Named for the intent, not the endpoint that carries it.
#[derive(Debug, Clone)]
pub enum AccountCommand {
    /// Dev-login: register if new, then mint a session. Idempotent by design — the client calls it
    /// on every dev sign-in and must not accumulate accounts.
    SignIn {
        email: String,
        plan: Plan,
        trial: bool,
        session_id: SessionId,
        refresh_token_hash: String,
        at_ms: i64,
    },
    /// Rotate a session's refresh token. The presented hash must match a live session.
    Refresh {
        presented_hash: String,
        new_hash: String,
        at_ms: i64,
    },
    SignOut {
        session_id: SessionId,
        at_ms: i64,
    },
}

impl Account {
    /// Rebuild from the log. The only way an `Account` is ever constructed.
    pub fn replay<'a>(events: impl IntoIterator<Item = &'a AccountEvent>) -> Self {
        let mut state = Self::default();
        for event in events {
            state.apply(event);
        }
        state
    }

    pub fn apply(&mut self, event: &AccountEvent) {
        match event {
            AccountEvent::Registered {
                email, plan, trial, ..
            } => {
                self.registered = true;
                self.email = email.clone();
                self.plan = Some(*plan);
                self.trial = *trial;
            }
            AccountEvent::PlanChanged { plan, trial, .. } => {
                self.plan = Some(*plan);
                self.trial = *trial;
            }
            AccountEvent::SessionIssued {
                session_id,
                refresh_token_hash,
                ..
            } => {
                self.sessions.insert(
                    session_id.clone(),
                    Session {
                        refresh_token_hash: refresh_token_hash.clone(),
                        revoked: false,
                    },
                );
            }
            AccountEvent::SessionRefreshed {
                session_id,
                refresh_token_hash,
                ..
            } => {
                if let Some(session) = self.sessions.get_mut(session_id) {
                    session.refresh_token_hash = refresh_token_hash.clone();
                }
            }
            AccountEvent::SessionRevoked { session_id, .. } => {
                if let Some(session) = self.sessions.get_mut(session_id) {
                    session.revoked = true;
                }
            }
        }
    }

    /// Decide. Pure: no clock, no randomness, no I/O — the caller supplies all three, which is why
    /// a test can assert on exact event sequences.
    pub fn decide(&self, command: AccountCommand) -> Result<Vec<AccountEvent>, AccountError> {
        match command {
            AccountCommand::SignIn {
                email,
                plan,
                trial,
                session_id,
                refresh_token_hash,
                at_ms,
            } => {
                let mut events = Vec::new();
                if !self.registered {
                    events.push(AccountEvent::Registered {
                        email,
                        plan,
                        trial,
                        at_ms,
                    });
                } else if self.plan != Some(plan) || self.trial != trial {
                    events.push(AccountEvent::PlanChanged { plan, trial, at_ms });
                }
                events.push(AccountEvent::SessionIssued {
                    session_id,
                    refresh_token_hash,
                    at_ms,
                });
                Ok(events)
            }

            AccountCommand::Refresh {
                presented_hash,
                new_hash,
                at_ms,
            } => {
                if !self.registered {
                    return Err(AccountError::NotRegistered);
                }
                let (session_id, session) = self
                    .sessions
                    .iter()
                    .find(|(_, session)| session.refresh_token_hash == presented_hash)
                    .ok_or(AccountError::UnknownRefreshToken)?;
                if session.revoked {
                    return Err(AccountError::SessionRevoked);
                }
                Ok(vec![AccountEvent::SessionRefreshed {
                    session_id: session_id.clone(),
                    refresh_token_hash: new_hash,
                    at_ms,
                }])
            }

            AccountCommand::SignOut { session_id, at_ms } => {
                if !self.sessions.contains_key(&session_id) {
                    return Err(AccountError::UnknownRefreshToken);
                }
                Ok(vec![AccountEvent::SessionRevoked { session_id, at_ms }])
            }
        }
    }
}

/// The read model a query answers from — a projection, not the aggregate.
///
/// CQRS in the small: the write side folds events to decide, the read side keeps this flat row so
/// answering "who is this token for" never replays a log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountView {
    pub id: AccountId,
    pub email: String,
    pub plan: Plan,
    pub trial: bool,
    pub updated_at_ms: i64,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn sign_in(email: &str, plan: Plan, trial: bool, at_ms: i64) -> AccountCommand {
        AccountCommand::SignIn {
            email: email.to_string(),
            plan,
            trial,
            session_id: SessionId::from_stored("sess_1"),
            refresh_token_hash: "hash-1".to_string(),
            at_ms,
        }
    }

    #[test]
    fn a_first_sign_in_registers_then_issues() {
        let account = Account::default();
        let events = account
            .decide(sign_in("a@b.c", Plan::Pro, true, 10))
            .unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], AccountEvent::Registered { .. }));
        assert!(matches!(events[1], AccountEvent::SessionIssued { .. }));
    }

    /// The client dev-logs-in on every launch; a second sign-in must not register a second account.
    #[test]
    fn a_repeat_sign_in_only_issues_a_session() {
        let mut account = Account::default();
        for event in account
            .decide(sign_in("a@b.c", Plan::Pro, true, 10))
            .unwrap()
        {
            account.apply(&event);
        }
        let events = account
            .decide(sign_in("a@b.c", Plan::Pro, true, 20))
            .unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], AccountEvent::SessionIssued { .. }));
    }

    #[test]
    fn signing_in_on_a_different_plan_records_the_change() {
        let mut account = Account::default();
        for event in account
            .decide(sign_in("a@b.c", Plan::Pro, true, 10))
            .unwrap()
        {
            account.apply(&event);
        }
        let events = account
            .decide(sign_in("a@b.c", Plan::Ultra, false, 20))
            .unwrap();
        assert!(matches!(
            events[0],
            AccountEvent::PlanChanged {
                plan: Plan::Ultra,
                trial: false,
                ..
            }
        ));
    }

    #[test]
    fn a_refresh_rotates_the_hash_of_the_matching_session() {
        let mut account = Account::default();
        for event in account
            .decide(sign_in("a@b.c", Plan::Pro, false, 10))
            .unwrap()
        {
            account.apply(&event);
        }
        let events = account
            .decide(AccountCommand::Refresh {
                presented_hash: "hash-1".to_string(),
                new_hash: "hash-2".to_string(),
                at_ms: 30,
            })
            .unwrap();
        for event in &events {
            account.apply(event);
        }
        let session = account
            .sessions
            .get(&SessionId::from_stored("sess_1"))
            .unwrap();
        assert_eq!(session.refresh_token_hash, "hash-2");
    }

    /// A rotated-away token must stop working — otherwise a leaked refresh token is immortal.
    #[test]
    fn the_previous_refresh_token_stops_being_accepted() {
        let mut account = Account::default();
        for event in account
            .decide(sign_in("a@b.c", Plan::Pro, false, 10))
            .unwrap()
        {
            account.apply(&event);
        }
        let events = account
            .decide(AccountCommand::Refresh {
                presented_hash: "hash-1".to_string(),
                new_hash: "hash-2".to_string(),
                at_ms: 30,
            })
            .unwrap();
        for event in &events {
            account.apply(event);
        }
        assert_eq!(
            account.decide(AccountCommand::Refresh {
                presented_hash: "hash-1".to_string(),
                new_hash: "hash-3".to_string(),
                at_ms: 40,
            }),
            Err(AccountError::UnknownRefreshToken)
        );
    }

    #[test]
    fn a_revoked_session_refuses_refresh() {
        let mut account = Account::default();
        for event in account
            .decide(sign_in("a@b.c", Plan::Pro, false, 10))
            .unwrap()
        {
            account.apply(&event);
        }
        for event in account
            .decide(AccountCommand::SignOut {
                session_id: SessionId::from_stored("sess_1"),
                at_ms: 50,
            })
            .unwrap()
        {
            account.apply(&event);
        }
        assert_eq!(
            account.decide(AccountCommand::Refresh {
                presented_hash: "hash-1".to_string(),
                new_hash: "hash-2".to_string(),
                at_ms: 60,
            }),
            Err(AccountError::SessionRevoked)
        );
    }

    /// Replaying the log must reconstruct exactly the state the folds produced.
    #[test]
    fn replay_reconstructs_the_same_state() {
        let mut account = Account::default();
        let mut log = Vec::new();
        for event in account
            .decide(sign_in("a@b.c", Plan::Pro, false, 10))
            .unwrap()
        {
            account.apply(&event);
            log.push(event);
        }
        assert_eq!(Account::replay(&log), account);
    }

    #[test]
    fn an_unknown_tier_plan_becomes_ultra_like_the_client() {
        assert_eq!(Plan::from_wire("something-new"), Plan::Ultra);
        assert_eq!(Plan::from_wire("pro_plus"), Plan::ProPlus);
        assert_eq!(Plan::ProPlus.as_wire(), "pro_plus");
    }
}
