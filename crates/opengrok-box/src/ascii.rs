//! box.ascii.dev — persistent Ubuntu VMs behind a plain REST API.
//!
//! Bearer-token auth, JSON in and out, so a `reqwest` client is the whole integration; there is no
//! Rust SDK and none is needed. Verified surface (docs.ascii.dev/box/api/v1):
//!   POST   /boxes                              create
//!   POST   /boxes/{id}/commands                run — sync, or `detached` for background
//!   GET    /boxes/{id}/commands/{processId}    poll a detached command's tail
//!   GET    /boxes/{id}/files?path=             read
//!   PUT    /boxes/{id}/files                   write
//!   POST   /boxes/{id}/host                    expose a port, get a preview URL
//!   POST   /boxes/{id}/stop | /resume | /fork  lifecycle
//!   DELETE /boxes/{id}                         destroy
//!
//! TWO SHAPES ARE NOT YET PINNED and are marked at their call sites rather than guessed: the field
//! name carrying a created box's id, and the confirmation header `DELETE` requires. The first
//! slice's task is to hit a real box and write them down.
//!
//! THE FILESYSTEM PERSISTS ACROSS STOP AND RESUME. That is what makes this the right first
//! computer: `stop` pauses billing and keeps the disk, so an agent's machine can sleep between
//! turns and wake with its work intact — which is the difference between a coworker and a session.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{BoxError, BoxResult, CommandOutput, Computer, StartedCommand};

pub const DEFAULT_BASE_URL: &str = "https://ascii.dev/api/box/v1";

/// Where the boxes live and the key that opens them.
///
/// The key is read from the environment and never from a coworker's row: a computer is not a
/// credential a client may set, which is the same rule the gateway applies to model keys.
#[derive(Clone)]
pub struct AsciiBoxes {
    pub base_url: String,
    api_key: String,
    http: reqwest::Client,
}

impl std::fmt::Debug for AsciiBoxes {
    /// Hand-written so the key cannot reach a log through a derived `Debug`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsciiBoxes")
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl AsciiBoxes {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key: api_key.into(),
            http: reqwest::Client::new(),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// Every response is checked the same way, so a failure names the status and a bounded body
    /// rather than surfacing as a confusing parse error three frames later.
    async fn json<T: for<'de> Deserialize<'de>>(
        &self,
        response: reqwest::Response,
    ) -> BoxResult<T> {
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| BoxError::Unreachable(error.to_string()))?;

        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(BoxError::NoSuchBox);
        }
        if !status.is_success() {
            return Err(BoxError::Refused {
                status: status.as_u16(),
                // Bounded: an upstream error page must not become a megabyte in our logs.
                body: body.chars().take(500).collect(),
            });
        }
        serde_json::from_str(&body).map_err(|error| BoxError::Refused {
            status: status.as_u16(),
            body: format!("could not read the reply: {error}"),
        })
    }
}

/// The reply to `POST /boxes`.
///
/// THE ID FIELD IS NOT PINNED. The reference documents the endpoint but not the exact key, so all
/// three plausible spellings are accepted rather than one being guessed — and `id()` returns
/// `None` rather than an empty string when none is present, so a wrong guess fails loudly at the
/// call that made the box instead of silently later, on a box id of "".
#[derive(Debug, Clone, Deserialize)]
pub struct CreatedBox {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default, rename = "boxId")]
    pub box_id: Option<String>,
    #[serde(default, rename = "box")]
    pub nested: Option<NestedBox>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NestedBox {
    #[serde(default)]
    pub id: Option<String>,
}

impl CreatedBox {
    pub fn id(&self) -> Option<&str> {
        self.id
            .as_deref()
            .or(self.box_id.as_deref())
            .or_else(|| self.nested.as_ref().and_then(|nested| nested.id.as_deref()))
    }
}

/// `{type:"command.finished", …}` — a synchronous command.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FinishedCommand {
    #[serde(default, rename = "exitCode")]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    #[serde(default, rename = "stdoutTruncated")]
    pub stdout_truncated: bool,
    #[serde(default, rename = "stderrTruncated")]
    pub stderr_truncated: bool,
    #[serde(default, rename = "timedOut")]
    pub timed_out: bool,
}

