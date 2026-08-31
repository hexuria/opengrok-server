//! Domain types, ids and errors for OpenGrok.
//!
//! This crate performs no I/O. It exists so every other crate agrees on what a coworker, a run
//! and a principal *are* without depending on a database, an HTTP client, or each other.

pub mod account;
pub mod connection;
pub mod coworker;
pub mod id;
pub mod monitor;
pub mod org;
pub mod run;
pub mod schedule;

pub use account::{
    Account, AccountCommand, AccountError, AccountEvent, AccountView, Plan, Session,
};
pub use connection::{
    Connection, ConnectionCommand, ConnectionError, ConnectionEvent, ConnectionView, Owner,
};
pub use coworker::{
    BoxMode, Coworker, CoworkerCommand, CoworkerError, CoworkerEvent, CoworkerView,
};
pub use id::{
    AccountId, BoxId, CoworkerId, MonitorId, OrgId, PrincipalId, RunId, ScheduleId, SessionId,
    TranscriptEntryId,
};
pub use monitor::{Monitor, MonitorCommand, MonitorError, MonitorEvent, MonitorView};
pub use org::{Org, OrgCommand, OrgError, OrgEvent, OrgView, email_domain, normalize_domain};
pub use run::{Run, RunCommand, RunError, RunEvent, RunStatus, RunView};
pub use schedule::{
    Schedule, ScheduleCommand, ScheduleError, ScheduleEvent, ScheduleView, next_fire_ms,
    normalized_cron,
};

/// What went wrong, in the vocabulary of the domain rather than of a transport.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no coworker with id {0}")]
    NoSuchCoworker(CoworkerId),
    #[error("{principal} may not {action} coworker {coworker}")]
    Refused {
        principal: PrincipalId,
        action: &'static str,
        coworker: CoworkerId,
    },
    #[error("the coworker's computer is unreachable: {0}")]
    BoxUnreachable(String),
    #[error("the model gateway refused: {0}")]
    Gateway(String),
    #[error(transparent)]
    Other(#[from] anyhow_lite::Any),
}

/// A stand-in so this crate stays dependency-light; replaced by a real boxed error in the slice
/// that needs one.
pub mod anyhow_lite {
    #[derive(Debug, thiserror::Error)]
    #[error("{0}")]
    pub struct Any(pub String);
}

pub type Result<T> = std::result::Result<T, Error>;
