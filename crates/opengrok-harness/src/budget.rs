//! What one run may spend, and the clocks that hold a model to it.
//!
//! THE ROUND CAPS USED TO BE THE ONLY BOUND (#93). Nothing timed a model call, so a provider that
//! accepted a request and went quiet held the run open for as long as the process lived — and
//! `recovery::hold` renewed its lease the whole time, so the sweep never reclaimed it either. The
//! Stop button could not reach it: Stop is read between rounds, and the round never ended.

use std::time::Duration;

use futures::StreamExt;

use crate::model::{DeltaStream, ModelDelta, ModelDoor, ModelError, ModelRequest};

/// The limits one run is held to. `Default` is what every run gets unless its caller says
/// otherwise.
///
/// The round limits keep their meaning: rounds that ended in words or other work, and rounds
/// spent on the screen, counted apart because a desktop task is a dozen looks before a sentence.
/// When either runs out — or the wall clock does — the run makes one last call with no tools
/// and finishes with the model's own account of what it did, rather than a RUN_ERROR that says
/// only that a limit was reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct RunBudget {
    /// Model calls that ended in words or in work other than the screen.
    pub max_rounds: usize,
    /// Model calls spent looking at and acting on the box's screen.
    pub max_computer_rounds: usize,
    /// Wall clock for the whole run. Read at the top of each round, so a tool already running
    /// (a recipe on the box) finishes; the next one does not start.
    pub max_wall_ms: u64,
    /// How long a model call may take to start answering.
    pub call_timeout_ms: u64,
    /// How long a started answer may go without a single delta.
    ///
    /// LONGER THAN THE GATEWAY'S OWN IDLE LIMIT (180 s, `stream_idle` 504), on purpose. The
    /// gateway's keep-alives every 10 s never become a delta, so a reasoning model that thinks
    /// silently for two minutes looks idle from here; the gateway, which can tell, answers first.
    /// This clock is for the gateway that cannot answer at all, and for doors that are not it.
    pub idle_ms: u64,
}

impl Default for RunBudget {
    fn default() -> Self {
        Self {
            max_rounds: crate::MAX_ROUNDS,
            max_computer_rounds: crate::MAX_COMPUTER_ROUNDS,
            max_wall_ms: 15 * 60 * 1000,
            call_timeout_ms: 180_000,
            idle_ms: 240_000,
        }
    }
}

impl RunBudget {
    pub(crate) fn max_wall(&self) -> Duration {
        Duration::from_millis(self.max_wall_ms)
    }

    /// Open the door's stream, or say why it did not open in time.
    pub(crate) async fn open(
        &self,
        door: &dyn ModelDoor,
        request: ModelRequest,
    ) -> Result<DeltaStream, ModelError> {
        let limit = Duration::from_millis(self.call_timeout_ms);
        tokio::time::timeout(limit, door.stream(request))
            .await
            .unwrap_or_else(|_| {
                Err(ModelError::TimedOut(format!(
                    "the model did not start answering within {}",
                    spoken(limit)
                )))
            })
    }

    /// The next delta, or a timeout the loop ends the run with.
    pub(crate) async fn next(
        &self,
        stream: &mut DeltaStream,
    ) -> Option<Result<ModelDelta, ModelError>> {
        let limit = Duration::from_millis(self.idle_ms);
        tokio::time::timeout(limit, stream.next())
            .await
            .unwrap_or_else(|_| {
                Some(Err(ModelError::TimedOut(format!(
                    "the model stopped answering for {}",
                    spoken(limit)
                ))))
            })
    }
}

/// A duration as a person reads it: seconds under two minutes, minutes above, milliseconds only
/// for the short limits tests set.
pub(crate) fn spoken(duration: Duration) -> String {
    let ms = duration.as_millis();
    match ms {
        0..1_000 => format!("{ms} ms"),
        1_000..120_000 => format!("{} seconds", ms / 1_000),
        _ => format!("{} minutes", ms / 60_000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_limit_reads_as_a_person_would_say_it() {
        assert_eq!(spoken(Duration::from_millis(100)), "100 ms");
        assert_eq!(spoken(Duration::from_secs(90)), "90 seconds");
        assert_eq!(spoken(Duration::from_secs(15 * 60)), "15 minutes");
    }
}
