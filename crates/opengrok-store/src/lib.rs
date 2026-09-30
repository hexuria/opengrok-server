//! The event store, and the projections read from it.
//!
//! ONE APPEND-ONLY TABLE IS THE WHOLE OF THE EVENT SOURCING. No framework: a transcript, a run and
//! an account are all already sequences of things that happened, so the log is the natural grain
//! rather than an imposed one. Reads never replay it — a projection row is updated in the SAME
//! transaction as the append, so a query cannot observe an event that has not yet reached the view.
//!
//! `expected_seq` is what makes concurrent writers safe: two requests refreshing the same session
//! both read version 4, both try to append version 5, and the unique index means exactly one wins.
//! The loser is told `Conflict` and retries against fresh state. Losing that check would let a
//! rotated refresh token be rotated twice.
//!
//! Queries are `sqlx::query` rather than `query!` on purpose: the macros need a live database at
//! COMPILE time, which would put Postgres in the path of `cargo check` and CI for everyone.

use opengrok_core::id::AccountId;

pub mod auto_review;
pub mod autonomy;
pub mod gateway;
pub mod identity;
pub mod migrations;
pub mod pending;
pub mod points;
pub mod postgres;
pub mod purge;
pub mod replica;
pub mod skills;
pub mod spend;
pub mod templates;
pub mod vault;
pub mod vault_rows;

pub use autonomy::{DueSchedule, FiredBy, HookRow, LogEvent};
pub use gateway::{
    CoworkerKeyView, KeyRefusal, McpCallView, NewGatewayKey, NewMcpCall, OAuthClient, RefreshClaim,
    RefreshTokenRow,
};
pub use pending::{
    DrainKey, DrainResult, EnqueueResult, NewPendingUserMessage, PendingUserMessagePatch,
    PendingUserMessageRow,
};
pub use points::{PointsLimit, PointsLimitRow, PointsScope};
pub use postgres::{
    ArtifactRow, CredentialUpdate, PasskeyMeta, PasskeyWrite, PgStore, RecipeGrantRow, RecipeRow,
    RecipeRunRow, RecipeShareRow, RecipeVersionRow, RosterOwner, SiteLoginRow, SiteLoginSecrets,
    SiteLoginWrite, ThreadListing, ThreadRun,
};
pub use purge::PurgeReport;
pub use replica::{AllowOnce, OAuthCodeRow};
pub use skills::{NewSkill, NewSkillVersion, SkillFileRow, SkillRow, SkillVersionRow};
pub use spend::SpendLimit;
pub use templates::CoworkerTemplate;
pub use vault::{Sealed, Vault};
pub use vault_rows::{ResealReport, VaultCheck};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("another writer got there first; re-read and retry")]
    Conflict,
    #[error("the database refused: {0}")]
    Database(String),
    #[error("a stored event could not be read back: {0}")]
    Corrupt(String),
    /// A sealed credential that will not open. Its own variant because the cure is a key, not a
    /// database: folded into `Corrupt` it read as "store unavailable" to the person revealing it.
    #[error("{0}")]
    Unopenable(String),
    #[error("the store's lock was poisoned")]
    Poisoned,
}

pub type StoreResult<T> = Result<T, StoreError>;

impl From<sqlx::Error> for StoreError {
    fn from(error: sqlx::Error) -> Self {
        // A unique violation on (stream_id, stream_seq) is not a database fault — it is the
        // optimistic-concurrency check doing its job, and the caller must be able to tell the
        // difference in order to retry rather than fail the request.
        if let sqlx::Error::Database(ref db) = error
            && db.code().as_deref() == Some("23505")
        {
            return Self::Conflict;
        }
        Self::Database(error.to_string())
    }
}

// The gate-database guard and each test binary's own database live in opengrok-testdb; they are
// re-exported here because every integration test already reaches for them through the store.
pub use opengrok_testdb::{gate_database_or_panic, is_test_database_url};

/// One stream per coworker.
pub fn coworker_stream(id: &opengrok_core::id::CoworkerId) -> String {
    format!("coworker/{id}")
}

/// One stream per run. Runs and accounts share the `events` table and never the same stream.
pub fn run_stream(id: &opengrok_core::id::RunId) -> String {
    format!("run/{id}")
}

/// One stream per org.
pub fn org_stream(id: &opengrok_core::id::OrgId) -> String {
    format!("org/{id}")
}

/// One stream per schedule.
pub fn schedule_stream(id: &opengrok_core::id::ScheduleId) -> String {
    format!("schedule/{id}")
}

/// The stream a monitor's events live on.
pub fn monitor_stream(id: &opengrok_core::id::MonitorId) -> String {
    format!("monitor/{id}")
}

/// The account stream's id. One stream per account, keyed by the account id.
pub fn account_stream(id: &AccountId) -> String {
    format!("account/{id}")
}

#[cfg(test)]
#[path = "../tests/unit/lib_tests.rs"]
#[allow(clippy::unwrap_used)]
mod tests;
