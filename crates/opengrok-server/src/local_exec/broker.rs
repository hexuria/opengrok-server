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
//! rather than queued to wait forever. One daemon per machine; a reconnect replaces the old stream.

use std::collections::HashMap;

use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc, oneshot};

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
    /// machine_id → the account whose daemon opened its stream, and the stream. Last connect
    /// wins. A machine id is the client's to choose: one account's revoke ends no other's stream.
    providers: HashMap<String, (String, mpsc::UnboundedSender<Value>)>,
    /// request_id → (the machine it was dispatched to, the caller waiting for its result). The
    /// machine is kept so a result is only ever accepted from the SAME machine — a daemon for one
    /// machine cannot resolve another machine's command.
    waiters: HashMap<String, (String, oneshot::Sender<ExecOutcome>)>,
    /// request_id → accumulated (stdout, stderr) for the STREAMING shell, which sends chunks across
    /// several frames before a terminal exit. Dropped when the request resolves.
    streams: HashMap<String, (String, String)>,
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
            .insert(machine_id.to_string(), (account.to_string(), tx));
        rx
    }

    /// `account`'s daemon token for this machine was revoked, or rotated by a re-enrolment (#299):
    /// its stream ends now, sent no more commands; its results were refused already, as the token
    /// is checked on every POST. A `null`, which is never a frame, tells the route to end.
    pub async fn disconnect(&self, account: &str, machine_id: &str) {
        let mut inner = self.inner.lock().await;
        if let Some((owner, stream)) = inner.providers.get(machine_id)
            && owner == account
        {
            let _ = stream.send(Value::Null);
            inner.providers.remove(machine_id);
        }
    }

    /// Is a daemon currently connected for this machine?
    pub async fn has_provider(&self, machine_id: &str) -> bool {
        let inner = self.inner.lock().await;
        inner
            .providers
            .get(machine_id)
            .is_some_and(|(_, tx)| !tx.is_closed())
    }

    /// Push an already-approved exec frame to a machine's daemon and return the channel its result
    /// arrives on. Refuses if no daemon is connected. `request_id` correlates the later result;
    /// `server_message` is the opaque exec payload from [`super::wire`].
    pub async fn dispatch(
        &self,
        machine_id: &str,
        request_id: &str,
        approval_id: &str,
        server_message: Value,
    ) -> Result<oneshot::Receiver<ExecOutcome>, DispatchError> {
        let (tx, rx) = oneshot::channel();
        let mut inner = self.inner.lock().await;
        let Some((_, provider)) = inner.providers.get(machine_id) else {
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
            inner.providers.remove(machine_id);
            return Err(DispatchError::NoDaemon);
        }
        inner
            .waiters
            .insert(request_id.to_string(), (machine_id.to_string(), tx));
        Ok(rx)
    }

    /// A daemon posted a result. Resolve the waiting caller, but ONLY if the posting machine is the
    /// one the command was dispatched to — a mismatched or unknown id is ignored, so one machine's
    /// daemon can neither resolve nor observe another's command. A late id (the caller already gave
    /// up) is ignored too, so a slow daemon can never wedge the broker.
    pub async fn resolve(&self, from_machine: &str, request_id: &str, outcome: ExecOutcome) {
        let waiter = {
            let mut inner = self.inner.lock().await;
            match inner.waiters.get(request_id) {
                Some((machine, _)) if machine == from_machine => {
                    inner.waiters.remove(request_id).map(|(_, tx)| tx)
                }
                _ => None,
            }
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
        from_machine: &str,
        request_id: &str,
        is_stderr: bool,
        data: &str,
    ) {
        let mut inner = self.inner.lock().await;
        let ours = matches!(inner.waiters.get(request_id), Some((m, _)) if m == from_machine);
        if !ours {
            return;
        }
        let (out, err) = inner.streams.entry(request_id.to_string()).or_default();
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
        from_machine: &str,
        request_id: &str,
        case: &str,
        exit_code: Option<i32>,
        detail: &str,
    ) {
        let (waiter, buffers) = {
            let mut inner = self.inner.lock().await;
            let ours = matches!(inner.waiters.get(request_id), Some((m, _)) if m == from_machine);
            if !ours {
                (None, (String::new(), String::new()))
            } else {
                let waiter = inner.waiters.remove(request_id).map(|(_, tx)| tx);
                let buffers = inner.streams.remove(request_id).unwrap_or_default();
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
    pub async fn cancel(&self, machine_id: &str, request_id: &str) {
        let mut inner = self.inner.lock().await;
        inner.waiters.remove(request_id);
        inner.streams.remove(request_id);
        if let Some((_, provider)) = inner.providers.get(machine_id) {
            let _ = provider.send(json!({ "kind": "cancel", "requestId": request_id }));
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/local_exec_broker.rs"]
mod tests;
