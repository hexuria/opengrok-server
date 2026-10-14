//! The reverse-exec BROKER — the in-memory transport core, one per replica.
//!
//! It is the meeting point of two live parties: a daemon on the user's Mac holding an SSE stream
//! open (`GET /local-exec/requests`), and a caller (a suspended bot turn, or a direct enqueue from
//! the user's phone) waiting for one command's result. The broker pushes an approved command down
//! the daemon's stream and hands the caller back a channel it resolves when the daemon posts the
//! result (`POST /local-exec/responses`). It never decides anything — the gate did that upstream;
//! the broker only carries an ALREADY-APPROVED command.
//!
//! Refuse-if-offline: a command for a machine whose daemon is not connected is refused crisply
//! rather than queued to wait forever. One daemon per account and machine; a reconnect replaces
//! only that account's old stream.

use std::collections::HashMap;

use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc, oneshot};

type MachineKey = (String, String);
type RequestKey = (String, String, String);

/// The outcome of one command, read off the daemon's result frame ([`super::wire`]). Carries both
/// what the caller sees (stdout/stderr/detail) and what the audit records (the `case`, and an exit
/// code only when the process actually ran).
#[derive(Debug, Clone)]
pub struct ExecOutcome {
    /// The ShellResult oneof case (success / failure / timeout / rejected / spawnError /
    /// permissionDenied), or the server's own `offline`. This is what the audit's `outcome` records.
    pub case: String,
    /// The process exit code — `Some` only for `success`/`failure`. A refusal has none.
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// The reason for a non-success case (a timeout duration, a rejection reason, an error string).
    pub detail: String,
}

impl ExecOutcome {
    /// Did the command run to a normal completion (exit 0)?
    pub fn succeeded(&self) -> bool {
        self.case == "success" && self.exit_code == Some(0)
    }

    /// A stand-in outcome for a daemon reply the server could not read — a definite answer for the
    /// caller and a recordable case, never a panic.
    pub fn malformed(reason: &str) -> Self {
        Self {
            case: "spawnError".to_string(),
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            detail: reason.to_string(),
        }
    }

    /// No daemon holds this machine's stream. A server-only word, never read off the wire
    /// (`wire::RESULT_CASES` must not learn it, or a daemon could claim it): an asleep Mac and a
    /// garbled reply were both `spawnError`, and the audit could not tell them apart (#147).
    pub fn offline(reason: &str) -> Self {
        Self {
            case: "offline".to_string(),
            ..Self::malformed(reason)
        }
    }

    /// The outcome the caller gets when the daemon never answers in time.
    pub fn timed_out(reason: &str) -> Self {
        Self {
            case: "timeout".to_string(),
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            detail: reason.to_string(),
        }
    }

    /// A compact human rendering for a tool result or the direct-enqueue reply.
    pub fn render(&self) -> String {
        match self.case.as_str() {
            "success" | "failure" => {
                let code = self.exit_code.unwrap_or_default();
                let mut out = format!("exit {code}");
                if !self.stdout.is_empty() {
                    out.push_str(&format!("\n--- stdout ---\n{}", self.stdout));
                }
                if !self.stderr.is_empty() {
                    out.push_str(&format!("\n--- stderr ---\n{}", self.stderr));
                }
                out
            }
            other => {
                if self.detail.is_empty() {
                    other.to_string()
                } else {
                    format!("{other}: {}", self.detail)
                }
            }
        }
    }
}

/// A daemon connected for a machine, but its stream cannot accept a request just now.
#[derive(Debug, PartialEq, Eq)]
pub enum DispatchError {
    /// No daemon is holding a stream open for this machine.
    NoDaemon,
}

#[derive(Default)]
struct Inner {
    /// (account, machine) → stream. A machine id is the client's to choose, so accounts are
    /// separate providers even when they use the same id.
    providers: HashMap<MachineKey, mpsc::UnboundedSender<Value>>,
    /// (account, machine, request) → caller waiting for the daemon's result.
    waiters: HashMap<RequestKey, oneshot::Sender<ExecOutcome>>,
    /// (account, machine, request) → accumulated stdout and stderr for streaming shell output.
    streams: HashMap<RequestKey, (String, String)>,
}

/// The broker. Cheap to clone through an `Arc`; all state is behind one mutex held only for the
/// brief map operations, never across an `.await` on the network.
#[derive(Default)]
pub struct LocalExecBroker {
    inner: Mutex<Inner>,
}

impl LocalExecBroker {
    pub fn new() -> Self {
        Self::default()
    }

    /// A daemon opened its stream with `account`'s token. Returns the receiver the SSE route drains
    /// as frames, after a `welcome` is queued. Replaces any previous stream for this machine (a
    /// reconnect wins): the old one is sent nothing more and stays open, out of `disconnect`'s
    /// reach, until its daemon hangs up.
    pub async fn connect(&self, account: &str, machine_id: &str) -> mpsc::UnboundedReceiver<Value> {
        let (tx, rx) = mpsc::unbounded_channel();
        // The first frame names the provider so the daemon can correlate — mirrors the client's
        // `welcome{providerId}`.
        let _ = tx.send(json!({ "kind": "welcome", "providerId": machine_id }));
        let mut inner = self.inner.lock().await;
        inner
            .providers
            .insert((account.to_string(), machine_id.to_string()), tx);
        rx
    }

