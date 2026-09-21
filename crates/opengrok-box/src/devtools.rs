//! Chromium's DevTools protocol over a pipe the server holds.
//!
//! The box starts Chromium with `--remote-debugging-pipe` (`box-chromium-pipe`), which reads
//! protocol messages from its fd 3 and writes them to fd 4; the server runs that through
//! `docker exec -i` and keeps both ends. No port is opened and no socket is made inside the
//! box, so nothing there — not the bot's shell — can reach the protocol; when this handle is
//! dropped the pipe closes and Chromium exits.
//!
//! What rides on it: the WebAuthn domain, for a saved passkey. A virtual authenticator is
//! added, the person's key is loaded for one sign-in, and removed after. A password never
//! goes this way.
//!
//! The wire is plain: one JSON object per message, terminated by a NUL byte.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, broadcast, oneshot};

use crate::{BoxError, BoxResult};

/// How long one protocol call may take. Chromium answers in milliseconds; a longer wait means
/// the pipe is dead.
const CALL_TIMEOUT: Duration = Duration::from_secs(15);

/// One passkey as the authenticator takes it: the key in PKCS#8 (base64), the credential id
/// and user handle (base64), for one relying party. Debug never shows the key.
#[derive(Clone, PartialEq, Eq)]
pub struct Passkey {
    pub rp_id: String,
    pub credential_id_b64: String,
    pub user_handle_b64: String,
    pub private_key_b64: String,
    pub user_name: String,
}

impl std::fmt::Debug for Passkey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Passkey")
            .field("rp_id", &self.rp_id)
            .field("user_name", &self.user_name)
            .field("private_key_b64", &"<redacted>")
            .finish()
    }
}

pub struct DevTools {
    child: Child,
    stdin: Mutex<ChildStdin>,
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    events: broadcast::Sender<Value>,
}

impl std::fmt::Debug for DevTools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DevTools")
            .field("pid", &self.child.id())
            .finish()
    }
}

/// A virtual authenticator's shape: an internal, resident-key, user-verifying one, which is
/// what a platform passkey looks like to a site.
pub fn platform_authenticator_options() -> Value {
    json!({
        "protocol": "ctap2",
        "ctapVersion": "ctap2_1",
        "transport": "internal",
        "hasResidentKey": true,
        "hasUserVerification": true,
        "isUserVerified": true,
        "automaticPresenceSimulation": true,
        "defaultBackupEligibility": true,
        "defaultBackupState": true
    })
}

