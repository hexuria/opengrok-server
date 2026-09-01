//! box.ascii.dev — persistent Ubuntu VMs behind a plain REST API.
//!
//! The typed HTTP client (`Client`) is the SDK, transcribed from `docs/box/`. `AsciiBoxes` is
//! the OpenGrok `Computer` adapter on top of it so the harness never talks HTTP itself.
//!
//! THE FILESYSTEM PERSISTS ACROSS STOP AND RESUME. That is what makes this the right first
//! computer: `stop` pauses billing and keeps the disk, so an agent's machine can sleep between
//! turns and wake with its work intact — which is the difference between a coworker and a session.

use async_trait::async_trait;

use crate::{BoxError, BoxResult, CommandOutput, Computer, StartedCommand};

mod client;
pub mod types;

pub use client::{CONFIRM_DELETE_HEADER, Client, DEFAULT_BASE_URL};
pub use types::{
    BoxRecord, CommandFinished, CommandRequest, CommandStarted, CommandStatus, CreateBoxRequest,
    CreateBoxResponse, HostedPort,
};

/// Where the boxes live and the key that opens them.
///
/// The key is read from the environment and never from a coworker's row: a computer is not a
/// credential a client may set, which is the same rule the gateway applies to model keys.
#[derive(Clone, Debug)]
pub struct AsciiBoxes {
    client: Client,
}

impl AsciiBoxes {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: Client::new(api_key),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.client = self.client.with_base_url(base_url);
        self
    }

    /// The typed v1 client. Use this when you need a documented endpoint the `Computer` trait
    /// does not expose (list, fork, interrupt, SSH, idempotent create).
    pub fn client(&self) -> &Client {
        &self.client
    }
}

impl From<CommandFinished> for CommandOutput {
    fn from(finished: CommandFinished) -> Self {
        Self {
            // A command killed by a signal reports no exit code. -1 rather than 0, because a
            // coworker reading 0 would conclude the command succeeded.
            exit_code: finished.exit_code.unwrap_or(-1),
            stdout: finished.stdout,
            stderr: finished.stderr,
            stdout_truncated: finished.stdout_truncated,
            stderr_truncated: finished.stderr_truncated,
            timed_out: finished.timed_out,
        }
    }
}

impl From<CommandStarted> for StartedCommand {
    fn from(started: CommandStarted) -> Self {
        Self {
            process_id: started.process_id,
            running: started.running,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
        }
    }
}

impl From<CommandStatus> for StartedCommand {
    fn from(status: CommandStatus) -> Self {
        Self {
            process_id: status.process_id,
            running: status.running,
            stdout: status.stdout,
            stderr: status.stderr,
            exit_code: status.exit_code,
        }
    }
}

