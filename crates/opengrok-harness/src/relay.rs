//! The Mac relay (#292): a person's own subscription reached through their own Mac, which holds
//! a stream open to this server (`GET /inference-relay/requests`) and carries each call to the
//! opencodex it runs, answering on `POST /inference-relay/responses/{requestId}`. The routes are
//! the server's (`opengrok-server/src/inference.rs`); this is the broker they share with the
//! door, and the door half (`ModelEndpoint::Relay`).
//!
//! LOCAL-EXEC'S SHAPE, FOR A MODEL CALL (`opengrok-server/src/local_exec/broker.rs`): the Mac
//! signs in with the daemon token local-exec enrols it with; one stream per machine, and the
//! latest connect wins; a request goes down one machine's stream under an id only that machine
//! may answer; a caller that gives up tells the machine to cancel. What differs is the answer: a
//! body piped into the run as it arrives rather than one result, and a replaced stream is told so
//! (`replaced`) and closed, where local-exec's is dropped.
//!
//! WHICH MAC: the account's machine that most recently opened its stream and still holds it.
//! Opening it IS the machine saying it can carry calls. A call is only ever sent under the
//! account whose turn it is, so only the account's own machines serve, and only its own turns.
//!
//! THE DOOR KEEPS ITS OWN CLOCKS: a Mac that does not start answering within `FIRST_BYTE`, or goes
//! quiet for `IDLE`, ends the call with `relay_timeout` and is told to cancel; a Stop ends it
//! where it is. The loop above only ever awaits a stream, as it does for the gateway.
//!
//! PER REPLICA, like local-exec's broker: a turn is carried only by a Mac whose stream this
//! process holds, and a Stop reaches a call only through the replica that holds its stream.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures::{Stream, StreamExt};
use opengrok_wire::relay::RelayFrame;
use serde_json::Value;
use tokio::sync::{Notify, mpsc};

use crate::model::{DeltaStream, ModelError, ModelRequest, bounded};

/// The most calls one Mac may carry at once. A run makes one at a time; this is a person with a
/// few conversations going, not a machine asked to carry anybody's load.
pub const MAX_IN_FLIGHT: usize = 16;

/// The most one answer may be. A long streamed reply is a few megabytes of SSE.
pub const MAX_ANSWER_BYTES: usize = 32 * 1024 * 1024;

/// Answered ids remembered, so a second answer is told it came twice (409) and not that the
/// request is unknown (404).
const REMEMBERED: usize = 1024;

/// The broker's clocks. `Default` is the contract's; a test shortens them.
#[derive(Debug, Clone, Copy)]
pub struct Clocks {
    /// How long the Mac may take to start answering a call.
    pub first_byte: Duration,
    /// How long a started answer may go without a byte.
    pub idle: Duration,
    /// How long `/models` waits for a Mac's list: a picker asks on every read.
    pub listing: Duration,
    /// How often a quiet stream says it is alive.
    pub ping: Duration,
}

impl Default for Clocks {
    fn default() -> Self {
        Self {
            first_byte: Duration::from_secs(60),
            idle: Duration::from_secs(60),
            listing: Duration::from_secs(3),
            ping: Duration::from_secs(15),
        }
    }
}

/// The meeting point of the Macs' streams and the calls waiting on them. All state is behind one
/// lock held only for map operations, never across an await, so a `Drop` can take it.
#[derive(Default)]
pub struct RelayBroker {
    state: Mutex<State>,
    clocks: Clocks,
}

/// A machine as its daemon token names it: the account it is enrolled to, and its id. Keyed by
/// both, since a machine id is the client's to choose at enrolment and two accounts may pick the
/// same one: neither may replace the other's stream or answer the other's calls.
type Machine = (String, String);

#[derive(Default)]
struct State {
    /// Bumped on every connect: which of an account's streams is the latest.
    opened: u64,
    macs: HashMap<Machine, Mac>,
    /// request id → the call waiting on it.
    asks: HashMap<String, Ask>,
    /// (request id, machine) of recent answers, oldest first.
    answered: VecDeque<(String, Machine)>,
    /// (account, thread) of each thread whose held sends are being sent, and whether another
    /// trigger came for it meanwhile (`draining`).
    draining: HashMap<(String, String), bool>,
}

