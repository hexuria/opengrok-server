//! Shapes transcribed from `docs/box/openapi/box-v1.yaml` (fetched 1 Sep 2026).
//!
//! Field names, envelopes, and id patterns (`bx_…`) exist because the vendor documents them.
//! Unknown extra fields must not fail a parse — they add properties; we do not.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// `ErrorEnvelope` — every failed v1 JSON body.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorEnvelope {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub status: u16,
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub request_id: Option<String>,
}

/// `Box` — the resource nested under create/get/list.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BoxRecord {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub state: String,
    #[serde(default, rename = "type")]
    pub machine_type: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub ip: Option<String>,
    #[serde(default)]
    pub desktop_available: bool,
    #[serde(default)]
    pub desktop_url: Option<String>,
    #[serde(default)]
    pub snapshot_available: bool,
    #[serde(default)]
    pub subdomain: Option<String>,
    #[serde(default)]
    pub environment: Option<String>,
}

/// `CreateBoxRequest`.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateBoxRequest {
    #[serde(skip_serializing_if = "Option::is_none", rename = "type")]
    pub machine_type: Option<String>,
    /// `None` omits the field (API default 3600). `Some(None)` sends JSON `null` (no auto-stop).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<Option<u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_env: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setup_script: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

impl CreateBoxRequest {
    pub fn with_ttl(seconds: u64) -> Self {
        Self {
            ttl_seconds: Some(Some(seconds)),
            ..Self::default()
        }
    }
}

/// `CreateBoxResponse` — `{ type: box.created, box: { id } }`.
///
/// Also accepts a bare `id` / `boxId` so a stand-in that has not been updated still works.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateBoxResponse {
    #[serde(default)]
    pub ok: Option<bool>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
    #[serde(default, rename = "box")]
    pub box_: Option<BoxRecord>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default, rename = "boxId")]
    pub box_id: Option<String>,
}

impl CreateBoxResponse {
    /// The new box's id. Documented path is `box.id` (`bx_…`).
    pub fn id(&self) -> Option<&str> {
        self.box_
            .as_ref()
            .map(|b| b.id.as_str())
            .or(self.id.as_deref())
            .or(self.box_id.as_deref())
            .filter(|id| !id.is_empty())
    }
}

/// `BoxInfoResponse` — `GET /boxes/{id}`.
#[derive(Debug, Clone, Deserialize)]
pub struct BoxInfoResponse {
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(rename = "box")]
    pub box_: BoxRecord,
}

/// `BoxListResponse` — `GET /boxes`.
#[derive(Debug, Clone, Deserialize)]
pub struct BoxListResponse {
    #[serde(default)]
    pub boxes: Vec<BoxRecord>,
}

/// `BoxActionResponse` — stop/resume.
#[derive(Debug, Clone, Deserialize)]
pub struct BoxActionResponse {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default, rename = "box")]
    pub box_: Option<BoxRecord>,
}

/// `CommandRequest`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandRequest {
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detached: Option<bool>,
}

/// `command.finished`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandFinished {
    #[serde(default)]
    pub success: Option<bool>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    #[serde(default)]
    pub stdout_truncated: bool,
    #[serde(default)]
    pub stderr_truncated: bool,
    #[serde(default)]
    pub timed_out: bool,
}

/// Read an id the API may send as a JSON string OR a number (OpenAPI: integer processId).
pub(crate) fn de_id_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::String(s) => Ok(s),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        _ => Ok(String::new()),
    }
}

/// `command.started`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandStarted {
    #[serde(default, deserialize_with = "de_id_string")]
    pub process_id: String,
    #[serde(default)]
    pub pid: Option<i64>,
    #[serde(default)]
    pub running: bool,
    #[serde(default)]
    pub command: Option<String>,
}

/// `command.status` — poll of a detached command.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandStatus {
    #[serde(default, deserialize_with = "de_id_string")]
    pub process_id: String,
    #[serde(default)]
    pub running: bool,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub stdout_truncated: bool,
    #[serde(default)]
    pub stderr_truncated: bool,
}

/// `file.read`.
#[derive(Debug, Clone, Deserialize)]
pub struct FileRead {
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub encoding: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
}

/// `file.written`.
#[derive(Debug, Clone, Deserialize)]
pub struct FileWritten {
    #[serde(default)]
    pub success: Option<bool>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
}

/// `port.hosted`. Also accepts `previewUrl` (older replies).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostedPort {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub preview_url: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
}

impl HostedPort {
    pub fn url(&self) -> Option<&str> {
        self.url.as_deref().or(self.preview_url.as_deref())
    }
}

/// `desktop.url`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopReply {
    #[serde(default)]
    pub desktop_url: Option<String>,
    #[serde(default)]
    pub provisioning: Option<bool>,
    #[serde(default)]
    pub mode: Option<String>,
}

/// `POST /boxes/{id}/sshkey`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshKeyReply {
    #[serde(default)]
    pub success: Option<bool>,
    #[serde(default)]
    pub machine_ip: Option<String>,
    #[serde(default)]
    pub ssh_user: Option<String>,
}
