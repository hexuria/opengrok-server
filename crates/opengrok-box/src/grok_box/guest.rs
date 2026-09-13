//! Connect-only HTTP client for a running grok-box guest.
//!
//! Shapes transcribed from hexuria/box `docs/API.md` (guest protocol v1). The Computer adapter
//! starts the container and passes the **published** exec/host URLs plus the bearer it minted.
//! This client never runs Docker and never replaces those URLs with `/v1/info.endpoints` (those
//! are container-local listen addresses).
//!
//! BOX_TOKEN never implements Debug and is never interpolated into an error that a log might
//! print. A 401 is `Refused`, not "unreachable", so a caller can tell a bad token from a down box.

use serde::Deserialize;
use serde_json::json;

use crate::{BoxError, BoxResult, CommandOutput};

#[derive(Clone)]
pub(crate) struct Guest {
    exec_url: String,
    host_url: String,
    token: String,
    http: reqwest::Client,
}

impl std::fmt::Debug for Guest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Guest")
            .field("exec_url", &self.exec_url)
            .field("host_url", &self.host_url)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl Guest {
    pub(crate) fn connect(
        exec_url: impl Into<String>,
        host_url: impl Into<String>,
        token: impl Into<String>,
        http: reqwest::Client,
    ) -> Self {
        Self {
            exec_url: trim_slash(exec_url),
            host_url: trim_slash(host_url),
            token: token.into(),
            http,
        }
    }

    /// `GET {host}/v1/ready` with Bearer. 200 means exec is up and (when required) the desktop is.
    pub(crate) async fn ready(&self) -> BoxResult<()> {
        let response = self
            .http
            .get(format!("{}/v1/ready", self.host_url))
            .bearer_auth(&self.token)
            .timeout(std::time::Duration::from_secs(3))
            .send()
            .await
            .map_err(|error| BoxError::Unreachable(error.to_string()))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let body = response.text().await.unwrap_or_default();
        Err(map_status(status.as_u16(), &body))
    }

    pub(crate) async fn exec(
        &self,
        command: &str,
        timeout_seconds: u32,
        stdin: Option<&str>,
    ) -> BoxResult<CommandOutput> {
        let timeout_seconds = timeout_seconds.clamp(1, 600);
        let timeout_ms = u64::from(timeout_seconds) * 1000;
        let mut body = json!({
            "command": command,
            "timeout_ms": timeout_ms,
        });
        if let Some(stdin) = stdin {
            body["stdin"] = json!(stdin);
        }
        let response = self
            .http
            .post(format!("{}/v1/exec", self.exec_url))
            .bearer_auth(&self.token)
            .json(&body)
            .timeout(std::time::Duration::from_millis(
                timeout_ms.saturating_add(5_000),
            ))
            .send()
            .await
            .map_err(|error| BoxError::Unreachable(error.to_string()))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|error| BoxError::Unreachable(error.to_string()))?;
        if !status.is_success() {
            return Err(map_status(status.as_u16(), &text));
        }
        let finished: ExecResponse =
            serde_json::from_str(&text).map_err(|error| BoxError::Refused {
                status: status.as_u16(),
                body: format!("could not read the exec reply: {error}"),
            })?;
        Ok(finished.into())
    }

    pub(crate) async fn read_file(&self, path: &str) -> BoxResult<String> {
        let response = self
            .http
            .get(format!("{}/v1/files", self.exec_url))
            .query(&[("path", path), ("encoding", "utf8")])
            .bearer_auth(&self.token)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .map_err(|error| BoxError::Unreachable(error.to_string()))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|error| BoxError::Unreachable(error.to_string()))?;
        if !status.is_success() {
            return Err(map_status(status.as_u16(), &text));
        }
        let file: FileGet = serde_json::from_str(&text).map_err(|error| BoxError::Refused {
            status: status.as_u16(),
            body: format!("could not read the file reply: {error}"),
        })?;
        if file.kind == "directory" {
            let listing = file
                .entries
                .into_iter()
                .map(|entry| format!("{}\t{}", entry.kind, entry.name))
                .collect::<Vec<_>>()
                .join("\n");
            return Ok(listing);
        }
        Ok(file.content)
    }

    pub(crate) async fn write_file(&self, path: &str, content: &str) -> BoxResult<()> {
        let response = self
            .http
            .put(format!("{}/v1/files", self.exec_url))
            .bearer_auth(&self.token)
            .json(&json!({
                "path": path,
                "content": content,
                "encoding": "utf8",
                "create_dirs": true,
            }))
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .map_err(|error| BoxError::Unreachable(error.to_string()))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let text = response.text().await.unwrap_or_default();
        Err(map_status(status.as_u16(), &text))
    }
}

fn trim_slash(url: impl Into<String>) -> String {
    url.into().trim().trim_end_matches('/').to_string()
}