struct Mac {
    opened: u64,
    frames: mpsc::UnboundedSender<RelayFrame>,
}

struct Ask {
    machine: Machine,
    /// The run a turn's call is for; `None` for a model list.
    run_id: Option<String>,
    /// Where the answer goes: taken by the one POST that answers it.
    answer: Option<mpsc::Sender<Piece>>,
    /// Woken when the run is stopped.
    stop: Arc<Notify>,
    /// The Mac was already told to cancel it.
    cancelled: bool,
}

/// A piece of a Mac's answer, as the door reads it.
enum Piece {
    Bytes(Vec<u8>),
    Json(Value),
    Failed(String),
}

/// Why an answer is refused: an id this server did not send or no longer waits on (404), one
/// already answered (409), or one sent to another machine (401).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    Unknown,
    Answered,
    NotYours,
}

/// How a piped answer ended: taken whole, or cut off past `MAX_ANSWER_BYTES`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Piped {
    Accepted,
    TooLarge,
}

/// Where a relayed turn's calls go (`ModelEndpoint::Relay`): the account whose Mac carries them,
/// and the run they are for. A Mac is picked per call, so a Mac that reconnects mid-run serves
/// the rest of it and one that leaves is `relay_offline` at the next call.
#[derive(Clone)]
pub struct RelayTo {
    pub broker: Arc<RelayBroker>,
    pub account: String,
    pub run_id: String,
}

impl PartialEq for RelayTo {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.broker, &other.broker)
            && (&self.account, &self.run_id) == (&other.account, &other.run_id)
    }
}

impl Eq for RelayTo {}

impl std::fmt::Debug for RelayTo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RelayTo({}, run {})", self.account, self.run_id)
    }
}

fn relay(code: &'static str, sentence: impl Into<String>) -> ModelError {
    ModelError::Relay {
        code,
        sentence: sentence.into(),
    }
}