    /// `account`'s daemon token for this machine was revoked, or rotated by a re-enrolment (#299):
    /// its stream ends now, sent no more commands; its results were refused already, as the token
    /// is checked on every POST. A `null`, which is never a frame, tells the route to end.
    pub async fn disconnect(&self, account: &str, machine_id: &str) {
        let mut inner = self.inner.lock().await;
        if let Some(stream) = inner
            .providers
            .remove(&(account.to_string(), machine_id.to_string()))
        {
            let _ = stream.send(Value::Null);
        }
    }

    /// Is a daemon currently connected for this account's machine?
    pub async fn has_provider(&self, account: &str, machine_id: &str) -> bool {
        let inner = self.inner.lock().await;
        inner
            .providers
            .get(&(account.to_string(), machine_id.to_string()))
            .is_some_and(|tx| !tx.is_closed())
    }

    /// Push an already-approved exec frame to a machine's daemon and return the channel its result
    /// arrives on. Refuses if no daemon is connected. `request_id` correlates the later result;
    /// `server_message` is the opaque exec payload from [`super::wire`].
    pub async fn dispatch(
        &self,
        account: &str,
        machine_id: &str,
        request_id: &str,
        approval_id: &str,
        server_message: Value,
    ) -> Result<oneshot::Receiver<ExecOutcome>, DispatchError> {
        let (tx, rx) = oneshot::channel();
        let mut inner = self.inner.lock().await;
        let provider_key = (account.to_string(), machine_id.to_string());
        let request_key = (
            account.to_string(),
            machine_id.to_string(),
            request_id.to_string(),
        );
        let Some(provider) = inner.providers.get(&provider_key) else {
            return Err(DispatchError::NoDaemon);
        };
        let frame = json!({
            "kind": "exec",
            "requestId": request_id,
            // The approvalId is the daemon's local consent handle: the id the inline card recorded
            // the local approval under (= the tool call id for a bot), so the daemon's own gate finds
            // it and does not re-prompt. Distinct from requestId, which only correlates the result.
            "approvalId": approval_id,
            "serverMessage": server_message,
        });
        if provider.send(frame).is_err() {
            // The daemon's stream is gone even though its entry lingered; clean it up and refuse.
            inner.providers.remove(&provider_key);
            return Err(DispatchError::NoDaemon);
        }
        inner.waiters.insert(request_key, tx);
        Ok(rx)
    }

    /// A daemon posted a result. Resolve the waiting caller only for the same account and machine.
    /// A mismatched or late result is ignored, so another account cannot resolve this command.
    pub async fn resolve(
        &self,
        account: &str,
        from_machine: &str,
        request_id: &str,
        outcome: ExecOutcome,
    ) {
        let key = (
            account.to_string(),
            from_machine.to_string(),
            request_id.to_string(),
        );
        let waiter = {
            let mut inner = self.inner.lock().await;
            inner.waiters.remove(&key)
        };
        if let Some(waiter) = waiter {
            let _ = waiter.send(outcome);
        }
    }

    /// Accumulate a streaming-shell output chunk for a request. Ignored unless a waiter for this
    /// request exists and belongs to the posting machine — so a stray or cross-machine chunk is
    /// dropped rather than buffered forever.
    pub async fn accumulate(
        &self,
        account: &str,
        from_machine: &str,
        request_id: &str,
        is_stderr: bool,
        data: &str,
    ) {
        let mut inner = self.inner.lock().await;
        let key = (
            account.to_string(),
            from_machine.to_string(),
            request_id.to_string(),
        );
        if !inner.waiters.contains_key(&key) {
            return;
        }
        let (out, err) = inner.streams.entry(key).or_default();
        if is_stderr {
            err.push_str(data);
        } else {
            out.push_str(data);
        }
    }

    /// Terminal for a streaming-shell request: combine the accumulated stdout/stderr with a final
    /// `case` (success/failure/rejected/…), optional exit code, and reason, then resolve the caller
    /// and drop the buffers. Machine-guarded like `resolve`.
    pub async fn finish_stream(
        &self,
        account: &str,
        from_machine: &str,
        request_id: &str,
        case: &str,
        exit_code: Option<i32>,
        detail: &str,
    ) {
        let key = (
            account.to_string(),
            from_machine.to_string(),
            request_id.to_string(),
        );
        let (waiter, buffers) = {
            let mut inner = self.inner.lock().await;
            if !inner.waiters.contains_key(&key) {
                (None, (String::new(), String::new()))
            } else {
                let waiter = inner.waiters.remove(&key);
                let buffers = inner.streams.remove(&key).unwrap_or_default();
                (waiter, buffers)
            }
        };
        if let Some(waiter) = waiter {
            let _ = waiter.send(ExecOutcome {
                case: case.to_string(),
                exit_code,
                stdout: buffers.0,
                stderr: buffers.1,
                detail: detail.to_string(),
            });
        }
    }

    /// Abandon a request: drop its waiter and tell the daemon to cancel it if we still can. Used
    /// when the caller times out.
    pub async fn cancel(&self, account: &str, machine_id: &str, request_id: &str) {
        let mut inner = self.inner.lock().await;
        let request_key = (
            account.to_string(),
            machine_id.to_string(),
            request_id.to_string(),
        );
        inner.waiters.remove(&request_key);
        inner.streams.remove(&request_key);
        if let Some(provider) = inner
            .providers
            .get(&(account.to_string(), machine_id.to_string()))
        {
            let _ = provider.send(json!({ "kind": "cancel", "requestId": request_id }));
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/local_exec_broker.rs"]
mod tests;