fn map_status(status: u16, body: &str) -> BoxError {
    if status == 404 {
        let code = guest_error_code(body);
        // A missing *file* is a refusal the model can read. A missing *box* is 404 from a
        // reverse-proxy in front of a gone container — treat unknown 404s as NoSuchBox only when
        // the guest said so, otherwise keep the body.
        if code == "not_found" || body_looks_like_missing_box(body) {
            return BoxError::Refused {
                status,
                body: guest_error_message(body).unwrap_or_else(|| body.chars().take(500).collect()),
            };
        }
    }
    if let Some(message) = guest_error_message(body) {
        return BoxError::Refused {
            status,
            body: message,
        };
    }
    BoxError::Refused {
        status,
        body: body.chars().take(500).collect(),
    }
}

fn guest_error_code(body: &str) -> String {
    serde_json::from_str::<GuestErrorEnvelope>(body)
        .ok()
        .map(|envelope| envelope.error.code)
        .unwrap_or_default()
}

fn guest_error_message(body: &str) -> Option<String> {
    let envelope = serde_json::from_str::<GuestErrorEnvelope>(body).ok()?;
    if envelope.error.message.is_empty() {
        if envelope.error.code.is_empty() {
            None
        } else {
            Some(envelope.error.code)
        }
    } else if envelope.error.code.is_empty() {
        Some(envelope.error.message)
    } else {
        Some(format!(
            "{}: {}",
            envelope.error.code, envelope.error.message
        ))
    }
}

fn body_looks_like_missing_box(body: &str) -> bool {
    let lowered = body.to_lowercase();
    lowered.contains("no such container") || lowered.contains("no such object")
}

#[derive(Debug, Deserialize)]
struct GuestErrorEnvelope {
    error: GuestErrorBody,
}

#[derive(Debug, Deserialize)]
struct GuestErrorBody {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
}

#[derive(Debug, Deserialize)]
struct ExecResponse {
    #[serde(default)]
    stdout: String,
    #[serde(default)]
    stderr: String,
    exit_code: Option<i32>,
    #[serde(default)]
    timed_out: bool,
    #[serde(default)]
    truncated: bool,
}

impl From<ExecResponse> for CommandOutput {
    fn from(finished: ExecResponse) -> Self {
        let timed_out = finished.timed_out;
        Self {
            // Timeout with a null exit code is 124, same convention as DockerComputer, so a
            // coworker reading 0 would not conclude the command succeeded.
            exit_code: if timed_out {
                finished.exit_code.unwrap_or(124)
            } else {
                finished.exit_code.unwrap_or(-1)
            },
            stdout: finished.stdout,
            stderr: finished.stderr,
            stdout_truncated: finished.truncated,
            stderr_truncated: finished.truncated,
            timed_out,
        }
    }
}

#[derive(Debug, Deserialize)]
struct FileGet {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    entries: Vec<DirEntry>,
}

