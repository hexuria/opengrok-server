//! The account aggregate — who is signed in, and on what plan.
//!
//! The first domain in the system, and the template for the ones after it: state is a fold over
//! events, commands are *decisions* that either produce events or refuse, and nothing here knows
//! that Postgres or Axum exist. `decide` is pure and total, which is what makes the behaviour
//! testable without a database and replayable from the log.
//!
//! Provenance for the vocabulary: `opengrok/source/electron-main/account/cursor-auth.ts:76-85`
//! (`resolveDevLoginPlan` — the five plan strings and which of them are trials) and
//! `source/shared/cursor-session-policy.ts:cursorSessionPresent` (a session exists when BOTH an
//! access and a refresh token are held; that pair is what the client calls `logged-in`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::id::{AccountId, SessionId};

/// How long the just-rotated-away refresh hash still identifies the session.
///
/// NativeChat (and the console) can fire concurrent `POST /auth/refresh` with the same cookie.
/// The first request rotates; a loser that presents the old hash inside this window must reuse
/// the already-minted current pair (fresh access, same current refresh) instead of 401. After
/// this, the old hash is dead — a leaked refresh token must not be immortal. 45s covers a burst
/// of retries without stretching the stolen-token window.
pub const REFRESH_GRACE_MS: i64 = 45_000;

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
    /// A refresh rotated the pair. `refresh_token_hash` is the new current hash.
    /// `previous_refresh_token_hash` is the one just rotated away: it remains acceptable for
    /// [`REFRESH_GRACE_MS`] after `at_ms` so a concurrent refresh with the old cookie reuses
    /// this rotation instead of 401. Absent on events written before the grace existed.
    SessionRefreshed {
        session_id: SessionId,
        refresh_token_hash: String,
        #[serde(default)]
        previous_refresh_token_hash: Option<String>,
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
    /// Credential registration — signup or admin mint. Carries the argon2 hash (never the
    /// password), the person's name, and the org they belong to.
    CredentialsSet {
        password_hash: String,
        first_name: String,
        last_name: String,
        org_id: String,
        at_ms: i64,
    },
    /// The Resend round-trip completed (or was auto-passed when no mailer is configured).
    EmailVerified {
        at_ms: i64,
    },
    /// An admin flipped the account's usability. Login refuses a disabled account distinguishably.
    Enabled {
        at_ms: i64,
    },
    Disabled {
        at_ms: i64,
    },
    /// Self-service profile edit — name and avatar. Email is deliberately NOT here: a person
    /// cannot change the address their org and their invite were bound to.
    ProfileUpdated {
        first_name: String,
        last_name: String,
        avatar_url: Option<String>,
        at_ms: i64,
    },
    /// A password change (self-service, after proving the current one) or an admin reset.
    PasswordChanged {
        password_hash: String,
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
            Self::CredentialsSet { .. } => "credentials-set",
            Self::EmailVerified { .. } => "email-verified",
            Self::Enabled { .. } => "account-enabled",
            Self::Disabled { .. } => "account-disabled",
            Self::ProfileUpdated { .. } => "account-profile-updated",
            Self::PasswordChanged { .. } => "account-password-changed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub refresh_token_hash: String,
    /// The hash `SessionRefreshed` just replaced. Acceptable only inside [`REFRESH_GRACE_MS`]
    /// of `refreshed_at_ms`; never kept as a second live token.
    pub previous_refresh_token_hash: Option<String>,
    pub refreshed_at_ms: i64,
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
    /// Credential fields — present once `CredentialsSet` has been applied. A dev/session-only
    /// account (the pre-identity path) has none of these and cannot credential-login.
    pub password_hash: Option<String>,
    pub first_name: String,
    pub last_name: String,
    pub org_id: Option<String>,
    pub verified: bool,
    pub enabled: bool,
    pub avatar_url: Option<String>,
}

/// Current hash vs just-rotated-away hash inside [`REFRESH_GRACE_MS`].
enum RefreshMatch<'a> {
    Current(&'a SessionId),
    Previous(&'a SessionId),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AccountError {
    #[error("that refresh token is not valid for this account")]
    UnknownRefreshToken,
    #[error("that session has been revoked")]
    SessionRevoked,
    #[error("the account does not exist")]
    NotRegistered,
    #[error("that account has no password set")]
    NoCredentials,
    #[error("that account's email is not verified")]
    NotVerified,
    #[error("that account is not enabled")]
    NotEnabled,
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
    /// Rotate a session's refresh token. The presented hash must match a live session's current
    /// hash, or — inside [`REFRESH_GRACE_MS`] of the last rotation — the hash just rotated away
    /// (that path emits no event; the HTTP layer reuses the current pair).
    Refresh {
        presented_hash: String,
        new_hash: String,
        at_ms: i64,
    },
    SignOut {
        session_id: SessionId,
        at_ms: i64,
    },
    /// Set credentials on this account. `verified`/`enabled` let the CLI mint a ready account
    /// (both true) while signup mints a pending one (verified per the mailer, enabled false).
    Register {
        email: String,
        password_hash: String,
        first_name: String,
        last_name: String,
        org_id: String,
        plan: Plan,
        verified: bool,
        enabled: bool,
        at_ms: i64,
    },
    VerifyEmail {
        at_ms: i64,
    },
    Enable {
        at_ms: i64,
    },
    Disable {
        at_ms: i64,
    },
    UpdateProfile {
        first_name: String,
        last_name: String,
        avatar_url: Option<String>,
        at_ms: i64,
    },
    ChangePassword {
        password_hash: String,
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
                at_ms,
                ..
            } => {
                self.sessions.insert(
                    session_id.clone(),
                    Session {
                        refresh_token_hash: refresh_token_hash.clone(),
                        previous_refresh_token_hash: None,
                        refreshed_at_ms: *at_ms,
                        revoked: false,
                    },
                );
            }
            AccountEvent::SessionRefreshed {
                session_id,
                refresh_token_hash,
                previous_refresh_token_hash,
                at_ms,
                ..
            } => {
                if let Some(session) = self.sessions.get_mut(session_id) {
                    session.previous_refresh_token_hash = previous_refresh_token_hash
                        .clone()
                        .or_else(|| Some(session.refresh_token_hash.clone()));
                    session.refresh_token_hash = refresh_token_hash.clone();
                    session.refreshed_at_ms = *at_ms;
                }
            }
            AccountEvent::SessionRevoked { session_id, .. } => {
                if let Some(session) = self.sessions.get_mut(session_id) {
                    session.revoked = true;
                }
            }
            AccountEvent::CredentialsSet {
                password_hash,
                first_name,
                last_name,
                org_id,
                ..
            } => {
                self.registered = true;
                self.password_hash = Some(password_hash.clone());
                self.first_name = first_name.clone();
                self.last_name = last_name.clone();
                // AN EMPTY ORG IS NO ORG, and this is the line that has to say so.
                //
                // `CredentialsSet.org_id` is a plain `String` because every caller had one to
                // hand; an account registered without one carried `""`, and this used to replay
                // it to `Some("")`. Two such accounts then had EQUAL orgs, so anything deciding
                // "same org, therefore may read" made every person in no org a colleague of
                // every other. That is a bug class rather than a bug — each reader that forgot
                // the empty string grew its own copy of it — so it is closed where the value is
                // born rather than at each reader in turn.
                self.org_id = Some(org_id.clone()).filter(|org| !org.is_empty());
            }
            AccountEvent::EmailVerified { .. } => self.verified = true,
            AccountEvent::Enabled { .. } => self.enabled = true,
            AccountEvent::Disabled { .. } => self.enabled = false,
            AccountEvent::ProfileUpdated {
                first_name,
                last_name,
                avatar_url,
                ..
            } => {
                self.first_name = first_name.clone();
                self.last_name = last_name.clone();
                self.avatar_url = avatar_url.clone();
            }
            AccountEvent::PasswordChanged { password_hash, .. } => {
                self.password_hash = Some(password_hash.clone());
            }
        }
    }

    /// Which live session the presented refresh hash belongs to: the current hash, or the
    /// just-rotated-away hash inside [`REFRESH_GRACE_MS`].
    fn match_refresh(
        &self,
        presented_hash: &str,
        at_ms: i64,
    ) -> Result<RefreshMatch<'_>, AccountError> {
        if !self.registered {
            return Err(AccountError::NotRegistered);
        }
        if let Some((session_id, session)) = self
            .sessions
            .iter()
            .find(|(_, session)| session.refresh_token_hash == presented_hash)
        {
            if session.revoked {
                return Err(AccountError::SessionRevoked);
            }
            return Ok(RefreshMatch::Current(session_id));
        }
        if let Some((session_id, session)) = self.sessions.iter().find(|(_, session)| {
            session.previous_refresh_token_hash.as_deref() == Some(presented_hash)
        }) {
            if session.revoked {
                return Err(AccountError::SessionRevoked);
            }
            if at_ms.saturating_sub(session.refreshed_at_ms) <= REFRESH_GRACE_MS {
                return Ok(RefreshMatch::Previous(session_id));
            }
            return Err(AccountError::UnknownRefreshToken);
        }
        Err(AccountError::UnknownRefreshToken)
    }

    /// The session a presented refresh hash currently names, including grace reuse.
    /// The HTTP rotation path uses this to mint a fresh access token without a new event.
    pub fn session_id_for_refresh(
        &self,
        presented_hash: &str,
        at_ms: i64,
    ) -> Result<&SessionId, AccountError> {
        match self.match_refresh(presented_hash, at_ms)? {
            RefreshMatch::Current(session_id) | RefreshMatch::Previous(session_id) => {
                Ok(session_id)
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
                match self.match_refresh(&presented_hash, at_ms)? {
                    RefreshMatch::Current(session_id) => Ok(vec![AccountEvent::SessionRefreshed {
                        session_id: session_id.clone(),
                        refresh_token_hash: new_hash,
                        previous_refresh_token_hash: Some(presented_hash),
                        at_ms,
                    }]),
                    // Within grace: no second rotation. The HTTP layer returns the already-minted
                    // current refresh (and a fresh access token). `new_hash` is discarded.
                    RefreshMatch::Previous(_) => Ok(vec![]),
                }
            }

            AccountCommand::SignOut { session_id, at_ms } => {
                if !self.sessions.contains_key(&session_id) {
                    return Err(AccountError::UnknownRefreshToken);
                }
                Ok(vec![AccountEvent::SessionRevoked { session_id, at_ms }])
            }

            AccountCommand::Register {
                email,
                password_hash,
                first_name,
                last_name,
                org_id,
                plan,
                verified,
                enabled,
                at_ms,
            } => {
                // Registration is the account coming into being as a person, not a session. A
                // fresh account also needs its `Registered` marker (email/plan) if it never had a
                // dev sign-in — emit it first so a credential-only account still has a plan.
                let mut events = Vec::new();
                if !self.registered {
                    events.push(AccountEvent::Registered {
                        email: email.clone(),
                        plan,
                        trial: false,
                        at_ms,
                    });
                }
                events.push(AccountEvent::CredentialsSet {
                    password_hash,
                    first_name,
                    last_name,
                    org_id,
                    at_ms,
                });
                if verified {
                    events.push(AccountEvent::EmailVerified { at_ms });
                }
                if enabled {
                    events.push(AccountEvent::Enabled { at_ms });
                }
                Ok(events)
            }

            AccountCommand::VerifyEmail { at_ms } => {
                if self.password_hash.is_none() {
                    return Err(AccountError::NoCredentials);
                }
                Ok(vec![AccountEvent::EmailVerified { at_ms }])
            }

            AccountCommand::Enable { at_ms } => {
                if self.password_hash.is_none() {
                    return Err(AccountError::NoCredentials);
                }
                Ok(vec![AccountEvent::Enabled { at_ms }])
            }

            AccountCommand::Disable { at_ms } => Ok(vec![AccountEvent::Disabled { at_ms }]),

            AccountCommand::UpdateProfile {
                first_name,
                last_name,
                avatar_url,
                at_ms,
            } => Ok(vec![AccountEvent::ProfileUpdated {
                first_name,
                last_name,
                avatar_url,
                at_ms,
            }]),

            AccountCommand::ChangePassword {
                password_hash,
                at_ms,
            } => {
                if self.password_hash.is_none() {
                    return Err(AccountError::NoCredentials);
                }
                Ok(vec![AccountEvent::PasswordChanged {
                    password_hash,
                    at_ms,
                }])
            }
        }
    }

    /// The gate a credential login must pass, in order, each failure distinguishable so the client
    /// can say which. Returns the password hash to verify against on success.
    pub fn credential_login_ready(&self) -> Result<&str, AccountError> {
        let hash = self
            .password_hash
            .as_deref()
            .ok_or(AccountError::NoCredentials)?;
        if !self.verified {
            return Err(AccountError::NotVerified);
        }
        if !self.enabled {
            return Err(AccountError::NotEnabled);
        }
        Ok(hash)
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
    /// Credential fields — `None`/empty/false for a dev/session-only account.
    #[serde(default)]
    pub password_hash: Option<String>,
    #[serde(default)]
    pub first_name: String,
    #[serde(default)]
    pub last_name: String,
    #[serde(default)]
    pub org_id: Option<String>,
    #[serde(default)]
    pub verified: bool,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub avatar_url: Option<String>,
}