impl From<FinishedCommand> for CommandOutput {
    fn from(finished: FinishedCommand) -> Self {
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

/// `{type:"command.started", …}` and the poll reply share enough shape to read as one.
#[derive(Debug, Clone, Deserialize)]
pub struct ProcessStatus {
    #[serde(default, rename = "processId")]
    pub process_id: String,
    #[serde(default)]
    pub running: bool,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    #[serde(default, rename = "exitCode")]
    pub exit_code: Option<i32>,
}

impl From<ProcessStatus> for StartedCommand {
    fn from(status: ProcessStatus) -> Self {
        Self {
            process_id: status.process_id,
            running: status.running,
            stdout: status.stdout,
            stderr: status.stderr,
            exit_code: status.exit_code,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct FileRead {
    #[serde(default)]
    pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HostedPort {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default, rename = "previewUrl")]
    pub preview_url: Option<String>,
}

impl HostedPort {
    pub fn url(&self) -> Option<&str> {
        self.url.as_deref().or(self.preview_url.as_deref())
    }
}

#[async_trait]
impl Computer for AsciiBoxes {
    fn kind(&self) -> &'static str {
        "ascii"
    }

    async fn create(&self, ttl_seconds: Option<u64>) -> BoxResult<String> {
        let response = self
            .http
            .post(self.url("/boxes"))
            .bearer_auth(&self.api_key)
            .json(&json!({ "ttlSeconds": ttl_seconds.unwrap_or(3600) }))
            .send()
            .await
            .map_err(|error| BoxError::Unreachable(error.to_string()))?;

        let created: CreatedBox = self.json(response).await?;
        created
            .id()
            .map(str::to_string)
            .ok_or_else(|| BoxError::Refused {
                status: 200,
                // Loud on purpose: see the note on `CreatedBox`.
                body: "the create reply carried no box id under id/boxId/box.id".to_string(),
            })
    }

    async fn run(
        &self,
        box_id: &str,
        command: &str,
        timeout_seconds: u32,
    ) -> BoxResult<CommandOutput> {
        let response = self
            .http
            .post(self.url(&format!("/boxes/{box_id}/commands")))
            .bearer_auth(&self.api_key)
            .json(&json!({
                "command": command,
                // The API accepts 1–600; anything larger is silently rejected upstream, so it is
                // clamped here where the reason can be written down.
                "timeoutSeconds": timeout_seconds.clamp(1, 600),
                "detached": false,
            }))
            .send()
            .await
            .map_err(|error| BoxError::Unreachable(error.to_string()))?;

        let finished: FinishedCommand = self.json(response).await?;
        Ok(finished.into())
    }

    async fn start(&self, box_id: &str, command: &str) -> BoxResult<StartedCommand> {
        let response = self
            .http
            .post(self.url(&format!("/boxes/{box_id}/commands")))
            .bearer_auth(&self.api_key)
            .json(&json!({ "command": command, "detached": true }))
            .send()
            .await
            .map_err(|error| BoxError::Unreachable(error.to_string()))?;

        let started: ProcessStatus = self.json(response).await?;
        Ok(started.into())
    }

    async fn watch(&self, box_id: &str, process_id: &str) -> BoxResult<StartedCommand> {
        let response = self
            .http
            .get(self.url(&format!("/boxes/{box_id}/commands/{process_id}")))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|error| BoxError::Unreachable(error.to_string()))?;

        let status: ProcessStatus = self.json(response).await?;
        Ok(status.into())
    }

    async fn read_file(&self, box_id: &str, path: &str) -> BoxResult<String> {
        let response = self
            .http
            .get(self.url(&format!("/boxes/{box_id}/files")))
            .bearer_auth(&self.api_key)
            .query(&[("path", path), ("encoding", "utf8")])
            .send()
            .await
            .map_err(|error| BoxError::Unreachable(error.to_string()))?;

        let file: FileRead = self.json(response).await?;
        Ok(file.content)
    }

    async fn write_file(&self, box_id: &str, path: &str, content: &str) -> BoxResult<()> {
        let response = self
            .http
            .put(self.url(&format!("/boxes/{box_id}/files")))
            .bearer_auth(&self.api_key)
            .json(&json!({ "path": path, "content": content, "encoding": "utf8" }))
            .send()
            .await
            .map_err(|error| BoxError::Unreachable(error.to_string()))?;

        let _: serde_json::Value = self.json(response).await?;
        Ok(())
    }

    async fn expose_port(&self, box_id: &str, port: u16, title: &str) -> BoxResult<String> {
        let response = self
            .http
            .post(self.url(&format!("/boxes/{box_id}/host")))
            .bearer_auth(&self.api_key)
            .json(&json!({ "port": port, "title": title }))
            .send()
            .await
            .map_err(|error| BoxError::Unreachable(error.to_string()))?;

        let hosted: HostedPort = self.json(response).await?;
        hosted
            .url()
            .map(str::to_string)
            .ok_or_else(|| BoxError::Refused {
                status: 200,
                body: "the host reply carried no url".to_string(),
            })
    }

    async fn stop(&self, box_id: &str) -> BoxResult<()> {
        let response = self
            .http
            .post(self.url(&format!("/boxes/{box_id}/stop")))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|error| BoxError::Unreachable(error.to_string()))?;
        let _: serde_json::Value = self.json(response).await?;
        Ok(())
    }

    async fn resume(&self, box_id: &str) -> BoxResult<()> {
        let response = self
            .http
            .post(self.url(&format!("/boxes/{box_id}/resume")))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|error| BoxError::Unreachable(error.to_string()))?;
        let _: serde_json::Value = self.json(response).await?;
        Ok(())
    }