impl RelayBroker {
    pub fn new(clocks: Clocks) -> Self {
        Self {
            clocks,
            ..Self::default()
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A Mac opened its stream: the frames it is sent, each the JSON of one SSE event, `ready`
    /// first, then whatever is asked of it and a ping every `Clocks::ping`, until it hangs up or
    /// another stream from the same machine replaces it, which it is told (`replaced`) first.
    pub fn connect(
        &self,
        account: &str,
        machine: &str,
    ) -> impl Stream<Item = String> + Send + use<> {
        let (frames, rx) = mpsc::unbounded_channel();
        let ready = RelayFrame::Ready {
            machine_id: machine.to_string(),
        };
        let _ = frames.send(ready);
        let replaced = {
            let mut state = self.lock();
            state.opened += 1;
            let mac = Mac {
                opened: state.opened,
                frames,
            };
            let key = (account.to_string(), machine.to_string());
            state.macs.insert(key, mac)
        };
        if let Some(old) = replaced {
            let _ = old.frames.send(RelayFrame::Replaced);
        }
        let every = self.clocks.ping;
        let tick = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
        futures::stream::unfold((rx, tick), |(mut rx, mut tick)| async move {
            let frame = tokio::select! {
                frame = rx.recv() => frame?,
                _ = tick.tick() => RelayFrame::Ping,
            };
            Some((frame.to_json(), (rx, tick)))
        })
    }

    /// The account's Mac: its machine that most recently opened its stream and still holds it.
    pub fn connected(&self, account: &str) -> Option<String> {
        let state = self.lock();
        latest(&state, account).map(|((_, machine), _)| machine.clone())
    }

    /// A machine's daemon token was revoked, or rotated by a re-enrolment: its stream ends now,
    /// so it is sent no more turns. Its answers were refused already (the token is checked on
    /// every POST).
    pub fn disconnect(&self, account: &str, machine: &str) {
        let key = (account.to_string(), machine.to_string());
        self.lock().macs.remove(&key);
    }

    /// Claim the sending of `account`'s held sends on `thread` (`opengrok-server`'s
    /// `pending::drain_held`). ONE AT A TIME PER THREAD, AND NO TRIGGER LOST TO IT: false when
    /// one is running, which is told to go round again (`drained`), as it may be past the read
    /// that would have seen what this trigger saw (`formal/tla/HeldSend.tla`, `ReArms`).
    pub fn draining(&self, account: &str, thread: &str) -> bool {
        let key = (account.to_string(), thread.to_string());
        let mut state = self.lock();
        let running = state
            .draining
            .get_mut(&key)
            .map(|again| std::mem::replace(again, true));
        running.is_none() && state.draining.insert(key, false).is_none()
    }

    /// The sending `draining` claimed is done: true when another trigger came meanwhile, and it
    /// must go round again; otherwise the claim is let go.
    pub fn drained(&self, account: &str, thread: &str) -> bool {
        let key = (account.to_string(), thread.to_string());
        let mut state = self.lock();
        let again = state.draining.remove(&key) == Some(true);
        if again {
            state.draining.insert(key, false);
        }
        again
    }

    /// Send `frame` to the account's Mac under a fresh, unguessable id, and register where its
    /// answer goes.
    fn ask(
        self: &Arc<Self>,
        account: &str,
        run_id: Option<&str>,
        frame: impl FnOnce(String) -> RelayFrame,
    ) -> Result<Call, ModelError> {
        let offline = || {
            relay(
                "relay_offline",
                "Your Mac isn't connected, so your plan can't answer this turn. Connect it, or \
                 switch this turn to the gateway.",
            )
        };
        let (answer, pieces) = mpsc::channel(32);
        let stop = Arc::new(Notify::new());
        let mut state = self.lock();
        let (machine, sent) = {
            let (machine, mac) = latest(&state, account).ok_or_else(offline)?;
            let busy = state.asks.values().filter(|ask| ask.machine == *machine);
            if busy.count() >= MAX_IN_FLIGHT {
                return Err(ModelError::Proxy(format!(
                    "Your Mac is already carrying {MAX_IN_FLIGHT} calls, so this one was not \
                     sent; try again when one finishes."
                )));
            }
            let id = uuid::Uuid::new_v4().to_string();
            let sent = mac.frames.send(frame(id.clone())).map(|()| id);
            (machine.clone(), sent.map_err(|_| offline())?)
        };
        let ask = Ask {
            machine,
            run_id: run_id.map(str::to_string),
            answer: Some(answer),
            stop: stop.clone(),
            cancelled: false,
        };
        state.asks.insert(sent.clone(), ask);
        Ok(Call {
            broker: self.clone(),
            id: sent,
            pieces,
            stop,
            finished: false,
        })
    }

    /// Let a call go. When the Mac may still be working on it, it is told to cancel — once.
    fn forget(&self, id: &str, cancel: bool) {
        let mut state = self.lock();
        let Some(ask) = state.asks.remove(id) else {
            return;
        };
        if cancel && !ask.cancelled {
            cancel_at(&state.macs, &ask.machine, id);
        }
    }

    /// A person stopped `run_id`: each call it has at a Mac is cancelled there, and its door's
    /// stream ends where it is, for the loop's own stop check to end the run.
    pub fn stop(&self, run_id: &str) {
        let mut state = self.lock();
        let State { asks, macs, .. } = &mut *state;
        for (id, ask) in asks.iter_mut() {
            if ask.run_id.as_deref() == Some(run_id) {
                ask.stop.notify_one();
                if !std::mem::replace(&mut ask.cancelled, true) {
                    cancel_at(macs, &ask.machine, id);
                }
            }
        }
    }

    /// `account`'s machine `machine` answers `request_id`. Only the machine it was sent to may,
    /// and only once: a relayed answer is only ever piped into the call that asked.
    pub fn answer(
        &self,
        account: &str,
        machine: &str,
        request_id: &str,
    ) -> Result<Answering, Refused> {
        let by = (account.to_string(), machine.to_string());
        let mut state = self.lock();
        let taken = match state.asks.get_mut(request_id) {
            Some(ask) if ask.machine != by => return Err(Refused::NotYours),
            Some(ask) => ask.answer.take().ok_or(Refused::Answered)?,
            None => {
                let seen = state.answered.iter().find(|(id, _)| id == request_id);
                return Err(match seen {
                    Some((_, to)) if *to != by => Refused::NotYours,
                    Some(_) => Refused::Answered,
                    None => Refused::Unknown,
                });
            }
        };
        state.answered.push_back((request_id.to_string(), by));
        if state.answered.len() > REMEMBERED {
            state.answered.pop_front();
        }
        Ok(Answering { answer: taken })
    }

    /// A turn's model call, carried by the account's Mac. The model is held to the allowlist the
    /// loopback door holds it to, here, whatever built the request (a resume, a judge, a wrap-up).
    ///
    /// OPENED WHEN THE MAC STARTS ANSWERING, as an HTTP door opens on its headers: a Mac that is
    /// not connected, never answers or says why it cannot is the door not opening, before the
    /// turn has shown a word.
    pub(crate) async fn stream(
        self: &Arc<Self>,
        to: &RelayTo,
        request: &ModelRequest,
    ) -> Result<DeltaStream, ModelError> {
        opengrok_core::inference::subscription_model(&request.model)
            .map_err(|why| ModelError::Proxy(format!("This turn was not sent: {why}.")))?;
        // NO URL AND NO KEY (`RelayFrame`): the body the loopback door would POST, and nothing
        // about where it goes. The Mac's opencodex is at an address and a key entered on the Mac.
        let body = crate::gateway::chat_body(request);
        let (run_id, model) = (to.run_id.clone(), request.model.clone());
        let mut call = self.ask(&to.account, Some(&to.run_id), |request_id| {
            RelayFrame::Infer {
                request_id,
                run_id,
                model,
                request: body,
            }
        })?;
        let clocks = self.clocks;
        let first = call.bytes(clocks.first_byte, false).await?;
        let rest =
            futures::stream::unfold(first.is_some().then_some(call), move |call| async move {
                let mut call = call?;
                match call.bytes(clocks.idle, true).await.transpose()? {
                    Ok(bytes) => Some((Ok(bytes), Some(call))),
                    Err(error) => Some((Err(error), None)),
                }
            });
        Ok(crate::gateway::deltas_from(
            futures::stream::iter(first.map(Ok)).chain(rest),
        ))
    }

    /// The models the account's Mac says its opencodex serves that a subscription may use
    /// (`local_proxy::allowed`). None when no Mac is connected, or it does not answer within
    /// `Clocks::listing`: a list is never an error, and never waits long.
    pub async fn models(self: &Arc<Self>, account: &str) -> Vec<opengrok_core::catalogue::Model> {
        let asked = self.ask(account, None, |request_id| RelayFrame::Models {
            request_id,
        });
        let Ok(mut call) = asked else {
            return Vec::new();
        };
        match tokio::time::timeout(self.clocks.listing, call.pieces.recv()).await {
            Ok(Some(Piece::Json(listed))) => {
                call.finished = true;
                crate::local_proxy::allowed(&listed)
            }
            _ => Vec::new(),
        }
    }
}

/// The account's machine whose stream opened last and is still held.
fn latest<'a>(state: &'a State, account: &str) -> Option<(&'a Machine, &'a Mac)> {
    let theirs = state.macs.iter().filter(|((owner, _), _)| owner == account);
    let live = theirs.filter(|(_, mac)| !mac.frames.is_closed());
    live.max_by_key(|(_, mac)| mac.opened)
}

fn cancel_at(macs: &HashMap<Machine, Mac>, machine: &Machine, id: &str) {
    if let Some(mac) = macs.get(machine) {
        let request_id = id.to_string();
        let _ = mac.frames.send(RelayFrame::Cancel { request_id });
    }
}

/// One call in flight, from the door's side. Dropped before its answer ended — the run gave up,
/// was stopped, or went on without it — it tells the Mac to cancel.
struct Call {
    broker: Arc<RelayBroker>,
    id: String,
    pieces: mpsc::Receiver<Piece>,
    stop: Arc<Notify>,
    finished: bool,
}

impl Call {
    /// The answer's next bytes; `None` once it has ended or the run was stopped. A Mac quiet past
    /// `limit` is `relay_timeout` and told to cancel; its own `{error}` is `relay_failed`, in its
    /// words, bounded like a proxy's.
    async fn bytes(
        &mut self,
        limit: Duration,
        started: bool,
    ) -> Result<Option<Vec<u8>>, ModelError> {
        let piece = tokio::select! {
            biased;
            () = self.stop.notified() => return Ok(None),
            piece = self.pieces.recv() => piece,
            () = tokio::time::sleep(limit) => {
                self.broker.forget(&self.id, true);
                let how = if started {
                    "stopped answering for"
                } else {
                    "did not start answering within"
                };
                let long = crate::budget::spoken(limit);
                return Err(relay(
                    "relay_timeout",
                    format!("Your Mac {how} {long}, so the turn stopped there; it was told to cancel."),
                ));
            }
        };
        let failed = match piece {
            Some(Piece::Bytes(bytes)) => return Ok(Some(bytes)),
            None => None,
            Some(Piece::Failed(why)) => Some(bounded(&why)),
            Some(Piece::Json(_)) => {
                Some("Your Mac answered with something other than a stream.".to_string())
            }
        };
        self.finished = true;
        failed.map_or(Ok(None), |why| Err(relay("relay_failed", why)))
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        self.broker.forget(&self.id, !self.finished);
    }
}

/// The one answer to a call, from the machine it went to, on its way into the run that asked.
pub struct Answering {
    answer: mpsc::Sender<Piece>,
}

impl Answering {
    /// Pipe the Mac's body in. `streamed` is opencodex's SSE, sent on as it arrives; anything else
    /// is JSON read whole: a model list, or `{"error": "<sentence>"}`, which reaches the run in the
    /// Mac's own words. Past `MAX_ANSWER_BYTES` the run is told so and the rest is not read.
    pub async fn pipe<S, B, E>(self, streamed: bool, body: S) -> Piped
    where
        S: Stream<Item = Result<B, E>>,
        B: AsRef<[u8]>,
    {
        let mut body = std::pin::pin!(body);
        let (mut seen, mut whole) = (0usize, Vec::new());
        while let Some(chunk) = body.next().await {
            let Ok(chunk) = chunk else {
                let why = "Your Mac's answer broke off before it ended.";
                let _ = self.answer.send(Piece::Failed(why.to_string())).await;
                return Piped::Accepted;
            };
            seen += chunk.as_ref().len();
            if seen > MAX_ANSWER_BYTES {
                let why = format!(
                    "Your Mac's answer passed {} MiB, so it was cut off there.",
                    MAX_ANSWER_BYTES >> 20
                );
                let _ = self.answer.send(Piece::Failed(why)).await;
                return Piped::TooLarge;
            }
            if !streamed {
                whole.extend_from_slice(chunk.as_ref());
            } else if self
                .answer
                .send(Piece::Bytes(chunk.as_ref().to_vec()))
                .await
                .is_err()
            {
                // The run is no longer listening: it gave up, or was stopped, and the Mac was
                // told to cancel. What it sent is taken, and the rest is not read.
                return Piped::Accepted;
            }
        }
        if !streamed {
            let piece = match serde_json::from_slice::<Value>(&whole) {
                Ok(said) => match said.get("error").and_then(Value::as_str) {
                    Some(why) => Piece::Failed(why.to_string()),
                    None => Piece::Json(said),
                },
                Err(_) => Piece::Failed("Your Mac's answer could not be read.".to_string()),
            };
            let _ = self.answer.send(piece).await;
        }
        Piped::Accepted
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "../tests/unit/relay_tests.rs"]
mod tests;
