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

use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, Mutex};

/// The outcome of one command, read off the daemon's result frame ([`super::wire`]). Carries both
/// what the caller sees (stdout/stderr/detail) and what the audit records (the `case`, and an exit
/// code only when the process actually ran).
#[derive(Debug, Clone)]
pub struct ExecOutcome {
    /// The ShellResult oneof case: success / failure / timeout / rejected / spawnError /
    /// permissionDenied. This is what the audit row's `outcome` records.
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

    /// Did the command run to a normal completion (exit 0)?
    pub fn succeeded(&self) -> bool {
        self.case == "success" && self.exit_code == Some(0)
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
    /// machine_id → the frame sink of the daemon currently streaming for it. Last connect wins.
    providers: HashMap<String, mpsc::UnboundedSender<Value>>,
    /// request_id → (the machine it was dispatched to, the caller waiting for its result). The
    /// machine is kept so a result is only ever accepted from the SAME machine — a daemon for one
    /// machine cannot resolve another machine's command.
    waiters: HashMap<String, (String, oneshot::Sender<ExecOutcome>)>,
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

    /// A daemon opened its stream. Returns the receiver the SSE route drains as frames, after a
    /// `welcome` is queued. Replaces any previous stream for this machine (a reconnect wins), which
    /// drops the old receiver and ends its route.
    pub async fn connect(&self, machine_id: &str) -> mpsc::UnboundedReceiver<Value> {
        let (tx, rx) = mpsc::unbounded_channel();
        // The first frame names the provider so the daemon can correlate — mirrors the client's
        // `welcome{providerId}`.
        let _ = tx.send(json!({ "type": "welcome", "providerId": machine_id }));
        let mut inner = self.inner.lock().await;
        inner.providers.insert(machine_id.to_string(), tx);
        rx
    }

    /// Is a daemon currently connected for this machine?
    pub async fn has_provider(&self, machine_id: &str) -> bool {
        let inner = self.inner.lock().await;
        inner
            .providers
            .get(machine_id)
            .is_some_and(|tx| !tx.is_closed())
    }

    /// Push an already-approved exec frame to a machine's daemon and return the channel its result
    /// arrives on. Refuses if no daemon is connected. `request_id` correlates the later result;
    /// `server_message` is the opaque exec payload from [`super::wire`].
    pub async fn dispatch(
        &self,
        machine_id: &str,
        request_id: &str,
        server_message: Value,
    ) -> Result<oneshot::Receiver<ExecOutcome>, DispatchError> {
        let (tx, rx) = oneshot::channel();
        let mut inner = self.inner.lock().await;
        let Some(provider) = inner.providers.get(machine_id) else {
            return Err(DispatchError::NoDaemon);
        };
        let frame = json!({
            "type": "exec",
            "requestId": request_id,
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

    /// Abandon a request: drop its waiter and tell the daemon to cancel it if we still can. Used
    /// when the caller times out.
    pub async fn cancel(&self, machine_id: &str, request_id: &str) {
        let mut inner = self.inner.lock().await;
        inner.waiters.remove(request_id);
        if let Some(provider) = inner.providers.get(machine_id) {
            let _ = provider.send(json!({ "type": "cancel", "requestId": request_id }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn success(stdout: &str) -> ExecOutcome {
        ExecOutcome {
            case: "success".to_string(),
            exit_code: Some(0),
            stdout: stdout.to_string(),
            stderr: String::new(),
            detail: String::new(),
        }
    }

    #[tokio::test]
    async fn dispatch_refuses_when_no_daemon_is_connected() {
        let broker = LocalExecBroker::new();
        let result = broker
            .dispatch("mac_a", "req-1", json!({ "shellArgs": {} }))
            .await;
        assert_eq!(result.unwrap_err(), DispatchError::NoDaemon);
    }

    #[tokio::test]
    async fn a_connected_daemon_receives_the_exec_frame_and_the_caller_gets_the_result() {
        let broker = LocalExecBroker::new();
        let mut stream = broker.connect("mac_a").await;

        // First frame down the stream is the welcome.
        let welcome = stream.recv().await.expect("welcome");
        assert_eq!(welcome["type"], "welcome");

        let rx = broker
            .dispatch("mac_a", "req-1", json!({ "shellArgs": { "command": "echo hi" } }))
            .await
            .expect("dispatched");

        let frame = stream.recv().await.expect("exec frame");
        assert_eq!(frame["type"], "exec");
        assert_eq!(frame["requestId"], "req-1");
        assert_eq!(frame["serverMessage"]["shellArgs"]["command"], "echo hi");

        broker.resolve("mac_a", "req-1", success("hi\n")).await;
        let outcome = rx.await.expect("result");
        assert!(outcome.succeeded());
        assert_eq!(outcome.stdout, "hi\n");
    }

    #[tokio::test]
    async fn a_reconnect_replaces_the_old_stream() {
        let broker = LocalExecBroker::new();
        let mut first = broker.connect("mac_a").await;
        let _ = first.recv().await; // welcome
        let mut second = broker.connect("mac_a").await;
        let _ = second.recv().await; // welcome

        broker
            .dispatch("mac_a", "req-1", json!({ "shellArgs": {} }))
            .await
            .expect("dispatched to the live stream");
        // The exec frame goes to the NEW stream, not the retired one.
        let frame = second.recv().await.expect("exec on second");
        assert_eq!(frame["requestId"], "req-1");
    }

    #[tokio::test]
    async fn an_unknown_result_id_is_ignored() {
        let broker = LocalExecBroker::new();
        // No waiter registered — resolving must not panic.
        broker.resolve("mac_a", "nope", success("")).await;
    }

    #[test]
    fn render_shows_exit_and_streams_for_a_run_and_the_case_for_a_refusal() {
        assert!(success("hi").render().contains("exit 0"));
        let denied = ExecOutcome {
            case: "permissionDenied".to_string(),
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            detail: "not permitted".to_string(),
        };
        assert_eq!(denied.render(), "permissionDenied: not permitted");
    }
}