    async fn destroy(&self, box_id: &str) -> BoxResult<()> {
        // THE CONFIRMATION HEADER IS NOT PINNED. The reference says `DELETE` requires one but does
        // not name it. Sent as the most likely spelling; if a real delete is refused, the error
        // will say so, which is better than quietly not deleting and billing for a box forever.
        let response = self
            .http
            .delete(self.url(&format!("/boxes/{box_id}")))
            .bearer_auth(&self.api_key)
            .header("X-Confirm-Delete", box_id)
            .send()
            .await
            .map_err(|error| BoxError::Unreachable(error.to_string()))?;
        let _: serde_json::Value = self.json(response).await?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// The id field is not pinned, so every plausible spelling must be read.
    #[test]
    fn a_created_box_id_is_found_under_any_of_its_spellings() {
        let flat: CreatedBox = serde_json::from_str(r#"{"id":"box_1"}"#).unwrap();
        assert_eq!(flat.id(), Some("box_1"));

        let camel: CreatedBox = serde_json::from_str(r#"{"boxId":"box_2"}"#).unwrap();
        assert_eq!(camel.id(), Some("box_2"));

        let nested: CreatedBox = serde_json::from_str(r#"{"box":{"id":"box_3"}}"#).unwrap();
        assert_eq!(nested.id(), Some("box_3"));
    }

    /// A reply with no id must be `None`, not `""` — a box id of "" would be used in a URL and
    /// fail somewhere far from the mistake.
    #[test]
    fn a_reply_without_an_id_is_none_rather_than_empty() {
        let none: CreatedBox = serde_json::from_str(r#"{"status":"provisioning"}"#).unwrap();
        assert_eq!(none.id(), None);
    }

    /// A command killed by a signal reports no exit code. Reading that as 0 would tell a coworker
    /// the command succeeded.
    #[test]
    fn a_missing_exit_code_is_not_success() {
        let finished: FinishedCommand =
            serde_json::from_str(r#"{"stdout":"","stderr":"killed"}"#).unwrap();
        let output: CommandOutput = finished.into();
        assert_eq!(output.exit_code, -1);
        assert_ne!(
            output.exit_code, 0,
            "a signal death must not read as success"
        );
    }

    /// Truncation is carried, not dropped: a tail is not the output, and a coworker reasoning over
    /// a silently clipped log reaches confident wrong conclusions.
    #[test]
    fn truncation_survives_into_the_output() {
        let raw = r#"{"exitCode":0,"stdout":"a","stderr":"","stdoutTruncated":true,
            "stderrTruncated":false,"timedOut":false}"#;
        let finished: FinishedCommand = serde_json::from_str(raw).unwrap();
        let output: CommandOutput = finished.into();
        assert!(output.stdout_truncated);
        assert!(!output.stderr_truncated);
    }

    #[test]
    fn a_timed_out_command_says_so() {
        let raw = r#"{"exitCode":124,"stdout":"","stderr":"","timedOut":true}"#;
        let output: CommandOutput = serde_json::from_str::<FinishedCommand>(raw).unwrap().into();
        assert!(output.timed_out);
    }

    /// A field the provider adds tomorrow must not break a command today.
    #[test]
    fn unknown_fields_do_not_break_a_reply() {
        let raw = r#"{"type":"command.finished","success":true,"exitCode":0,"stdout":"hi",
            "stderr":"","startedAt":"now","somethingNew":42}"#;
        let output: CommandOutput = serde_json::from_str::<FinishedCommand>(raw).unwrap().into();
        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stdout, "hi");
    }

    #[test]
    fn a_running_process_reports_itself_as_running() {
        let raw = r#"{"processId":"p1","running":true,"stdout":"partial","stderr":""}"#;
        let started: StartedCommand = serde_json::from_str::<ProcessStatus>(raw).unwrap().into();
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

    /// The key must not be printable, however it is logged.
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
        assert_eq!(boxes.url("/boxes"), "http://127.0.0.1:9999/api/boxes");
    }
}
