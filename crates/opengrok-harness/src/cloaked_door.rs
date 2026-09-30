//! A door that swaps secrets for stand-ins on the way out and puts them back
//! on the way in.
//!
//! A coworker reads real files and runs real commands, so its transcript fills
//! up with real hostnames, real connection strings and real people. All of it
//! goes to the provider, because the model cannot reason about a log line it
//! has not been shown.
//!
//! This wraps another [`ModelDoor`] and rewrites what passes through it. Every
//! sensitive value is replaced by a stand-in of the same shape — a sixteen
//! digit card number stays a sixteen digit card number, a host stays a host —
//! so the model reasons normally, and the mapping is reversed on the way back
//! so the coworker acts on the real thing.
//!
//! # Why the door, and not four separate hooks
//!
//! Scrubbing an agent needs four things, and decorating this one trait gets
//! all four, because everything already flows through it:
//!
//! 1. **The prompt** is [`ModelRequest::messages`], scrubbed before the inner
//!    door sees it.
//! 2. **Tool results** re-enter the conversation as another message
//!    (`tool_result_message`), so the next round's scrub covers them with no
//!    extra hook. Rescrubbing an earlier round's messages is free: a value
//!    that already has a stand-in keeps it, and a stand-in is never scrubbed
//!    a second time.
//! 3. **Tool call arguments** are restored before they leave this stream, so
//!    the executor connects to the real host. This is the one that breaks an
//!    agent rather than leaking: the model was shown `cedar64.internal`, so it
//!    asks to ssh to `cedar64.internal`, and without this the coworker
//!    dutifully tries and fails against a host that does not exist.
//! 4. **The reply** is restored delta by delta, so the human reads real values.
//!
//! # Relationship to `scrub_secret_keys`
//!
//! [`crate::scrub_event_secrets`] and `opengrok_tools::credential::scrub_secret_keys`
//! do a different job and both are still needed. They *drop* values from what
//! the client and the journal see, by key name, one way. This *substitutes*
//! values in what the provider sees, by shape, reversibly. One protects the
//! transcript at rest; the other protects the third party.

use std::collections::VecDeque;
use std::sync::Arc;

use cred_swap_core::session::{Session, SessionError, SessionStore};
use futures::StreamExt as _;
use futures::stream::{self, Stream};
use sha2::{Digest, Sha256};

use crate::model::{DeltaStream, ModelDelta, ModelDoor, ModelError, ModelRequest};

/// Picks the conversation a request belongs to.
pub type KeyFn = dyn Fn(&ModelRequest) -> String + Send + Sync;

/// Wraps a [`ModelDoor`] so nothing sensitive reaches the provider verbatim.
pub struct CloakedDoor {
    inner: Arc<dyn ModelDoor>,
    sessions: SessionStore,
    key: Arc<KeyFn>,
}

impl std::fmt::Debug for CloakedDoor {
    /// Names the shape, never the contents. The session store holds every real
    /// value next to its stand-in, which is the thing being protected.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloakedDoor")
            .field("sessions", &self.sessions)
            .finish_non_exhaustive()
    }
}

impl CloakedDoor {
    /// Wrap a door, keying conversations the way the gateway's spend pin does.
    pub fn new(inner: Arc<dyn ModelDoor>, sessions: SessionStore) -> Self {
        Self {
            inner,
            sessions,
            key: Arc::new(default_key),
        }
    }

    /// Use a different notion of "one conversation".
    ///
    /// The default follows `conversation_pin`: a coworker two people share
    /// holds two transcripts, so it is two conversations. Supply your own when
    /// you have a better key to hand.
    #[must_use]
    pub fn keyed_by(
        mut self,
        key: impl Fn(&ModelRequest) -> String + Send + Sync + 'static,
    ) -> Self {
        self.key = Arc::new(key);
        self
    }

    /// The session a request belongs to.
    fn session_for(&self, request: &ModelRequest) -> Result<Session, ModelError> {
        self.sessions
            .session((self.key)(request))
            .map_err(as_model_error)
    }
}

