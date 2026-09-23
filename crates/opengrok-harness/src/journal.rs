//! Where a run's events go before anybody sees them.
//!
//! THIS TRAIT IS THE DURABILITY GUARANTEE, EXPRESSED AS A SEAM. A multi-round loop is only
//! resumable if each round's events reach durable storage *before* the next model call is made —
//! otherwise a crash between rounds loses the tool results that the next call depended on, and the
//! run cannot be picked up because nothing knows how far it got.
//!
//! Putting that ordering in the loop rather than in the caller is deliberate: it is a rule about
//! *when* to write, and a rule about when is only enforceable where the sequencing happens. A
//! caller handed the whole run at the end could not restore it.
//!
//! The trait exists so the harness can depend on the ordering without depending on Postgres, and
//! so a test can assert the interleaving — which is the only way to know the rule holds.

use opengrok_wire::agui::Event;

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("the run could not be recorded: {0}")]
    Unwritable(String),
}

/// Somewhere a run's events are durably kept.
#[async_trait::async_trait]
pub trait RunJournal: Send + Sync {
    /// Record events for `run_id`. Must not return until they are durable: the loop treats a
    /// return as permission to continue, and continuing on a lie is how work is lost.
    async fn record(&self, run_id: &str, events: &[Event]) -> Result<(), JournalError>;

    /// Has somebody stopped this run — or has the log ended it some other way?
    ///
    /// The loop's only question is "may I carry on", so an implementation answers yes for ANY
    /// ended run: stopped, failed by the recovery sweep, finished by another loop. A loop on an
    /// ended run otherwise kept running tools whose every event the log then refused.
    ///
    /// ASKED OF THE JOURNAL BECAUSE THE JOURNAL IS WHERE A STOP IS WRITTEN DOWN. A stop is not a
    /// message passed between two tasks that happen to be in the same process: it is a person
    /// pressing a button, recorded in the run's log before anything is said back to them. Reading
    /// it from the same place the run's events go means a stop reaches the turn whether the turn is
    /// in the process that took the request, in another replica, or in a process that has since
    /// restarted — and it means there is no second copy of "is this run still going" to disagree
    /// with the log.
    ///
    /// Asked at step boundaries, so it must be cheap — a primary-key read, never a replay. A
    /// journal that cannot answer says `false`: a database hiccup must stop nothing.
    async fn stopped(&self, _run_id: &str) -> bool {
        false
    }
}

/// Keeps events in memory. For tests, and for a caller that has chosen not to persist.
#[derive(Debug, Default)]
pub struct MemoryJournal {
    recorded: std::sync::Mutex<Vec<(String, Vec<Event>)>>,
}

impl MemoryJournal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every batch, in the order it was recorded.
    pub fn batches(&self) -> Vec<Vec<Event>> {
        self.recorded
            .lock()
            .map(|recorded| {
                recorded
                    .iter()
                    .map(|(_, events)| events.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    }

    pub fn event_count(&self) -> usize {
        self.batches().iter().map(Vec::len).sum()
    }
}

#[async_trait::async_trait]
impl RunJournal for MemoryJournal {
    async fn record(&self, run_id: &str, events: &[Event]) -> Result<(), JournalError> {
        self.recorded
            .lock()
            .map_err(|_| JournalError::Unwritable("the journal's lock was poisoned".to_string()))?
            .push((run_id.to_string(), events.to_vec()));
        Ok(())
    }
}
