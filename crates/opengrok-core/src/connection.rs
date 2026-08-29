//! A connection: an authentication that happened, and who may use it.
//!
//! TWO THINGS THAT LOOK LIKE ONE, AND MUST NOT BE. *Authenticating* is a person going to Google and
//! coming back; *being allowed to use it* is a separate decision made afterwards. Fusing them is
//! what forces every coworker to sign in to Gmail again — the connection would belong to whoever
//! happened to be running when it was made. Kept apart, a person authenticates once and **lends**
//! it to as many coworkers as they like.
//!
//! THREE SCOPES, AND THE ORDER BETWEEN THEM IS THE DESIGN:
//!   - `Global` — the deployment's own key for something that belongs to nobody (a weather API).
//!   - `User`   — a person's account. Almost everything is this.
//!   - `Bot`    — a coworker's *own* identity, so its actions are attributed to it and not to
//!     its owner. Made deliberately, only when the coworker must be somebody.
//!
//! `Bot` beats a loan, and that is not arbitrary: if somebody went to the trouble of giving a
//! coworker its own account, silently acting as the owner instead would put the owner's name on
//! work the coworker did. A loan beats `Global` for the same reason in reverse — the more specific
//! answer to "whose is this" is the right one.
//!
//! THE SECRET IS NOT IN HERE. This aggregate records that a connection exists, whose it is and who
//! may borrow it. The token itself lives encrypted in the store and is fetched at the edge, so it
//! never enters an event, a replay, a log or a model's context (CLAUDE.md #4).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::id::{AccountId, CoworkerId};

/// Whose authentication this is.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "kebab-case")]
pub enum Owner {
    /// The whole deployment. Nothing personal may be stored here.
    Global,
    User(AccountId),
    Bot(CoworkerId),
}