#[async_trait::async_trait]
impl ModelDoor for CloakedDoor {
    async fn stream(&self, mut request: ModelRequest) -> Result<DeltaStream, ModelError> {
        let session = self.session_for(&request)?;

        if let Some(system) = request.system.take() {
            request.system = Some(session.scrub(&system).map_err(as_model_error)?.text);
        }
        for message in &mut request.messages {
            message.content = session
                .scrub(&message.content)
                .map_err(as_model_error)?
                .text;
            // A call's arguments are the model's own words from an earlier round, restored to the
            // real values on the way in (3. below) — so they leave again with the real host in
            // them unless they are scrubbed like any other message.
            for call in &mut message.tool_calls {
                call.arguments = session.scrub(&call.arguments).map_err(as_model_error)?.text;
            }
        }

        // `tools` is the schema the model is offered, not conversation
        // content. Rewriting a tool name or a parameter name there would make
        // the model ask for a tool that does not exist.

        let inner = self.inner.stream(request).await?;
        Ok(Box::pin(restoring(inner, session)))
    }

    async fn ready(&self) -> Option<Result<(), ModelError>> {
        self.inner.ready().await
    }
}

/// The conversation key, mirroring the gateway's spend pin.
fn default_key(request: &ModelRequest) -> String {
    match (&request.spend_scope, &request.spend_actor) {
        // Before spend caps exist there is nothing on the request that names
        // the conversation, so fall back to the opening message, which does
        // not change as a conversation grows. Two conversations that open with
        // the same words share a vault; that is a weaker guarantee than the
        // pin, and it is why a host that knows its own run ids should pass
        // `keyed_by`.
        (None, None) => format!("open:{}", opening_digest(request)),
        (scope, actor) => format!(
            "pin:{}:{}",
            scope.as_deref().unwrap_or("-"),
            actor.as_deref().unwrap_or("-")
        ),
    }
}