impl DevTools {
    /// Run `docker exec -i <box> box-chromium-pipe <url>` and take its pipe.
    pub async fn spawn(box_id: &str, url: &str) -> BoxResult<Self> {
        let mut child = Command::new("docker")
            .args(["exec", "-i", box_id, "box-chromium-pipe", url])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| {
                BoxError::Unreachable(format!("could not run docker exec: {error}"))
            })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| BoxError::Unreachable("no stdin on the exec".to_string()))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| BoxError::Unreachable("no stdout on the exec".to_string()))?;
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (events, _) = broadcast::channel(256);
        let reader_pending = pending.clone();
        let reader_events = events.clone();
        tokio::spawn(async move {
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                let read = match stdout.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                buffer.extend_from_slice(&chunk[..read]);
                while let Some(end) = buffer.iter().position(|b| *b == 0) {
                    let message: Vec<u8> = buffer.drain(..=end).collect();
                    let Ok(value) = serde_json::from_slice::<Value>(&message[..message.len() - 1])
                    else {
                        continue;
                    };
                    if let Some(id) = value.get("id").and_then(Value::as_u64) {
                        if let Some(sender) = reader_pending.lock().await.remove(&id) {
                            let _ = sender.send(value);
                        }
                    } else if value.get("method").is_some() {
                        let _ = reader_events.send(value);
                    }
                }
            }
            // The pipe closed: every waiting call learns it now rather than at its timeout.
            reader_pending.lock().await.clear();
        });
        let this = Self {
            child,
            stdin: Mutex::new(stdin),
            next_id: AtomicU64::new(1),
            pending,
            events,
        };
        // The first answer is the proof the pipe works.
        this.call("Browser.getVersion", json!({}), None).await?;
        Ok(this)
    }

    /// One protocol call, on the browser or, with a session id, on a page.
    pub async fn call(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> BoxResult<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut message = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session_id {
            message["sessionId"] = Value::String(session.to_string());
        }
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let mut bytes = serde_json::to_vec(&message)
            .map_err(|error| BoxError::Unreachable(format!("could not encode a call: {error}")))?;
        bytes.push(0);
        {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(&bytes).await.map_err(|error| {
                BoxError::Unreachable(format!("the DevTools pipe is closed: {error}"))
            })?;
            stdin.flush().await.map_err(|error| {
                BoxError::Unreachable(format!("the DevTools pipe is closed: {error}"))
            })?;
        }
        let answer = tokio::time::timeout(CALL_TIMEOUT, rx)
            .await
            .map_err(|_| {
                BoxError::Unreachable(format!("{method}: no answer on the DevTools pipe"))
            })?
            .map_err(|_| BoxError::Unreachable(format!("{method}: the DevTools pipe closed")))?;
        if let Some(error) = answer.get("error") {
            let text = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            return Err(BoxError::Refused {
                status: 500,
                body: format!("{method}: {text}"),
            });
        }
        Ok(answer.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Events as they come. Subscribe before the call that causes them.
    pub fn events(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }

    /// Wait for one event by method name (optionally on one session), up to `patience`.
    pub async fn wait_event(
        &self,
        mut receiver: broadcast::Receiver<Value>,
        method: &str,
        session_id: Option<&str>,
        patience: Duration,
    ) -> BoxResult<Value> {
        let deadline = tokio::time::Instant::now() + patience;
        loop {
            let event = tokio::time::timeout_at(deadline, receiver.recv())
                .await
                .map_err(|_| BoxError::Unreachable(format!("no {method} within {patience:?}")))?
                .map_err(|_| BoxError::Unreachable("the event stream ended".to_string()))?;
            let same_session = session_id.is_none_or(|wanted| {
                event.get("sessionId").and_then(Value::as_str) == Some(wanted)
            });
            if event.get("method").and_then(Value::as_str) == Some(method) && same_session {
                return Ok(event.get("params").cloned().unwrap_or(Value::Null));
            }
        }
    }

    /// The page target whose URL contains `url_part` (or the first page), attached flat.
    pub async fn attach_to_page(&self, url_part: Option<&str>) -> BoxResult<String> {
        let targets = self.call("Target.getTargets", json!({}), None).await?;
        let empty = Vec::new();
        let list = targets
            .get("targetInfos")
            .and_then(Value::as_array)
            .unwrap_or(&empty);
        let pages: Vec<&Value> = list
            .iter()
            .filter(|t| t.get("type").and_then(Value::as_str) == Some("page"))
            .collect();
        let chosen = url_part
            .and_then(|part| {
                pages.iter().find(|t| {
                    t.get("url")
                        .and_then(Value::as_str)
                        .is_some_and(|u| u.contains(part))
                })
            })
            .or_else(|| pages.first())
            .ok_or_else(|| BoxError::Unreachable("the browser has no page".to_string()))?;
        let target_id = chosen
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| BoxError::Unreachable("a page without a target id".to_string()))?;
        let attached = self
            .call(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
                None,
            )
            .await?;
        attached
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| BoxError::Unreachable("attach gave no session".to_string()))
    }

    /// The browser's page URLs, for choosing a target and for tests.
    pub async fn page_urls(&self) -> BoxResult<Vec<String>> {
        let targets = self.call("Target.getTargets", json!({}), None).await?;
        Ok(targets
            .get("targetInfos")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|t| t.get("type").and_then(Value::as_str) == Some("page"))
            .filter_map(|t| t.get("url").and_then(Value::as_str).map(str::to_string))
            .collect())
    }

    /// Open a URL in the attached page.
    pub async fn navigate(&self, session_id: &str, url: &str) -> BoxResult<()> {
        self.call("Page.navigate", json!({ "url": url }), Some(session_id))
            .await?;
        Ok(())
    }

    /// Turn the WebAuthn domain on for a page and add a platform-shaped virtual
    /// authenticator. Returns the authenticator id.
    pub async fn add_platform_authenticator(&self, session_id: &str) -> BoxResult<String> {
        self.call(
            "WebAuthn.enable",
            json!({ "enableUI": false }),
            Some(session_id),
        )
        .await?;
        let added = self
            .call(
                "WebAuthn.addVirtualAuthenticator",
                json!({ "options": platform_authenticator_options() }),
                Some(session_id),
            )
            .await?;
        added
            .get("authenticatorId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| BoxError::Unreachable("no authenticator id".to_string()))
    }

    /// Load one passkey into the authenticator. `signCount: -1` means the credential has no
    /// counter, as synced passkeys do, so a site never sees a stale count.
    pub async fn add_credential(
        &self,
        session_id: &str,
        authenticator_id: &str,
        passkey: &Passkey,
    ) -> BoxResult<()> {
        self.call(
            "WebAuthn.addCredential",
            json!({
                "authenticatorId": authenticator_id,
                "credential": {
                    "credentialId": passkey.credential_id_b64,
                    "isResidentCredential": true,
                    "rpId": passkey.rp_id,
                    "privateKey": passkey.private_key_b64,
                    "userHandle": passkey.user_handle_b64,
                    "signCount": -1,
                    "userName": passkey.user_name,
                    "backupEligibility": true,
                    "backupState": true
                }
            }),
            Some(session_id),
        )
        .await?;
        Ok(())
    }

    pub async fn remove_credential(
        &self,
        session_id: &str,
        authenticator_id: &str,
        credential_id_b64: &str,
    ) -> BoxResult<()> {
        self.call(
            "WebAuthn.removeCredential",
            json!({ "authenticatorId": authenticator_id, "credentialId": credential_id_b64 }),
            Some(session_id),
        )
        .await?;
        Ok(())
    }

    /// The credentials the authenticator holds, keys included — used once, right after a
    /// registration, to seal the new key; never left lying around.
    pub async fn credentials(
        &self,
        session_id: &str,
        authenticator_id: &str,
    ) -> BoxResult<Vec<Value>> {
        let got = self
            .call(
                "WebAuthn.getCredentials",
                json!({ "authenticatorId": authenticator_id }),
                Some(session_id),
            )
            .await?;
        Ok(got
            .get("credentials")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    pub async fn remove_authenticator(
        &self,
        session_id: &str,
        authenticator_id: &str,
    ) -> BoxResult<()> {
        self.call(
            "WebAuthn.removeVirtualAuthenticator",
            json!({ "authenticatorId": authenticator_id }),
            Some(session_id),
        )
        .await?;
        let _ = self
            .call("WebAuthn.disable", json!({}), Some(session_id))
            .await;
        Ok(())
    }

    /// Whether the exec (and so the pipe) is still alive.
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_authenticator_looks_like_a_platform_one() {
        let options = platform_authenticator_options();
        assert_eq!(options["transport"], "internal");
        assert_eq!(options["hasResidentKey"], true);
        assert_eq!(options["hasUserVerification"], true);
        assert_eq!(options["isUserVerified"], true);
    }
}