#[async_trait]
impl Computer for AsciiBoxes {
    fn kind(&self) -> &'static str {
        "ascii"
    }

    async fn create(&self, ttl_seconds: Option<u64>) -> BoxResult<String> {
        let created = self
            .client
            .create_box(
                &CreateBoxRequest::with_ttl(ttl_seconds.unwrap_or(3600)),
                None,
            )
            .await?;
        created
            .id()
            .map(str::to_string)
            .ok_or_else(|| BoxError::Refused {
                status: 200,
                body: "the create reply carried no box id under box.id".to_string(),
            })
    }

    async fn run(
        &self,
        box_id: &str,
        command: &str,
        timeout_seconds: u32,
    ) -> BoxResult<CommandOutput> {
        let finished = self
            .client
            .run_command(
                box_id,
                CommandRequest {
                    command: command.to_string(),
                    cwd: None,
                    timeout_seconds: Some(timeout_seconds),
                    detached: Some(false),
                },
            )
            .await?;
        Ok(finished.into())
    }

    async fn start(&self, box_id: &str, command: &str) -> BoxResult<StartedCommand> {
        let started = self
            .client
            .start_command(
                box_id,
                CommandRequest {
                    command: command.to_string(),
                    cwd: None,
                    timeout_seconds: None,
                    detached: Some(true),
                },
            )
            .await?;
        Ok(started.into())
    }

    async fn watch(&self, box_id: &str, process_id: &str) -> BoxResult<StartedCommand> {
        let status = self.client.command_status(box_id, process_id).await?;
        Ok(status.into())
    }

    async fn read_file(&self, box_id: &str, path: &str) -> BoxResult<String> {
        Ok(self.client.read_file(box_id, path).await?.content)
    }

    async fn write_file(&self, box_id: &str, path: &str, content: &str) -> BoxResult<()> {
        let _ = self.client.write_file(box_id, path, content).await?;
        Ok(())
    }

    async fn expose_port(&self, box_id: &str, port: u16, title: &str) -> BoxResult<String> {
        let hosted = self.client.host_port(box_id, port, title).await?;
        hosted
            .url()
            .map(str::to_string)
            .ok_or_else(|| BoxError::Refused {
                status: 200,
                body: "the host reply carried no url".to_string(),
            })
    }

    async fn stop(&self, box_id: &str) -> BoxResult<()> {
        let _ = self.client.stop(box_id).await?;
        Ok(())
    }

    async fn resume(&self, box_id: &str) -> BoxResult<()> {
        let _ = self.client.resume(box_id).await?;
        Ok(())
    }

    async fn destroy(&self, box_id: &str) -> BoxResult<()> {
        let _ = self.client.delete_box(box_id).await?;
        Ok(())
    }

    async fn state(&self, box_id: &str) -> BoxResult<String> {
        // GET /boxes/{id} returns box.info with the real `state` (confirmed against the live API):
        // "idle"/"running"/"busy" mean up, and 404 means the box is gone. An earlier version probed a
        // file read, but ascii restricts reads to /home/user and /tmp, so it 400'd on any system path
        // and reported EVERY running box as "stopped". Read the actual state instead.
        match self.client.get_box(box_id).await {
            Err(BoxError::NoSuchBox) => Ok("absent".to_string()),
            Err(BoxError::Refused { .. }) => Ok("stopped".to_string()),
            Err(error) => Err(error),
            Ok(info) => Ok(match info.box_.state.as_str() {
                "idle" | "running" | "busy" | "ready" | "active" => "running".to_string(),
                "" => "running".to_string(),
                other => other.to_string(),
            }),
        }
    }

    async fn screen_url(&self, box_id: &str) -> BoxResult<Option<String>> {
        // POST /boxes/{id}/desktop?vnc=1 provisions (first call) then returns a noVNC URL
        // (`desktopUrl`) once ready — confirmed live. Idempotent: polling returns the same URL, so a
        // status poll can call it. While it is still provisioning there is no URL yet ⇒ `None`, and
        // the client shows "preparing" until a later poll carries the link.
        match self.client.desktop(box_id).await {
            Ok(desktop) => Ok(desktop.desktop_url.filter(|url| !url.is_empty())),
            Err(BoxError::Refused { .. } | BoxError::NoSuchBox) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::CommandOutput;

    #[test]
    fn process_status_accepts_a_numeric_process_id() {
        let started: CommandStarted =
            serde_json::from_str(r#"{"type":"command.started","processId":15182,"running":true}"#)
                .unwrap();
        assert_eq!(started.process_id, "15182");
        assert!(started.running);

        let s: CommandStarted =
            serde_json::from_str(r#"{"processId":"proc_abc","running":false}"#).unwrap();
        assert_eq!(s.process_id, "proc_abc");
    }

    #[test]
    fn a_created_box_id_is_read_from_the_documented_envelope() {
        let documented: CreateBoxResponse = serde_json::from_str(
            r#"{"ok":true,"type":"box.created","status":"provisioning","ttlSeconds":3600,
                "box":{"id":"bx_23456789","name":"Box","state":"provisioning",
                       "desktopAvailable":false,"snapshotAvailable":false}}"#,
        )
        .unwrap();
        assert_eq!(documented.id(), Some("bx_23456789"));
    }

    #[test]
    fn a_created_box_id_is_found_under_legacy_spellings() {
        let flat: CreateBoxResponse = serde_json::from_str(r#"{"id":"box_1"}"#).unwrap();
        assert_eq!(flat.id(), Some("box_1"));

        let camel: CreateBoxResponse = serde_json::from_str(r#"{"boxId":"box_2"}"#).unwrap();
        assert_eq!(camel.id(), Some("box_2"));
    }

    #[test]
    fn a_reply_without_an_id_is_none_rather_than_empty() {
        let none: CreateBoxResponse = serde_json::from_str(r#"{"status":"provisioning"}"#).unwrap();
        assert_eq!(none.id(), None);
    }

    #[test]
    fn a_missing_exit_code_is_not_success() {
        let finished: CommandFinished =
            serde_json::from_str(r#"{"stdout":"","stderr":"killed"}"#).unwrap();
        let output: CommandOutput = finished.into();
        assert_eq!(output.exit_code, -1);
        assert_ne!(
            output.exit_code, 0,
            "a signal death must not read as success"
        );
    }

    #[test]
    fn truncation_survives_into_the_output() {
        let raw = r#"{"exitCode":0,"stdout":"a","stderr":"","stdoutTruncated":true,
            "stderrTruncated":false,"timedOut":false}"#;
        let finished: CommandFinished = serde_json::from_str(raw).unwrap();
        let output: CommandOutput = finished.into();
        assert!(output.stdout_truncated);
        assert!(!output.stderr_truncated);
    }

    #[test]
    fn a_timed_out_command_says_so() {
        let raw = r#"{"exitCode":124,"stdout":"","stderr":"","timedOut":true}"#;
        let output: CommandOutput = serde_json::from_str::<CommandFinished>(raw).unwrap().into();
        assert!(output.timed_out);
    }

    #[test]
    fn unknown_fields_do_not_break_a_reply() {
        let raw = r#"{"type":"command.finished","success":true,"exitCode":0,"stdout":"hi",
            "stderr":"","startedAt":"now","somethingNew":42}"#;
        let output: CommandOutput = serde_json::from_str::<CommandFinished>(raw).unwrap().into();
        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stdout, "hi");
    }

    #[test]
    fn a_running_process_reports_itself_as_running() {
        let raw = r#"{"processId":"p1","running":true,"stdout":"partial","stderr":""}"#;
        let started: StartedCommand = serde_json::from_str::<CommandStatus>(raw).unwrap().into();
        assert_eq!(started.process_id, "p1");
        assert!(started.running);
        assert_eq!(started.exit_code, None, "a running process has not exited");
    }

    #[test]
    fn a_preview_url_is_read_under_either_spelling() {
        let plain: HostedPort =
            serde_json::from_str(r#"{"url":"https://a.on.ascii.dev"}"#).unwrap();
        assert_eq!(plain.url(), Some("https://a.on.ascii.dev"));
        let camel: HostedPort =
            serde_json::from_str(r#"{"previewUrl":"https://b.on.ascii.dev"}"#).unwrap();
        assert_eq!(camel.url(), Some("https://b.on.ascii.dev"));
    }

    #[test]
    fn the_client_does_not_print_its_key() {
        let boxes = AsciiBoxes::new("box_supersecret");
        let printed = format!("{boxes:?}");
        assert!(!printed.contains("box_supersecret"), "{printed}");
        assert!(printed.contains("<redacted>"));
    }

    #[test]
    fn the_base_url_can_be_pointed_somewhere_else_for_testing() {
        let boxes = AsciiBoxes::new("k").with_base_url("http://127.0.0.1:9999/api");
        assert_eq!(
            boxes.client().url("/boxes"),
            "http://127.0.0.1:9999/api/boxes"
        );
    }

    #[test]
    fn an_error_envelope_names_the_vendor_code() {
        let env: super::types::ErrorEnvelope = serde_json::from_str(
            r#"{"ok":false,"type":"box.error","status":409,"code":"provider_not_configured",
                "message":"Prompting is locked","error":{"code":"provider_not_configured",
                "message":"Prompting is locked","status":409},"requestId":"req_1"}"#,
        )
        .unwrap();
        assert_eq!(env.code, "provider_not_configured");
        assert_eq!(env.status, 409);
    }
}