fn opening_digest(request: &ModelRequest) -> String {
    use std::fmt::Write as _;
    let mut hasher = Sha256::new();
    hasher.update(b"opengrok/cloaked-door/opening/v1");
    if let Some(first) = request.messages.first() {
        hasher.update(first.role.as_bytes());
        hasher.update([0]);
        // Bounded: the opening message is what identifies the conversation,
        // and hashing a megabyte of pasted log on every request is waste.
        let content = first.content.as_bytes();
        hasher.update(&content[..content.len().min(4096)]);
    }
    hasher
        .finalize()
        .iter()
        .take(12)
        .fold(String::new(), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn as_model_error(error: SessionError) -> ModelError {
    // There is no "the turn could not be prepared" variant, and failing open
    // would send the real values. Refusing the turn is the safe direction.
    ModelError::Stream(format!("cred-swap could not rewrite the turn: {error}"))
}

/// Restore stand-ins in a delta stream.
fn restoring(
    inner: DeltaStream,
    session: Session,
) -> impl Stream<Item = Result<ModelDelta, ModelError>> + Send {
    let start = (inner, Restorer::new(session), VecDeque::new(), false);

    stream::unfold(
        start,
        |(mut inner, mut restorer, mut queue, mut ended)| async move {
            loop {
                if let Some(item) = queue.pop_front() {
                    return Some((item, (inner, restorer, queue, ended)));
                }
                if ended {
                    return None;
                }
                match inner.next().await {
                    Some(Ok(delta)) => restorer.push(delta, &mut queue),
                    Some(Err(error)) => {
                        // Emit what is held before the error, so a partial
                        // answer is not lost along with it.
                        restorer.flush(&mut queue);
                        queue.push_back(Err(error));
                        ended = true;
                    }
                    None => {
                        restorer.flush(&mut queue);
                        ended = true;
                    }
                }
            }
        },
    )
}

/// Reassembles stand-ins that arrive split across deltas.
///
/// A stand-in is a word; a stream delivers words a few characters at a time.
/// Matching one delta at a time would never see a whole stand-in, so text is
/// held until enough of it has arrived to be sure, and tool arguments are held
/// until the call closes, since they are only parsed once anyway.
struct Restorer {
    session: Session,
    text: String,
    reasoning: String,
    /// In arrival order, so a flush emits calls the way they came.
    arguments: Vec<(String, String)>,
}

impl Restorer {
    fn new(session: Session) -> Self {
        Self {
            session,
            text: String::new(),
            reasoning: String::new(),
            arguments: Vec::new(),
        }
    }

    fn push(&mut self, delta: ModelDelta, out: &mut VecDeque<Result<ModelDelta, ModelError>>) {
        match delta {
            ModelDelta::Text(fragment) => self.stream_words(&fragment, Kind::Text, out),
            ModelDelta::Reasoning(fragment) => self.stream_words(&fragment, Kind::Reasoning, out),

            ModelDelta::ToolCallStart { id, name } => {
                // A tool call ends the run of text before it.
                self.flush_words(out);
                self.arguments.push((id.clone(), String::new()));
                out.push_back(Ok(ModelDelta::ToolCallStart { id, name }));
            }

            ModelDelta::ToolCallArgs { id, delta } => {
                match self.arguments.iter_mut().find(|(held, _)| *held == id) {
                    Some((_, held)) => held.push_str(&delta),
                    // Arguments without a start: keep them rather than drop
                    // them, and let the projection decide what that means.
                    None => self.arguments.push((id, delta)),
                }
            }

            ModelDelta::ToolCallEnd { id } => {
                self.emit_arguments(&id, out);
                out.push_back(Ok(ModelDelta::ToolCallEnd { id }));
            }
        }
    }

    /// Emit as much of the held text as is certainly complete.
    fn stream_words(
        &mut self,
        fragment: &str,
        kind: Kind,
        out: &mut VecDeque<Result<ModelDelta, ModelError>>,
    ) {
        let mut held = kind.take(self);
        held.push_str(fragment);

        match self.session.restore_streaming(&held) {
            Ok((ready, consumed)) => {
                held.drain(..consumed);
                if !ready.is_empty() {
                    out.push_back(Ok(kind.delta(ready)));
                }
            }
            Err(error) => out.push_back(Err(as_model_error(error))),
        }

        kind.put(self, held);
    }

    /// Emit everything still held, restored whole.
    fn flush(&mut self, out: &mut VecDeque<Result<ModelDelta, ModelError>>) {
        self.flush_words(out);
        let pending: Vec<String> = self.arguments.iter().map(|(id, _)| id.clone()).collect();
        for id in pending {
            self.emit_arguments(&id, out);
        }
    }

    fn flush_words(&mut self, out: &mut VecDeque<Result<ModelDelta, ModelError>>) {
        for kind in [Kind::Text, Kind::Reasoning] {
            let held = kind.take(self);
            if held.is_empty() {
                continue;
            }
            match self.session.restore(&held) {
                Ok(restored) if !restored.is_empty() => out.push_back(Ok(kind.delta(restored))),
                Ok(_) => {}
                Err(error) => out.push_back(Err(as_model_error(error))),
            }
        }
    }

    fn emit_arguments(&mut self, id: &str, out: &mut VecDeque<Result<ModelDelta, ModelError>>) {
        let Some(position) = self.arguments.iter().position(|(held, _)| held == id) else {
            return;
        };
        let (_, arguments) = self.arguments.remove(position);
        if arguments.is_empty() {
            return;
        }
        match self.session.restore(&arguments) {
            Ok(restored) => out.push_back(Ok(ModelDelta::ToolCallArgs {
                id: id.to_string(),
                delta: restored,
            })),
            Err(error) => out.push_back(Err(as_model_error(error))),
        }
    }
}

/// Which of the two text buffers a delta belongs to.
#[derive(Clone, Copy)]
enum Kind {
    Text,
    Reasoning,
}

impl Kind {
    fn take(self, restorer: &mut Restorer) -> String {
        match self {
            Self::Text => std::mem::take(&mut restorer.text),
            Self::Reasoning => std::mem::take(&mut restorer.reasoning),
        }
    }

    fn put(self, restorer: &mut Restorer, held: String) {
        match self {
            Self::Text => restorer.text = held,
            Self::Reasoning => restorer.reasoning = held,
        }
    }

    fn delta(self, words: String) -> ModelDelta {
        match self {
            Self::Text => ModelDelta::Text(words),
            Self::Reasoning => ModelDelta::Reasoning(words),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "../tests/unit/cloaked_door_tests.rs"]
mod tests;
