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
    /// The run ended before this could be written. For a batch that opened a card: a suspension
    /// the log refuses leaves a card whose answer can only be a 409, so nothing of the batch was
    /// written and the loop may write its round again with the ending the log holds. For a tool's
    /// start (`tools_starting`): the tool does not run, and the loop stops.
    #[error("the run ended before this could be recorded: {0}")]
    Ended(String),
    /// The run was resumed into a newer generation than this loop's (#91): the sweep carried it
    /// on while this loop was still alive after its lease lapsed. Nothing was written, and
    /// nothing this loop writes after it will be either — it has been replaced.
    #[error("the run was carried on by another loop: {0}")]
    Fenced(String),
}

/// Somewhere a run's events are durably kept.
#[async_trait::async_trait]
pub trait RunJournal: Send + Sync {
    /// Record events for `run_id`. Must not return until they are durable: the loop treats a
    /// return as permission to continue, and continuing on a lie is how work is lost.
    ///
    /// A BATCH THAT PARKS IS ALL OR NOTHING. If its suspension cannot be recorded because the run
    /// has ended — a Stop that landed after the loop last asked — write none of it and answer
    /// `Ended`: an `Ok` there is a card on screen with no suspension behind it.
    async fn record(&self, run_id: &str, events: &[Event]) -> Result<(), JournalError>;

    /// Has somebody stopped this run — or has the log ended it some other way?
    ///
    /// The loop's only question is "may I carry on", so an implementation may answer yes for any
    /// ended run: a loop only ever runs on a run it claimed or resumed, so one that ended under it
    /// was ended from outside (the store's journal does; see `StoreJournal::stopped`).
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

    /// Record that these calls are about to run, BEFORE they do (#91). Must not return until
    /// the record is durable, and an error means the calls do not run: an action the log cannot
    /// account for is the one thing a resumed run must never meet blind. `Ended` means the run
    /// ended under the loop, as `stopped` would have said.
    ///
    /// The default records nothing: a journal that keeps no log has no resume to protect.
    async fn tools_starting(
        &self,
        _run_id: &str,
        _tools: &[opengrok_core::run::StartedTool],
    ) -> Result<(), JournalError> {
        Ok(())
    }

    /// `record`, and what the round spent (#256) IN THE SAME WRITE: the recipes it played and
    /// which budget it drew on. One write, so the result that closes a recipe's call and the
    /// record that it played cannot land apart, and a run carried on after a restart reads back
    /// every play whose call the log shows closed.
    ///
    /// The default records the round alone: a journal that keeps no log has no resume to protect.
    async fn record_spent(
        &self,
        run_id: &str,
        events: &[Event],
        _spent: &opengrok_core::run::RoundSpent,
    ) -> Result<(), JournalError> {
        self.record(run_id, events).await
    }
}

/// Keeps events in memory. For tests, and for a caller that has chosen not to persist.
#[derive(Debug, Default)]
pub struct MemoryJournal {
    recorded: std::sync::Mutex<Vec<(String, Vec<Event>)>>,
    spent: std::sync::Mutex<Vec<(Vec<Event>, opengrok_core::run::RoundSpent)>>,
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

    /// Every recipe `record_spent` recorded, in order.
    pub fn recipes_played(&self) -> Vec<String> {
        self.spent()
            .into_iter()
            .flat_map(|spent| spent.recipes)
            .collect()
    }

    /// Every `record_spent`, in order.
    pub fn spent(&self) -> Vec<opengrok_core::run::RoundSpent> {
        self.spent_with_frames()
            .into_iter()
            .map(|(_, spent)| spent)
            .collect()
    }

    /// Every `record_spent` with the frames it was written with.
    pub fn spent_with_frames(&self) -> Vec<(Vec<Event>, opengrok_core::run::RoundSpent)> {
        self.spent
            .lock()
            .map(|spent| spent.clone())
            .unwrap_or_default()
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

    async fn record_spent(
        &self,
        run_id: &str,
        events: &[Event],
        spent: &opengrok_core::run::RoundSpent,
    ) -> Result<(), JournalError> {
        self.record(run_id, events).await?;
        self.spent
            .lock()
            .map_err(|_| JournalError::Unwritable("the journal's lock was poisoned".to_string()))?
            .push((events.to_vec(), spent.clone()));
        Ok(())
    }
}
