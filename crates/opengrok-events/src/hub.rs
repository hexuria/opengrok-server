//! A process's hold on the outbox: one listening connection, and a room per account that has a
//! stream open, which the listener wakes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::{PgListener, PgPoolOptions};
use tokio::sync::broadcast;

/// The one channel every note is announced on; its payload is `<account>:<id>`.
pub const CHANNEL: &str = "opengrok_events";

/// A wake that means "read the outbox", whatever your cursor: the listener lost its connection, so
/// notes may have been announced into the gap. Larger than any id.
const RESYNC: i64 = i64::MAX;

/// How long the listener waits before it connects again.
const RECONNECT: Duration = Duration::from_secs(2);

/// The clocks and the sizes a stream runs on, which a test shortens.
#[derive(Debug, Clone, Copy)]
pub struct Tuning {
    /// A `: ping` comment this often when nothing else was sent.
    pub ping: Duration,
    /// The outbox is read this often whether or not a wake came: a lost NOTIFY costs this much.
    pub poll: Duration,
    /// At most one read this often once caught up, so a burst of `thread.changed` for one thread
    /// leaves as one: about two a second.
    pub window: Duration,
    /// How many wakes a room holds for a stream that is not reading. Past it the stream is told
    /// `reset`, which is how a slow client costs the server nothing.
    pub room: usize,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            ping: Duration::from_secs(15),
            poll: Duration::from_secs(30),
            window: Duration::from_millis(500),
            room: 64,
        }
    }
}

/// What a process holds of the outbox. Cheap to clone: the store carries one, and every stream
/// opened through it shares the same listening connection.
#[derive(Debug, Clone)]
pub struct Hub(Arc<Shared>);

#[derive(Debug)]
struct Shared {
    pool: PgPool,
    tuning: Mutex<Tuning>,
    /// An open stream's way to hear its account's notes land: the highest id announced. Only an
    /// account with a stream open has a room; the listener drops one nobody holds.
    rooms: Mutex<HashMap<String, broadcast::Sender<i64>>>,
    listening: AtomicBool,
}

impl Hub {
    /// Holds nothing until a stream opens: no connection, and no task, so this is safe wherever a
    /// store is built, runtime or not.
    pub fn new(pool: PgPool) -> Self {
        Self(Arc::new(Shared {
            pool,
            tuning: Mutex::new(Tuning::default()),
            rooms: Mutex::new(HashMap::new()),
            listening: AtomicBool::new(false),
        }))
    }

    /// Change the clocks for streams opened from now on, and for rooms made from now on.
    pub fn tune(&self, tuning: Tuning) {
        *self.0.tuning.lock().unwrap_or_else(PoisonError::into_inner) = tuning;
    }

    pub(crate) fn tuning(&self) -> Tuning {
        *self.0.tuning.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn pool(&self) -> &PgPool {
        &self.0.pool
    }

    fn rooms(&self) -> MutexGuard<'_, HashMap<String, broadcast::Sender<i64>>> {
        self.0.rooms.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A seat in the account's room, taken BEFORE the stream reads anything: a wake that arrives
    /// while it reads waits for it here, so no note falls between the read and the listening.
    pub(crate) fn room(&self, account: &str) -> broadcast::Receiver<i64> {
        let capacity = self.tuning().room.max(1);
        let mut rooms = self.rooms();
        let room = rooms.entry(account.to_string());
        room.or_insert_with(|| broadcast::channel(capacity).0)
            .subscribe()
    }

    fn wake(&self, account: &str, id: i64) {
        let mut rooms = self.rooms();
        if rooms
            .get(account)
            .is_some_and(|room| room.send(id).is_err())
        {
            rooms.remove(account);
        }
    }

    fn wake_all(&self) {
        self.rooms().retain(|_, room| room.send(RESYNC).is_ok());
    }

    /// Start listening, once per process (per store). Safe to call on every stream.
    pub(crate) fn listen(&self) {
        if !self.0.listening.swap(true, Ordering::AcqRel) {
            tokio::spawn(self.clone().listen_forever());
        }
    }

    async fn listen_forever(self) {
        loop {
            if let Err(error) = self.pump().await {
                tracing::warn!(%error, "the events listener stopped; connecting again");
            }
            tokio::time::sleep(RECONNECT).await;
            self.wake_all();
        }
    }

    /// One listening connection, kept apart from the pool: the pool's connections are the
    /// requests', and this one is held for as long as the process lives.
    async fn pump(&self) -> Result<(), sqlx::Error> {
        let own = PgPoolOptions::new()
            .max_connections(1)
            .max_lifetime(None)
            .idle_timeout(None);
        let own = own.connect_with((*self.pool().connect_options()).clone());
        let own = own.await?;
        let mut listener = PgListener::connect_with(&own).await?;
        listener.listen(CHANNEL).await?;
        // Anything announced before the LISTEN took hold is in the outbox and not in a room.
        self.wake_all();
        loop {
            match listener.try_recv().await? {
                Some(note) => {
                    if let Some((account, id)) = note.payload().rsplit_once(':')
                        && let Ok(id) = id.parse()
                    {
                        self.wake(account, id);
                    }
                }
                // The connection dropped and came back; what was announced meanwhile was not heard.
                None => self.wake_all(),
            }
        }
    }
}