#[derive(Debug, Deserialize)]
struct DirEntry {
    #[serde(default)]
    name: String,
    #[serde(default)]
    kind: String,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use axum::extract::{Query, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[test]
    fn a_guest_debug_line_does_not_carry_the_token() {
        let guest = Guest::connect(
            "http://127.0.0.1:1337",
            "http://127.0.0.1:1340",
            "super-secret-box-token",
            reqwest::Client::new(),
        );
        let rendered = format!("{guest:?}");
        assert!(!rendered.contains("super-secret-box-token"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    #[test]
    fn trailing_slashes_are_stripped_from_urls() {
        let guest = Guest::connect(
            "http://127.0.0.1:1337/",
            "http://127.0.0.1:1340/",
            "tok",
            reqwest::Client::new(),
        );
        assert_eq!(guest.exec_url, "http://127.0.0.1:1337");
        assert_eq!(guest.host_url, "http://127.0.0.1:1340");
    }

    #[derive(Clone, Default)]
    struct Seen {
        files: Arc<Mutex<HashMap<String, String>>>,
        last_command: Arc<Mutex<Option<String>>>,
    }

    fn bearer(headers: &HeaderMap) -> Option<String> {
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    }

    fn refuse_if_wrong_token(
        headers: &HeaderMap,
        expected: &str,
    ) -> Option<axum::response::Response> {
        use axum::response::IntoResponse;
        match bearer(headers) {
            Some(value) if value == format!("Bearer {expected}") => None,
            _ => Some(
                (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error": {"code": "unauthorized", "message": "missing or invalid bearer token", "status": 401}})),
                )
                    .into_response(),
            ),
        }
    }

    async fn start_guest(token: &'static str) -> (String, String, Seen) {
        let seen = Seen::default();
        let exec_state = seen.clone();
        let host_state = seen.clone();
        let exec = Router::new()
            .route(
                "/v1/exec",
                post(
                    move |State(state): State<Seen>,
                          headers: HeaderMap,
                          Json(body): Json<Value>| async move {
                        if let Some(refusal) = refuse_if_wrong_token(&headers, token) {
                            return refusal;
                        }
                        let command = body
                            .get("command")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        if let Ok(mut last) = state.last_command.lock() {
                            *last = Some(command.clone());
                        }
                        if command.contains("exit 3") {
                            return Json(json!({
                                "stdout": "",
                                "stderr": "",
                                "exit_code": 3,
                                "timed_out": false,
                                "duration_ms": 1,
                                "truncated": false,
                                "cwd": "/workspace",
                            }))
                            .into_response();
                        }
                        Json(json!({
                            "stdout": format!("ran:{command}\n"),
                            "stderr": "",
                            "exit_code": 0,
                            "timed_out": false,
                            "duration_ms": 1,
                            "truncated": false,
                            "cwd": "/workspace",
                        }))
                        .into_response()
                    },
                ),
            )
            .route(
                "/v1/files",
                get(
                    move |State(state): State<Seen>,
                          headers: HeaderMap,
                          Query(query): Query<HashMap<String, String>>| async move {
                        if let Some(refusal) = refuse_if_wrong_token(&headers, token) {
                            return refusal;
                        }
                        let path = query.get("path").cloned().unwrap_or_default();
                        let files = state.files.lock().expect("files");
                        match files.get(&path) {
                            Some(content) => Json(json!({
                                "kind": "file",
                                "path": format!("/workspace/{path}"),
                                "encoding": "utf8",
                                "content": content,
                            }))
                            .into_response(),
                            None => (
                                StatusCode::NOT_FOUND,
                                Json(json!({"error": {"code": "not_found", "message": "no such file", "status": 404}})),
                            )
                                .into_response(),
                        }
                    },
                )
                .put(
                    move |State(state): State<Seen>,
                          headers: HeaderMap,
                          Json(body): Json<Value>| async move {
                        if let Some(refusal) = refuse_if_wrong_token(&headers, token) {
                            return refusal;
                        }
                        let path = body
                            .get("path")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let content = body
                            .get("content")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        state.files.lock().expect("files").insert(path.clone(), content);
                        Json(json!({"path": format!("/workspace/{path}"), "bytes_written": 1}))
                            .into_response()
                    },
                ),
            )
            .with_state(exec_state);
        let host = Router::new()
            .route(
                "/v1/ready",
                get(
                    move |State(_state): State<Seen>, headers: HeaderMap| async move {
                        if let Some(refusal) = refuse_if_wrong_token(&headers, token) {
                            return refusal;
                        }
                        Json(json!({
                            "status": "ready",
                            "service": "box-host",
                            "exec_ready": true,
                            "desktop_ready": true,
                        }))
                        .into_response()
                    },
                ),
            )
            .with_state(host_state);

        let exec_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind exec");
        let host_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind host");
        let exec_addr = exec_listener.local_addr().expect("exec addr");
        let host_addr = host_listener.local_addr().expect("host addr");
        tokio::spawn(async move {
            let _ = axum::serve(exec_listener, exec).await;
        });
        tokio::spawn(async move {
            let _ = axum::serve(host_listener, host).await;
        });
        (
            format!("http://{exec_addr}"),
            format!("http://{host_addr}"),
            seen,
        )
    }

    fn guest(exec: &str, host: &str, token: &str) -> Guest {
        Guest::connect(exec, host, token, reqwest::Client::new())
    }

    #[tokio::test]
    async fn ready_and_exec_and_files_speak_the_guest_wire_with_bearer() {
        let token = "guest-wire-token-aaaaaaaaaaaa";
        let (exec, host, seen) = start_guest(token).await;
        let client = guest(&exec, &host, token);

        client.ready().await.expect("ready");
        let output = client
            .exec("echo hello from the box", 5, None)
            .await
            .expect("exec");
        assert_eq!(output.exit_code, 0);
        assert!(
            output.stdout.contains("echo hello from the box"),
            "{output:?}"
        );
        assert_eq!(
            seen.last_command.lock().expect("cmd").as_deref(),
            Some("echo hello from the box")
        );

        let failed = client.exec("exit 3", 5, None).await.expect("failed cmd");
        assert_eq!(failed.exit_code, 3);

        client
            .write_file("notes.txt", "hello workspace")
            .await
            .expect("write");
        let read = client.read_file("notes.txt").await.expect("read");
        assert_eq!(read, "hello workspace");
    }

    #[tokio::test]
    async fn a_wrong_token_is_a_refusal_not_a_success() {
        let (exec, host, _) = start_guest("the-real-token-bbbbbbbbbbbb").await;
        let client = guest(&exec, &host, "wrong-token");
        let error = client.ready().await.expect_err("should refuse");
        match error {
            BoxError::Refused { status: 401, body } => {
                assert!(body.contains("unauthorized"), "{body}");
                assert!(!body.contains("the-real-token"), "{body}");
            }
            other => panic!("expected 401, got {other:?}"),
        }
    }
}
