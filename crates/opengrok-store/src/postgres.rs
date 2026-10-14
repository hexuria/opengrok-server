//! The Postgres event store.
//!
//! Async, unlike the `EventStore` trait the store's unit tests replay accounts through, which is
//! deliberately sync so their in-memory store needs no runtime. Rather than making the trait async
//! (and infecting every caller with a boxed future for the sake of a HashMap), the async surface
//! is stated plainly here.
//!
//! APPEND AND PROJECT IN ONE TRANSACTION. If the log and the view can drift, a caller can be told
//! "signed in" and then read a projection that has never heard of them. They commit together or
//! neither commits.

use std::collections::HashSet;

use crate::vault::{Sealed, Vault};
use opengrok_core::account::{Account, AccountEvent, AccountView, Plan};
use opengrok_core::connection::{Connection, ConnectionEvent, ConnectionView, Owner};
use opengrok_core::coworker::{Coworker, CoworkerEvent, CoworkerView};
use opengrok_core::id::{AccountId, BoxId, CoworkerId, RunId};
use opengrok_core::inference::SourceKind;
use opengrok_core::run::{Run, RunEvent, RunStatus, RunView};
use sqlx::{PgPool, Postgres, Row, Transaction};

use crate::{StoreError, StoreResult, account_stream};

#[derive(Debug, Clone)]
pub struct PgStore {
    pool: PgPool,
    /// The account events stream: `append_run` and `append_schedule` write its notes.
    pub events: opengrok_events::Hub,
}

impl PgStore {
    pub fn new(pool: PgPool) -> Self {
        let events = opengrok_events::Hub::new(pool.clone());
        Self { events, pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// A stream's events in order, and the sequence they were read at: what every `load_*`
    /// replays. An event this binary cannot read is `Corrupt`, never skipped.
    pub(crate) async fn stream<E: serde::de::DeserializeOwned>(
        &self,
        stream: &str,
    ) -> StoreResult<(Vec<E>, i64)> {
        let rows = sqlx::query(
            "select stream_seq, payload from events where stream_id = $1 order by stream_seq",
        )
        .bind(stream)
        .fetch_all(&self.pool)
        .await?;
        let mut seq = 0_i64;
        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            seq = row.try_get::<i64, _>("stream_seq")?;
            let payload: serde_json::Value = row.try_get("payload")?;
            let event = serde_json::from_value(payload);
            events.push(event.map_err(|error| StoreError::Corrupt(error.to_string()))?);
        }
        Ok((events, seq))
    }
}

mod accounts;
mod artifacts;
mod computers;
mod connections;
mod coworkers;
mod policy;
mod recipes;
mod runs;
mod site_logins;

pub use artifacts::*;
pub use computers::*;
pub use connections::*;
pub use coworkers::*;
pub use recipes::*;
pub use runs::*;
pub use site_logins::*;

// Calls only the tests make, kept beside them like `tests/unit/*`: nothing the server serves asks.
#[path = "../tests/support/test_only.rs"]
mod test_only;
