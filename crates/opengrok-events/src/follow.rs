//! One stream: a resume or a `reset`, then the outbox followed.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use futures::stream::{self, BoxStream, StreamExt};
use opengrok_wire::events::{PING, RESET, THREAD_CHANGED, block};
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::time::{Instant, Interval, MissedTickBehavior, interval_at, sleep_until};

use crate::hub::Hub;
use crate::outbox::{self, Stored};

/// Notes one read takes, so a long replay is a series of pages and not a heap of frames: a client
/// that reads slowly holds one page in memory, however much it has missed.
const PAGE: i64 = 500;

/// How long to wait before reading again after the outbox could not be read.
const RETRY: Duration = Duration::from_secs(1);

impl Hub {
    /// The blocks of `account`'s events, as SSE text, for `GET /ag-ui/events`.
    ///
    /// `last_event_id` is the header's text. A valid one (a number from the oldest id retention
    /// still holds to the account's head) is replayed from, in order, and then followed with no
    /// gap and no repeat. Anything else, or none, is a fresh start: the first block is `reset`,
    /// carrying the head as its id, and the stream follows from there. A connection that has never
    /// heard from the server and one that heard too long ago are treated alike.
    ///
    /// Only `account`'s notes are ever read: the account is the one argument that reaches the SQL.
    ///
    /// The stream ends only if the process does; a client that goes away drops it, and with it the
    /// seat in the account's room. An outbox that cannot be read now is read again soon, and does
    /// not end the stream.
    pub async fn follow(
        &self,
        account: &str,
        last_event_id: Option<&str>,
    ) -> Result<BoxStream<'static, String>, sqlx::Error> {
        let wakes = self.room(account);
        self.listen();
        let (head, floor) = outbox::bounds(self.pool(), account).await?;
        let id = last_event_id.and_then(|id| id.trim().parse::<i64>().ok());
        let resume = id.filter(|id| (floor..=head).contains(id));
        let tuning = self.tuning();
        let mut follow = Follow {
            hub: self.clone(),
            account: account.to_string(),
            wakes,
            cursor: resume.unwrap_or(head),
            ready: VecDeque::new(),
            dirty: resume.is_some(),
            replaying: resume.is_some(),
            gate: Instant::now(),
            window: tuning.window,
            ping: ticker(tuning.ping),
            poll: ticker(tuning.poll),
        };
        if resume.is_none() {
            follow.ready.push_back(block(head, RESET, "{}"));
        }
        let frames = stream::unfold(follow, |mut follow| async move {
            let frame = follow.next().await?;
            Some((frame, follow))
        });
        Ok(frames.boxed())
    }
}

fn ticker(period: Duration) -> Interval {
    let period = period.max(Duration::from_millis(1));
    let mut ticker = interval_at(Instant::now() + period, period);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    ticker
}

struct Follow {
    hub: Hub,
    account: String,
    wakes: broadcast::Receiver<i64>,
    /// The highest id this stream has read; the next read is everything after it.
    cursor: i64,
    /// Blocks read and not yet sent: at most a page.
    ready: VecDeque<String>,
    /// A wake or a poll says the outbox may hold more than the cursor has.
    dirty: bool,
    /// Still catching up on an id the client resumed from, which is replayed note for note.
    replaying: bool,
    /// The earliest the outbox is read again, once it has been read caught up.
    gate: Instant,
    window: Duration,
    ping: Interval,
    poll: Interval,
}

impl Follow {
    async fn next(&mut self) -> Option<String> {
        loop {
            if let Some(frame) = self.ready.pop_front() {
                return Some(frame);
            }
            if self.dirty && Instant::now() >= self.gate {
                self.read().await;
                continue;
            }
            tokio::select! {
                woke = self.wakes.recv() => match woke {
                    Ok(id) => self.dirty |= id > self.cursor,
                    Err(RecvError::Lagged(_)) => self.lagged().await?,
                    Err(RecvError::Closed) => return None,
                },
                () = sleep_until(self.gate), if self.dirty => {}
                _ = self.poll.tick() => self.dirty = true,
                _ = self.ping.tick() => return Some(PING.to_string()),
            }
        }
    }

    /// Read the next page after the cursor. Once caught up, the next read waits out the window, so
    /// a burst of notes is read together; while a page comes back full there is more, and no wait.
    async fn read(&mut self) {
        let replay = self.replaying;
        self.dirty = false;
        let page = outbox::page(self.hub.pool(), &self.account, self.cursor, PAGE).await;
        let now = Instant::now();
        let notes = match page {
            Ok(notes) => notes,
            Err(error) => {
                tracing::warn!(%error, "the events outbox could not be read; reading again soon");
                (self.dirty, self.gate) = (true, now + RETRY);
                return;
            }
        };
        let more = notes.len() as i64 == PAGE;
        self.replaying = replay && more;
        (self.dirty, self.gate) = (more, if more { now } else { now + self.window });
        if let Some(last) = notes.last() {
            self.cursor = last.id;
        }
        let notes = if replay { notes } else { coalesced(notes) };
        let blocks = notes
            .iter()
            .map(|note| block(note.id, &note.kind, &note.payload.to_string()));
        self.ready.extend(blocks);
    }

    /// The room dropped wakes this stream was too slow to take. It is told to forget what it holds
    /// and read everything again, and what it had not yet been sent is not sent: that read covers
    /// it. `None` ends the stream when the head cannot be read, and the client reconnects.
    async fn lagged(&mut self) -> Option<()> {
        let (head, _) = outbox::bounds(self.hub.pool(), &self.account).await.ok()?;
        self.ready.clear();
        self.ready.push_back(block(head, RESET, "{}"));
        (self.cursor, self.dirty, self.replaying) = (head, false, false);
        Some(())
    }
}

/// Of the `thread.changed` notes one live read found for a thread, only the last. The app reads
/// the thread again either way, and the last says where it must have got to. Nothing else is
/// merged, and a replay is not: it is every note, in order.
fn coalesced(notes: Vec<Stored>) -> Vec<Stored> {
    let key = |note: &Stored| {
        let word = |name: &str| note.payload[name].as_str().unwrap_or_default().to_string();
        (word("threadId"), word("coworkerId"))
    };
    let thread_changed = |note: &&Stored| note.kind == THREAD_CHANGED;
    let last: HashMap<_, _> = notes
        .iter()
        .filter(thread_changed)
        .map(|n| (key(n), n.id))
        .collect();
    let keep =
        |note: &Stored| note.kind != THREAD_CHANGED || last.get(&key(note)) == Some(&note.id);
    notes.into_iter().filter(keep).collect()
}
