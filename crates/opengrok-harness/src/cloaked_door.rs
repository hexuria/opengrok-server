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
mod tests {
    use super::*;
    use crate::model::ChatMessage;
    use cred_swap_core::{Policy, Style};
    use std::sync::Mutex;

    /// A door that records what it was asked and replays a scripted stream.
    struct ScriptedDoor {
        seen: Arc<Mutex<Vec<ModelRequest>>>,
        script: Arc<Mutex<Vec<ModelDelta>>>,
    }

    impl ScriptedDoor {
        fn wired(script: Vec<ModelDelta>) -> (Arc<dyn ModelDoor>, Arc<Mutex<Vec<ModelRequest>>>) {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let door = Arc::new(Self {
                seen: Arc::clone(&seen),
                script: Arc::new(Mutex::new(script)),
            });
            (door, seen)
        }
    }

    #[async_trait::async_trait]
    impl ModelDoor for ScriptedDoor {
        async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
            if let Ok(mut seen) = self.seen.lock() {
                seen.push(request);
            }
            let script = match self.script.lock() {
                Ok(mut guard) => std::mem::take(&mut *guard),
                Err(_) => Vec::new(),
            };
            Ok(Box::pin(stream::iter(script.into_iter().map(Ok))))
        }
    }

    fn store() -> SessionStore {
        SessionStore::builder()
            .policy(Policy::default())
            .style(Style::Realistic)
            .secret(b"cloaked door tests")
            .build()
            .unwrap()
    }

    fn request(text: &str) -> ModelRequest {
        ModelRequest {
            model: "oag/cheap".to_string(),
            messages: vec![ChatMessage::text("user", text.to_string())],
            spend_scope: Some("cw_test".to_string()),
            spend_actor: Some("acct_test".to_string()),
            ..ModelRequest::default()
        }
    }

    async fn collect(stream: DeltaStream) -> Vec<ModelDelta> {
        stream
            .filter_map(|item| async move { item.ok() })
            .collect()
            .await
    }

    fn said(deltas: &[ModelDelta]) -> String {
        deltas
            .iter()
            .filter_map(|delta| match delta {
                ModelDelta::Text(words) => Some(words.as_str()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn the_provider_never_sees_the_real_values() {
        let (inner, seen) = ScriptedDoor::wired(Vec::new());
        let door = CloakedDoor::new(inner, store());

        drop(
            door.stream(request("ssh db.prod.internal, mail dana@corp.com"))
                .await
                .unwrap(),
        );

        let sent = seen.lock().unwrap();
        let content = &sent[0].messages[0].content;
        assert!(!content.contains("db.prod.internal"), "{content}");
        assert!(!content.contains("dana@corp.com"), "{content}");
        // Still a sentence about a host and an address, so the model can answer it.
        assert!(content.contains("ssh "), "{content}");
        assert!(content.contains('@'), "{content}");
    }

    #[tokio::test]
    async fn the_system_prompt_is_scrubbed_but_the_tool_schema_is_not() {
        let (inner, seen) = ScriptedDoor::wired(Vec::new());
        let door = CloakedDoor::new(inner, store());

        let mut request = request("hello");
        request.system = Some("You maintain db.prod.internal.".to_string());
        request.tools = vec![serde_json::json!({
            "type": "function",
            "function": {"name": "ssh_run", "parameters": {"properties": {"host": {"type": "string"}}}}
        })];

        drop(door.stream(request).await.unwrap());

        let sent = seen.lock().unwrap();
        let system = sent[0].system.clone().unwrap();
        assert!(!system.contains("db.prod.internal"), "{system}");
        // Renaming a tool or its parameters would make the model ask for
        // something that does not exist.
        assert_eq!(sent[0].tools[0]["function"]["name"], "ssh_run");
        assert!(
            sent[0].tools[0]["function"]["parameters"]["properties"]["host"].is_object(),
            "the tool schema was rewritten"
        );
    }

    #[tokio::test]
    async fn a_stand_in_split_across_deltas_is_restored() {
        // Learn the stand-in the way the door will produce it.
        let sessions = store();
        let stand_in = sessions
            .session("pin:cw_test:acct_test")
            .unwrap()
            .scrub("db.prod.internal")
            .unwrap()
            .replacements[0]
            .fake
            .clone();

        // The provider streams it one character at a time, the worst case.
        let mut script = vec![ModelDelta::Text("connect to ".to_string())];
        script.extend(stand_in.chars().map(|c| ModelDelta::Text(c.to_string())));
        script.push(ModelDelta::Text(" and retry".to_string()));

        let (inner, _) = ScriptedDoor::wired(script);
        let door = CloakedDoor::new(inner, sessions);

        let out = collect(
            door.stream(request("check db.prod.internal"))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(said(&out), "connect to db.prod.internal and retry");
    }

    #[tokio::test]
    async fn tool_arguments_are_restored_before_the_tool_would_run() {
        let sessions = store();
        let stand_in = sessions
            .session("pin:cw_test:acct_test")
            .unwrap()
            .scrub("db.prod.internal")
            .unwrap()
            .replacements[0]
            .fake
            .clone();

        // Arguments arrive as JSON split mid-value, which is why they are held
        // until the call closes.
        let half = stand_in.len() / 2;
        let script = vec![
            ModelDelta::ToolCallStart {
                id: "call_1".to_string(),
                name: "ssh_run".to_string(),
            },
            ModelDelta::ToolCallArgs {
                id: "call_1".to_string(),
                delta: format!("{{\"host\":\"{}", &stand_in[..half]),
            },
            ModelDelta::ToolCallArgs {
                id: "call_1".to_string(),
                delta: format!("{}\",\"command\":\"uptime\"}}", &stand_in[half..]),
            },
            ModelDelta::ToolCallEnd {
                id: "call_1".to_string(),
            },
        ];

        let (inner, _) = ScriptedDoor::wired(script);
        let door = CloakedDoor::new(inner, sessions);
        let out = collect(
            door.stream(request("check db.prod.internal"))
                .await
                .unwrap(),
        )
        .await;

        let arguments: String = out
            .iter()
            .filter_map(|delta| match delta {
                ModelDelta::ToolCallArgs { delta, .. } => Some(delta.as_str()),
                _ => None,
            })
            .collect();

        let parsed: serde_json::Value = serde_json::from_str(&arguments).unwrap();
        assert_eq!(
            parsed["host"], "db.prod.internal",
            "the coworker would have sshed to a host that does not exist"
        );
        assert_eq!(parsed["command"], "uptime");

        // The bracket order the projection depends on is preserved.
        assert!(matches!(
            out.first(),
            Some(ModelDelta::ToolCallStart { .. })
        ));
        assert!(matches!(out.last(), Some(ModelDelta::ToolCallEnd { .. })));
    }

    #[tokio::test]
    async fn text_before_a_tool_call_is_flushed_in_order() {
        let (inner, _) = ScriptedDoor::wired(vec![
            ModelDelta::Text("I will check.".to_string()),
            ModelDelta::ToolCallStart {
                id: "call_1".to_string(),
                name: "ssh_run".to_string(),
            },
            ModelDelta::ToolCallEnd {
                id: "call_1".to_string(),
            },
        ]);
        let door = CloakedDoor::new(inner, store());
        let out = collect(door.stream(request("check it")).await.unwrap()).await;

        assert!(matches!(out.first(), Some(ModelDelta::Text(_))));
        assert_eq!(said(&out), "I will check.");
        assert_eq!(out.len(), 3);
    }

    #[tokio::test]
    async fn a_tool_result_coming_back_round_is_scrubbed_too() {
        let (inner, seen) = ScriptedDoor::wired(Vec::new());
        let door = CloakedDoor::new(inner, store());

        // The shape the harness builds in `tool_result_message`.
        let mut request = request("check the logs");
        request.messages.push(ChatMessage::tool_result(
            "call_1",
            "url=postgresql://appuser:s3cr3t@db.prod.internal/orders",
        ));

        drop(door.stream(request).await.unwrap());

        let sent = seen.lock().unwrap();
        let result = &sent[0].messages[1];
        assert!(!result.content.contains("s3cr3t"), "{result:?}");
        assert!(!result.content.contains("db.prod.internal"), "{result:?}");
        assert!(result.content.starts_with("url="), "{result:?}");
        assert_eq!(result.tool_call_id.as_deref(), Some("call_1"));
    }

    /// The model's own call from an earlier round comes back in the next request with the real
    /// values restored into it; it leaves scrubbed like every other message, or the cloak has a
    /// hole the size of every command the coworker ever ran.
    #[tokio::test]
    async fn a_call_the_model_made_leaves_scrubbed_too() {
        let (inner, seen) = ScriptedDoor::wired(Vec::new());
        let door = CloakedDoor::new(inner, store());

        let mut request = request("check the logs");
        request.messages.push(ChatMessage::calls(
            "",
            vec![crate::model::ToolCallRef {
                id: "call_1".to_string(),
                name: "shell".to_string(),
                arguments:
                    r#"{"command":"psql postgresql://appuser:s3cr3t@db.prod.internal/orders"}"#
                        .to_string(),
            }],
        ));

        drop(door.stream(request).await.unwrap());

        let sent = seen.lock().unwrap();
        let call = &sent[0].messages[1].tool_calls[0];
        assert!(!call.arguments.contains("s3cr3t"), "{call:?}");
        assert!(!call.arguments.contains("db.prod.internal"), "{call:?}");
        assert_eq!((call.id.as_str(), call.name.as_str()), ("call_1", "shell"));
    }

    #[tokio::test]
    async fn a_conversation_keeps_one_stand_in_across_rounds() {
        let sessions = store();
        let (inner, seen) = ScriptedDoor::wired(Vec::new());
        let door = CloakedDoor::new(inner, sessions);

        drop(door.stream(request("mail dana@corp.com")).await.unwrap());
        drop(
            door.stream(request("remind dana@corp.com again"))
                .await
                .unwrap(),
        );

        let sent = seen.lock().unwrap();
        let first = sent[0].messages[0].content.replace("mail ", "");
        assert!(
            sent[1].messages[0].content.contains(&first),
            "the second round gave the same person a different stand-in: {:?}",
            *sent
        );
    }

    #[tokio::test]
    async fn two_coworkers_do_not_share_stand_ins() {
        let sessions = store();
        let (inner, seen) = ScriptedDoor::wired(Vec::new());
        let door = CloakedDoor::new(inner, sessions);

        let mut theirs = request("mail dana@corp.com");
        theirs.spend_scope = Some("cw_other".to_string());
        theirs.spend_actor = Some("acct_other".to_string());

        drop(door.stream(request("mail dana@corp.com")).await.unwrap());
        drop(door.stream(theirs).await.unwrap());

        let sent = seen.lock().unwrap();
        assert_ne!(
            sent[0].messages[0].content, sent[1].messages[0].content,
            "one coworker's stand-in identified the value in another's transcript"
        );
    }

    #[tokio::test]
    async fn an_upstream_error_does_not_swallow_the_text_before_it() {
        struct Failing;

        #[async_trait::async_trait]
        impl ModelDoor for Failing {
            async fn stream(&self, _: ModelRequest) -> Result<DeltaStream, ModelError> {
                Ok(Box::pin(stream::iter(vec![
                    Ok(ModelDelta::Text("partial answer".to_string())),
                    Err(ModelError::Stream("upstream died".to_string())),
                ])))
            }
        }

        let door = CloakedDoor::new(Arc::new(Failing), store());
        let mut stream = door.stream(request("hello")).await.unwrap();

        let mut words = String::new();
        let mut saw_error = false;
        while let Some(item) = stream.next().await {
            match item {
                Ok(ModelDelta::Text(said)) => words.push_str(&said),
                Err(_) => saw_error = true,
                Ok(_) => {}
            }
        }
        assert_eq!(words, "partial answer");
        assert!(saw_error, "the error was lost");
    }

    #[tokio::test]
    async fn text_with_nothing_sensitive_passes_through_unchanged() {
        let (inner, seen) = ScriptedDoor::wired(vec![ModelDelta::Text(
            "Use a BTreeMap so the order is stable.".to_string(),
        )]);
        let door = CloakedDoor::new(inner, store());

        let asked = "Refactor the parser to stop allocating per token.";
        let out = collect(door.stream(request(asked)).await.unwrap()).await;

        assert_eq!(seen.lock().unwrap()[0].messages[0].content, asked);
        assert_eq!(said(&out), "Use a BTreeMap so the order is stable.");
    }

    #[test]
    fn the_conversation_key_follows_the_spend_pin() {
        let mut request = request("hello");
        assert_eq!(default_key(&request), "pin:cw_test:acct_test");

        // Without a pin, the opening message separates conversations.
        request.spend_scope = None;
        request.spend_actor = None;
        let mine = default_key(&request);
        assert!(mine.starts_with("open:"), "{mine}");

        let mut other = request.clone();
        other.messages[0].content = "a different opening".to_string();
        assert_ne!(mine, default_key(&other));

        // And it is stable as the conversation grows.
        request
            .messages
            .push(ChatMessage::text("assistant", "sure"));
        assert_eq!(mine, default_key(&request));
    }
}