impl AccountView {
    /// The identity-agnostic view a dev/session sign-in produces — no credentials.
    pub fn session_only(
        id: AccountId,
        email: String,
        plan: Plan,
        trial: bool,
        updated_at_ms: i64,
    ) -> Self {
        Self {
            id,
            email,
            plan,
            trial,
            updated_at_ms,
            password_hash: None,
            first_name: String::new(),
            last_name: String::new(),
            org_id: None,
            verified: false,
            enabled: false,
            avatar_url: None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// AN EMPTY ORG IS NO ORG. `Register` carries `org_id` as a plain `String`, and an account
    /// made without one used to replay to `Some("")` — which compares EQUAL to the next orgless
    /// account's, so every "are we in the same org" check said yes. Fixed here rather than in
    /// each reader, and pinned here so it cannot come back.
    #[test]
    fn registering_without_an_org_leaves_no_org_rather_than_an_empty_one() {
        let register = |org_id: &str| AccountCommand::Register {
            email: "ada@og.local".to_string(),
            password_hash: "x".to_string(),
            first_name: "Ada".to_string(),
            last_name: String::new(),
            org_id: org_id.to_string(),
            plan: Plan::Pro,
            verified: true,
            enabled: true,
            at_ms: 1,
        };
        let none = Account::replay(&Account::default().decide(register("")).unwrap());
        assert_eq!(none.org_id, None, "an empty org is no org");

        let some = Account::replay(&Account::default().decide(register("org_1")).unwrap());
        assert_eq!(some.org_id.as_deref(), Some("org_1"));

        // The point of the fix, stated as the thing that was wrong: two people in no org.
        let other = Account::replay(&Account::default().decide(register("")).unwrap());
        assert!(
            none.org_id.is_none() && other.org_id.is_none(),
            "two people in no org must not share an org id"
        );
    }

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
        assert_eq!(
            session.previous_refresh_token_hash.as_deref(),
            Some("hash-1")
        );
        assert_eq!(session.refreshed_at_ms, 30);
    }

    /// Inside the grace window the rotated-away hash reuses this rotation: no new event, current
    /// hash stays. After the window it is dead — a leaked refresh token must not be immortal.
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

        let reuse = account
            .decide(AccountCommand::Refresh {
                presented_hash: "hash-1".to_string(),
                new_hash: "hash-3".to_string(),
                at_ms: 30 + REFRESH_GRACE_MS,
            })
            .unwrap();
        assert!(
            reuse.is_empty(),
            "within grace, reuse emits no second rotation: {reuse:?}"
        );
        let session = account
            .sessions
            .get(&SessionId::from_stored("sess_1"))
            .unwrap();
        assert_eq!(
            session.refresh_token_hash, "hash-2",
            "grace reuse must not mint another rotation"
        );

        assert_eq!(
            account.decide(AccountCommand::Refresh {
                presented_hash: "hash-1".to_string(),
                new_hash: "hash-3".to_string(),
                at_ms: 30 + REFRESH_GRACE_MS + 1,
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