impl Owner {
    /// How specific this owner is. Higher wins when several connections could serve a call.
    fn specificity(&self) -> u8 {
        match self {
            Self::Bot(_) => 3,
            Self::User(_) => 2,
            Self::Global => 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ConnectionEvent {
    /// Somebody authenticated. The token is stored separately; this only records that it happened.
    Connected {
        connector: String,
        owner: Owner,
        /// What to show a person: `you@work.com`, an org name. Never a secret.
        label: String,
        at_ms: i64,
    },
    /// The token was replaced — a refresh, or a re-authentication.
    Refreshed {
        at_ms: i64,
    },
    /// Lent to a coworker. This is the "no need to authenticate again" move.
    LoanedTo {
        coworker: CoworkerId,
        at_ms: i64,
    },
    LoanRevoked {
        coworker: CoworkerId,
        at_ms: i64,
    },
    /// The connection itself is gone. Every loan goes with it.
    Disconnected {
        at_ms: i64,
    },
}

impl ConnectionEvent {
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::Connected { .. } => "connection-connected",
            Self::Refreshed { .. } => "connection-refreshed",
            Self::LoanedTo { .. } => "connection-loaned",
            Self::LoanRevoked { .. } => "connection-loan-revoked",
            Self::Disconnected { .. } => "connection-disconnected",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Connection {
    pub connected: bool,
    pub disconnected: bool,
    pub connector: String,
    pub owner: Option<Owner>,
    pub label: String,
    /// Coworkers this has been lent to. A bot-owned connection needs no loans — it is already the
    /// coworker's — but lending one on is allowed, and is how a team of coworkers shares one bot
    /// account.
    pub loans: BTreeSet<CoworkerId>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectionError {
    #[error("that connection does not exist")]
    NotConnected,
    #[error("that connection has been disconnected")]
    Disconnected,
    #[error("a global connection is not one person's to lend")]
    GlobalNotLendable,
}

#[derive(Debug, Clone)]
pub enum ConnectionCommand {
    Connect {
        connector: String,
        owner: Owner,
        label: String,
        at_ms: i64,
    },
    Refresh {
        at_ms: i64,
    },
    Lend {
        coworker: CoworkerId,
        at_ms: i64,
    },
    Revoke {
        coworker: CoworkerId,
        at_ms: i64,
    },
    Disconnect {
        at_ms: i64,
    },
}

impl Connection {
    pub fn replay<'a>(events: impl IntoIterator<Item = &'a ConnectionEvent>) -> Self {
        let mut state = Self::default();
        for event in events {
            state.apply(event);
        }
        state
    }

    pub fn apply(&mut self, event: &ConnectionEvent) {
        match event {
            ConnectionEvent::Connected {
                connector,
                owner,
                label,
                ..
            } => {
                self.connected = true;
                self.disconnected = false;
                self.connector = connector.clone();
                self.owner = Some(owner.clone());
                self.label = label.clone();
            }
            ConnectionEvent::Refreshed { .. } => {}
            ConnectionEvent::LoanedTo { coworker, .. } => {
                self.loans.insert(coworker.clone());
            }
            ConnectionEvent::LoanRevoked { coworker, .. } => {
                self.loans.remove(coworker);
            }
            ConnectionEvent::Disconnected { .. } => {
                self.disconnected = true;
                // Every loan goes with it. A loan that outlived its connection would be a coworker
                // holding a key to a door that no longer exists — or worse, to whatever is put
                // behind that name next.
                self.loans.clear();
            }
        }
    }

    /// May this coworker use this connection?
    pub fn usable_by(&self, coworker: &CoworkerId) -> bool {
        if !self.connected || self.disconnected {
            return false;
        }
        match &self.owner {
            // The coworker's own. No loan needed; it already is theirs.
            Some(Owner::Bot(owner)) => owner == coworker || self.loans.contains(coworker),
            // A person's, borrowed. This is the share.
            Some(Owner::User(_)) => self.loans.contains(coworker),
            // Everybody's, by definition.
            Some(Owner::Global) => true,
            None => false,
        }
    }

    pub fn decide(
        &self,
        command: ConnectionCommand,
    ) -> Result<Vec<ConnectionEvent>, ConnectionError> {
        match command {
            ConnectionCommand::Connect {
                connector,
                owner,
                label,
                at_ms,
            } => Ok(vec![ConnectionEvent::Connected {
                connector,
                owner,
                label,
                at_ms,
            }]),

            ConnectionCommand::Refresh { at_ms } => {
                self.alive()?;
                Ok(vec![ConnectionEvent::Refreshed { at_ms }])
            }

            ConnectionCommand::Lend { coworker, at_ms } => {
                self.alive()?;
                // A global connection is already available to everyone; "lending" one would imply
                // the lender controls it, and nobody does.
                if self.owner == Some(Owner::Global) {
                    return Err(ConnectionError::GlobalNotLendable);
                }
                // Lending twice is not an error — it is the same intent arriving again, and
                // returning no event keeps the log free of duplicates that say nothing.
                if self.loans.contains(&coworker) {
                    return Ok(Vec::new());
                }
                Ok(vec![ConnectionEvent::LoanedTo { coworker, at_ms }])
            }

            ConnectionCommand::Revoke { coworker, at_ms } => {
                self.alive()?;
                if !self.loans.contains(&coworker) {
                    return Ok(Vec::new());
                }
                Ok(vec![ConnectionEvent::LoanRevoked { coworker, at_ms }])
            }

            ConnectionCommand::Disconnect { at_ms } => {
                self.alive()?;
                Ok(vec![ConnectionEvent::Disconnected { at_ms }])
            }
        }
    }

    fn alive(&self) -> Result<(), ConnectionError> {
        if !self.connected {
            return Err(ConnectionError::NotConnected);
        }
        if self.disconnected {
            return Err(ConnectionError::Disconnected);
        }
        Ok(())
    }
}

/// One row of the read side: enough to choose between candidates without replaying anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionView {
    pub id: String,
    pub connector: String,
    pub owner: Owner,
    pub label: String,
    pub loans: BTreeSet<CoworkerId>,
    pub updated_at_ms: i64,
    /// When the access token stops working. `None` means it does not expire — GitHub OAuth-app
    /// tokens are like this — and must NOT be read as "already expired", which would refresh
    /// forever against a provider that issues no refresh token.
    #[serde(default)]
    pub expires_at_ms: Option<i64>,
}

impl ConnectionView {
    /// Whether this should be refreshed before it is used.
    ///
    /// A LEEWAY, NOT AN EXACT COMPARISON. A token with four seconds left passes an `expires_at >
    /// now` test and then expires while the request is in flight; the call fails for a reason
    /// nobody can reproduce. Sixty seconds is longer than any call we make.
    pub fn is_expiring(&self, now_ms: i64) -> bool {
        const LEEWAY_MS: i64 = 60_000;
        match self.expires_at_ms {
            Some(expires_at) => expires_at - now_ms < LEEWAY_MS,
            // Never expires; refreshing would be a request to an endpoint that issues nothing.
            None => false,
        }
    }
}

impl ConnectionView {
    fn usable_by(&self, coworker: &CoworkerId) -> bool {
        match &self.owner {
            Owner::Bot(owner) => owner == coworker || self.loans.contains(coworker),
            Owner::User(_) => self.loans.contains(coworker),
            Owner::Global => true,
        }
    }
}

/// Which connection a coworker should use for a connector.
///
/// THE MOST SPECIFIC USABLE ONE WINS: the coworker's own, then one lent to it, then the
/// deployment's. Ties inside a scope go to the most recently updated, so re-authenticating fixes a
/// stale token rather than adding a second one nobody chooses between.
///
/// Returns `None` when nothing is usable — which the caller must treat as "not connected", never
/// as "use whatever is nearest".
pub fn resolve<'a>(
    candidates: &'a [ConnectionView],
    connector: &str,
    coworker: &CoworkerId,
) -> Option<&'a ConnectionView> {
    candidates
        .iter()
        .filter(|candidate| candidate.connector == connector)
        .filter(|candidate| candidate.usable_by(coworker))
        .max_by_key(|candidate| (candidate.owner.specificity(), candidate.updated_at_ms))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn person() -> AccountId {
        AccountId::from_stored("acct_1")
    }
    fn bot() -> CoworkerId {
        CoworkerId::from_stored("cw_1")
    }
    fn other_bot() -> CoworkerId {
        CoworkerId::from_stored("cw_2")
    }

    fn view(id: &str, owner: Owner, loans: &[CoworkerId], at: i64) -> ConnectionView {
        ConnectionView {
            id: id.to_string(),
            connector: "gmail".to_string(),
            owner,
            label: "you@work.com".to_string(),
            loans: loans.iter().cloned().collect(),
            updated_at_ms: at,
            expires_at_ms: None,
        }
    }

    fn connected(owner: Owner) -> Connection {
        let mut connection = Connection::default();
        for event in connection
            .decide(ConnectionCommand::Connect {
                connector: "gmail".to_string(),
                owner,
                label: "you@work.com".to_string(),
                at_ms: 1,
            })
            .unwrap()
        {
            connection.apply(&event);
        }
        connection
    }

    /// The share: a person authenticates once and lends it, rather than every coworker signing in.
    #[test]
    fn a_lent_connection_is_usable_without_authenticating_again() {
        let mut connection = connected(Owner::User(person()));
        assert!(!connection.usable_by(&bot()), "not lent yet");

        for event in connection
            .decide(ConnectionCommand::Lend {
                coworker: bot(),
                at_ms: 2,
            })
            .unwrap()
        {
            connection.apply(&event);
        }
        assert!(connection.usable_by(&bot()));
        // And only to whom it was lent.
        assert!(!connection.usable_by(&other_bot()));
    }

    /// A coworker's own connection needs no loan: it already is theirs.
    #[test]
    fn a_bots_own_connection_needs_no_loan() {
        let connection = connected(Owner::Bot(bot()));
        assert!(connection.usable_by(&bot()));
        assert!(!connection.usable_by(&other_bot()));
    }

    #[test]
    fn a_global_connection_is_usable_by_anyone() {
        let connection = connected(Owner::Global);
        assert!(connection.usable_by(&bot()));
        assert!(connection.usable_by(&other_bot()));
    }

    /// Nobody controls a global connection, so nobody may lend it.
    #[test]
    fn a_global_connection_cannot_be_lent() {
        assert_eq!(
            connected(Owner::Global).decide(ConnectionCommand::Lend {
                coworker: bot(),
                at_ms: 2
            }),
            Err(ConnectionError::GlobalNotLendable)
        );
    }

    /// Lending twice is the same intent arriving again, not an error and not a second event.
    #[test]
    fn lending_twice_writes_nothing_the_second_time() {
        let mut connection = connected(Owner::User(person()));
        for event in connection
            .decide(ConnectionCommand::Lend {
                coworker: bot(),
                at_ms: 2,
            })
            .unwrap()
        {
            connection.apply(&event);
        }
        assert!(
            connection
                .decide(ConnectionCommand::Lend {
                    coworker: bot(),
                    at_ms: 3
                })
                .unwrap()
                .is_empty()
        );
    }

    /// A loan that outlived its connection would be a key to whatever is put behind that name next.
    #[test]
    fn disconnecting_takes_every_loan_with_it() {
        let mut connection = connected(Owner::User(person()));
        for command in [
            ConnectionCommand::Lend {
                coworker: bot(),
                at_ms: 2,
            },
            ConnectionCommand::Lend {
                coworker: other_bot(),
                at_ms: 3,
            },
            ConnectionCommand::Disconnect { at_ms: 4 },
        ] {
            for event in connection.decide(command).unwrap() {
                connection.apply(&event);
            }
        }
        assert!(connection.loans.is_empty());
        assert!(!connection.usable_by(&bot()));
        assert!(!connection.usable_by(&other_bot()));
    }

    #[test]
    fn revoking_a_loan_stops_that_coworker_only() {
        let mut connection = connected(Owner::User(person()));
        for command in [
            ConnectionCommand::Lend {
                coworker: bot(),
                at_ms: 2,
            },
            ConnectionCommand::Lend {
                coworker: other_bot(),
                at_ms: 3,
            },
            ConnectionCommand::Revoke {
                coworker: bot(),
                at_ms: 4,
            },
        ] {
            for event in connection.decide(command).unwrap() {
                connection.apply(&event);
            }
        }
        assert!(!connection.usable_by(&bot()));
        assert!(connection.usable_by(&other_bot()));
    }

    // ---- resolution ------------------------------------------------------

    /// THE ORDER THAT MATTERS. A coworker given its own account must act as itself, or its owner's
    /// name ends up on work the coworker did.
    #[test]
    fn a_bots_own_connection_beats_one_lent_to_it() {
        let candidates = vec![
            view("lent", Owner::User(person()), &[bot()], 100),
            view("own", Owner::Bot(bot()), &[], 1),
        ];
        assert_eq!(resolve(&candidates, "gmail", &bot()).unwrap().id, "own");
    }

    /// And a lent one beats the house key: the more specific answer to "whose is this" wins.
    #[test]
    fn a_lent_connection_beats_a_global_one() {
        let candidates = vec![
            view("house", Owner::Global, &[], 100),
            view("lent", Owner::User(person()), &[bot()], 1),
        ];
        assert_eq!(resolve(&candidates, "gmail", &bot()).unwrap().id, "lent");
    }

    /// A connection belonging to somebody who never lent it is not a candidate at all.
    #[test]
    fn an_unlent_connection_is_invisible() {
        let candidates = vec![view("theirs", Owner::User(person()), &[], 100)];
        assert!(resolve(&candidates, "gmail", &bot()).is_none());
    }

    /// Re-authenticating replaces a stale token rather than adding a rival nobody picks between.
    #[test]
    fn the_newest_wins_inside_a_scope() {
        let candidates = vec![
            view("old", Owner::User(person()), &[bot()], 10),
            view("new", Owner::User(person()), &[bot()], 20),
        ];
        assert_eq!(resolve(&candidates, "gmail", &bot()).unwrap().id, "new");
    }

    /// Nothing usable must read as "not connected", never as "use whatever is nearest".
    #[test]
    fn a_different_connector_is_never_substituted() {
        let mut github = view("gh", Owner::Global, &[], 100);
        github.connector = "github".to_string();
        assert!(resolve(&[github], "gmail", &bot()).is_none());
    }

    /// A token that expires mid-flight fails for a reason nobody can reproduce.
    #[test]
    fn a_token_expiring_within_the_leeway_counts_as_expiring() {
        let mut view = view("c", Owner::Global, &[], 1);
        // Expires two minutes from "now", so the leeway is the thing being tested rather than the
        // arithmetic: 30s left is inside the 60s leeway, 120s left is outside it.
        view.expires_at_ms = Some(120_000);
        assert!(
            view.is_expiring(90_000),
            "30s left is inside the 60s leeway"
        );
        assert!(view.is_expiring(120_001), "already past is expiring");
        assert!(!view.is_expiring(0), "120s left is comfortably outside it");
    }

    /// GitHub OAuth-app tokens never expire. Refreshing one would be a request to an endpoint that
    /// issues nothing, forever.
    #[test]
    fn a_token_with_no_expiry_never_needs_refreshing() {
        let view = view("c", Owner::Global, &[], 1);
        assert_eq!(view.expires_at_ms, None);
        assert!(!view.is_expiring(i64::MAX));
    }

    #[test]
    fn nothing_at_all_resolves_to_nothing() {
        assert!(resolve(&[], "gmail", &bot()).is_none());
    }
}
