//! Typed HTTP client for Box Public API v1 (`docs/box/openapi/box-v1.yaml`).
//!
//! This is the SDK. `AsciiBoxes` is the OpenGrok `Computer` adapter on top of it. Methods map
//! 1:1 onto documented paths; they do not invent query names or envelopes.

use serde::de::DeserializeOwned;
use serde_json::json;

use super::types::{
    BoxActionResponse, BoxInfoResponse, BoxListResponse, CommandFinished, CommandRequest,
    CommandStarted, CommandStatus, CreateBoxRequest, CreateBoxResponse, DesktopReply,
    ErrorEnvelope, FileRead, FileWritten, HostedPort, SshKeyReply,
};
use crate::{BoxError, BoxResult};

pub const DEFAULT_BASE_URL: &str = "https://ascii.dev/api/box/v1";

/// Header `DELETE /boxes/{id}` requires, equal to the box id.
/// Provenance: `docs/box/openapi/box-v1.yaml` `ConfirmDelete`; confirmed live 31 Aug 2026.
pub const CONFIRM_DELETE_HEADER: &str = "X-Ascii-Confirm-Delete";

/// A Box API v1 client. The key never implements `Debug`.
#[derive(Clone)]
pub struct Client {
    pub base_url: String,
    api_key: String,
    http: reqwest::Client,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl Client {
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

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.http.get(self.url(path)).bearer_auth(&self.api_key)
    }

    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.http.post(self.url(path)).bearer_auth(&self.api_key)
    }

    fn put(&self, path: &str) -> reqwest::RequestBuilder {
        self.http.put(self.url(path)).bearer_auth(&self.api_key)
    }

    fn patch(&self, path: &str) -> reqwest::RequestBuilder {
        self.http.patch(self.url(path)).bearer_auth(&self.api_key)
    }

    fn delete(&self, path: &str) -> reqwest::RequestBuilder {
        self.http.delete(self.url(path)).bearer_auth(&self.api_key)
    }

    async fn json<T: DeserializeOwned>(&self, response: reqwest::Response) -> BoxResult<T> {
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| BoxError::Unreachable(error.to_string()))?;

        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(BoxError::NoSuchBox);
        }
        if !status.is_success() {
            let detail = serde_json::from_str::<ErrorEnvelope>(&body)
                .ok()
                .filter(|e| !e.code.is_empty() || !e.message.is_empty())
                .map(|e| {
                    if e.message.is_empty() {
                        e.code
                    } else if e.code.is_empty() {
                        e.message
                    } else {
                        format!("{}: {}", e.code, e.message)
                    }
                })
                .unwrap_or_else(|| body.chars().take(500).collect());
            return Err(BoxError::Refused {
                status: status.as_u16(),
                body: detail,
            });
        }
        serde_json::from_str(&body).map_err(|error| BoxError::Refused {
            status: status.as_u16(),
            body: format!("could not read the reply: {error}"),
        })
    }

    async fn send<T: DeserializeOwned>(&self, request: reqwest::RequestBuilder) -> BoxResult<T> {
        let response = request
            .send()
            .await
            .map_err(|error| BoxError::Unreachable(error.to_string()))?;
        self.json(response).await
    }

    /// `POST /boxes`. Optional `Idempotency-Key` makes create safe to retry.
    pub async fn create_box(
        &self,
        request: &CreateBoxRequest,
        idempotency_key: Option<&str>,
    ) -> BoxResult<CreateBoxResponse> {
        let mut req = self.post("/boxes").json(request);
        if let Some(key) = idempotency_key.filter(|k| !k.is_empty()) {
            req = req.header("Idempotency-Key", key);
        }
        self.send(req).await
    }

    /// `GET /boxes/{id}`.
    pub async fn get_box(&self, box_id: &str) -> BoxResult<BoxInfoResponse> {
        self.send(self.get(&format!("/boxes/{box_id}"))).await
    }

    /// `GET /boxes`.
    pub async fn list_boxes(&self) -> BoxResult<BoxListResponse> {
        self.send(self.get("/boxes")).await
    }

    /// `POST /boxes/{id}/stop`.
    pub async fn stop(&self, box_id: &str) -> BoxResult<BoxActionResponse> {
        self.send(self.post(&format!("/boxes/{box_id}/stop"))).await
    }

    /// `POST /boxes/{id}/resume`.
    pub async fn resume(&self, box_id: &str) -> BoxResult<BoxActionResponse> {
        self.send(self.post(&format!("/boxes/{box_id}/resume")))
            .await
    }

    /// `POST /boxes/{id}/fork`.
    pub async fn fork(
        &self,
        box_id: &str,
        idempotency_key: Option<&str>,
    ) -> BoxResult<CreateBoxResponse> {
        let mut req = self.post(&format!("/boxes/{box_id}/fork")).json(&json!({}));
        if let Some(key) = idempotency_key.filter(|k| !k.is_empty()) {
            req = req.header("Idempotency-Key", key);
        }
        self.send(req).await
    }

    /// `DELETE /boxes/{id}` with `X-Ascii-Confirm-Delete`.
    pub async fn delete_box(&self, box_id: &str) -> BoxResult<serde_json::Value> {
        self.send(
            self.delete(&format!("/boxes/{box_id}"))
                .header(CONFIRM_DELETE_HEADER, box_id),
        )
        .await
    }

    /// `POST /boxes/{id}/commands` with `detached: false`.
    pub async fn run_command(
        &self,
        box_id: &str,
        mut request: CommandRequest,
    ) -> BoxResult<CommandFinished> {
        if let Some(timeout) = request.timeout_seconds.as_mut() {
            *timeout = (*timeout).clamp(1, 600);
        }
        request.detached = Some(false);
        self.send(
            self.post(&format!("/boxes/{box_id}/commands"))
                .json(&request),
        )
        .await
    }

    /// `POST /boxes/{id}/commands` with `detached: true`.
    pub async fn start_command(
        &self,
        box_id: &str,
        mut request: CommandRequest,
    ) -> BoxResult<CommandStarted> {
        request.detached = Some(true);
        self.send(
            self.post(&format!("/boxes/{box_id}/commands"))
                .json(&request),
        )
        .await
    }

    /// `GET /boxes/{id}/commands/{processId}`.
    pub async fn command_status(&self, box_id: &str, process_id: &str) -> BoxResult<CommandStatus> {
        self.send(self.get(&format!("/boxes/{box_id}/commands/{process_id}")))
            .await
    }

    /// `GET /boxes/{id}/files?path=&encoding=utf8`.
    pub async fn read_file(&self, box_id: &str, path: &str) -> BoxResult<FileRead> {
        self.send(
            self.get(&format!("/boxes/{box_id}/files"))
                .query(&[("path", path), ("encoding", "utf8")]),
        )
        .await
    }

    /// `PUT /boxes/{id}/files`.
    pub async fn write_file(
        &self,
        box_id: &str,
        path: &str,
        content: &str,
    ) -> BoxResult<FileWritten> {
        self.send(
            self.put(&format!("/boxes/{box_id}/files"))
                .json(&json!({ "path": path, "content": content, "encoding": "utf8" })),
        )
        .await
    }

    /// `POST /boxes/{id}/host`.
    pub async fn host_port(&self, box_id: &str, port: u16, title: &str) -> BoxResult<HostedPort> {
        self.send(
            self.post(&format!("/boxes/{box_id}/host"))
                .json(&json!({ "port": port, "title": title })),
        )
        .await
    }

    /// `POST /boxes/{id}/desktop?vnc=1`.
    pub async fn desktop(&self, box_id: &str) -> BoxResult<DesktopReply> {
        self.send(
            self.post(&format!("/boxes/{box_id}/desktop"))
                .query(&[("vnc", "1")]),
        )
        .await
    }

    /// `POST /boxes/{id}/interrupt`.
    pub async fn interrupt(&self, box_id: &str) -> BoxResult<serde_json::Value> {
        self.send(self.post(&format!("/boxes/{box_id}/interrupt")))
            .await
    }

    /// `POST /boxes/{id}/sshkey`.
    pub async fn configure_ssh_key(
        &self,
        box_id: &str,
        public_key: &str,
    ) -> BoxResult<SshKeyReply> {
        self.send(
            self.post(&format!("/boxes/{box_id}/sshkey"))
                .json(&json!({ "key": public_key })),
        )
        .await
    }

    /// `PATCH /boxes/{id}`.
    pub async fn update_box(
        &self,
        box_id: &str,
        body: &serde_json::Value,
    ) -> BoxResult<BoxInfoResponse> {
        self.send(self.patch(&format!("/boxes/{box_id}")).json(body))
            .await
    }
}
