//! Drives the `Computer` trait against a stand-in box API.
//!
//! The unit tests in `ascii/` prove we can READ the vendor's replies. These prove we make the
//! right CALLS — the URL, the bearer token, the request body, and what we do with a refusal. That
//! gap is where an integration actually fails: a client that parses perfectly and posts to the
//! wrong path is green in unit tests and broken in production.
//!
//! It is a stand-in, not the vendor, so it cannot prove the vendor agrees with the reference doc.
//! The created-box id field and the DELETE confirmation header (`X-Ascii-Confirm-Delete`) were
//! confirmed against the real service (a live key, 2026-08-31); this stand-in mirrors them.

// `expect`/`panic` are denied workspace-wide because a panic in the server is a coworker's work
// lost. In a test they are the correct failure: a wrong assertion should stop the test loudly.
#![allow(clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use opengrok_box::{AsciiBoxes, BoxError, Computer};
use serde_json::{Value, json};

/// What the stand-in saw, so a test can assert on the request rather than only the reply.
#[derive(Debug, Default, Clone)]
struct Recorded {
    calls: Arc<Mutex<Vec<(String, String, Value)>>>,
    auth: Arc<Mutex<Option<String>>>,
}

impl Recorded {
    fn record(&self, method: &str, path: &str, body: Value, headers: &HeaderMap) {
        if let Ok(mut calls) = self.calls.lock() {
            calls.push((method.to_string(), path.to_string(), body));
        }
        if let Ok(mut auth) = self.auth.lock()
            && let Some(value) = headers.get("authorization")
        {
            *auth = value.to_str().ok().map(str::to_string);
        }
    }

    fn last(&self) -> (String, String, Value) {
        self.calls
            .lock()
            .ok()
            .and_then(|calls| calls.last().cloned())
            .unwrap_or_default()
    }

    fn auth(&self) -> Option<String> {
        self.auth.lock().ok().and_then(|auth| auth.clone())
    }
}

async fn start_stand_in(router: Router<Recorded>) -> (String, Recorded) {
    let recorded = Recorded::default();
    let app = router.with_state(recorded.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind an ephemeral port");
    let addr = listener.local_addr().expect("read the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), recorded)
}

fn boxes(base: &str) -> AsciiBoxes {
    AsciiBoxes::new("box_test_key").with_base_url(base.to_string())
}

fn full_api(recorded_ok: Value) -> Router<Recorded> {
    Router::new()
        .route(
            "/boxes",
            post({
                let reply = recorded_ok.clone();
                move |State(state): State<Recorded>, headers: HeaderMap, Json(body): Json<Value>| {
                    let reply = reply.clone();
                    async move {
                        let mut recorded = body;
                        if let Some(key) = headers
                            .get("idempotency-key")
                            .and_then(|value| value.to_str().ok())
                        {
                            recorded["_idempotencyKey"] = json!(key);
                        }
                        state.record("POST", "/boxes", recorded, &headers);
                        Json(reply)
                    }
                }
            }),
        )
        .route(
            "/boxes/{id}/commands",
            post(
                |State(state): State<Recorded>,
                 Path(id): Path<String>,
                 headers: HeaderMap,
                 Json(body): Json<Value>| async move {
                    state.record("POST", &format!("/boxes/{id}/commands"), body.clone(), &headers);
                    let detached = body.get("detached").and_then(Value::as_bool) == Some(true);
                    Json(if detached {
                        json!({"type":"command.started","processId":"p1","running":true,
                               "stdout":"","stderr":""})
                    } else {
                        json!({"type":"command.finished","success":true,"exitCode":0,
                               "stdout":"hello from the box","stderr":"",
                               "stdoutTruncated":false,"stderrTruncated":false,"timedOut":false})
                    })
                },
            ),
        )
        .route(
            "/boxes/{id}/commands/{pid}",
            get(
                |State(state): State<Recorded>, Path((id, pid)): Path<(String, String)>, headers: HeaderMap| async move {
                    state.record("GET", &format!("/boxes/{id}/commands/{pid}"), json!(null), &headers);
                    Json(json!({"processId": pid, "running": false, "exitCode": 0,
                                "stdout":"done","stderr":""}))
                },
            ),
        )
        .route(
            "/boxes/{id}/files",
            get(
                |State(state): State<Recorded>, Path(id): Path<String>, Query(query): Query<Value>, headers: HeaderMap| async move {
                    state.record("GET", &format!("/boxes/{id}/files"), query, &headers);
                    Json(json!({"type":"file.read","success":true,"encoding":"utf8",
                                "content":"file contents"}))
                },
            )
            .put(
                |State(state): State<Recorded>, Path(id): Path<String>, headers: HeaderMap, Json(body): Json<Value>| async move {
                    state.record("PUT", &format!("/boxes/{id}/files"), body, &headers);
                    Json(json!({"type":"file.written","success":true}))
                },
            ),
        )
        .route(
            "/boxes/{id}/host",
            post(
                |State(state): State<Recorded>, Path(id): Path<String>, headers: HeaderMap, Json(body): Json<Value>| async move {
                    state.record("POST", &format!("/boxes/{id}/host"), body, &headers);
                    Json(json!({"url":"https://box-3000.on.ascii.dev?_token=abc"}))
                },
            ),
        )
        .route(
            "/boxes/{id}/stop",
            post(|State(state): State<Recorded>, Path(id): Path<String>, headers: HeaderMap| async move {
                state.record("POST", &format!("/boxes/{id}/stop"), json!(null), &headers);
                Json(json!({"ok": true}))
            }),
        )
        .route(
            "/boxes/{id}/resume",
            post(|State(state): State<Recorded>, Path(id): Path<String>, headers: HeaderMap| async move {
                state.record("POST", &format!("/boxes/{id}/resume"), json!(null), &headers);
                Json(json!({"ok": true}))
            }),
        )
        .route(
            "/boxes/{id}",
            delete(|State(state): State<Recorded>, Path(id): Path<String>, headers: HeaderMap| async move {
                let confirm = headers
                    .get("x-ascii-confirm-delete")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                state.record("DELETE", &format!("/boxes/{id}"), json!({"confirm": confirm}), &headers);
                Json(json!({"ok": true}))
            }),
        )
}

#[tokio::test]
async fn creating_a_box_posts_a_ttl_and_returns_its_id() {
    let (base, recorded) = start_stand_in(full_api(json!({"id": "box_abc"}))).await;
    let id = boxes(&base).create(Some(1800)).await.expect("create a box");

    assert_eq!(id, "box_abc");
    let (method, path, body) = recorded.last();
    assert_eq!((method.as_str(), path.as_str()), ("POST", "/boxes"));
    assert_eq!(body["ttlSeconds"], 1800);
}

/// The key opens every door, and it must travel as a bearer token.
#[tokio::test]
async fn every_call_carries_the_bearer_token() {
    let (base, recorded) = start_stand_in(full_api(json!({"id": "box_abc"}))).await;
    boxes(&base).create(None).await.expect("create a box");
    assert_eq!(recorded.auth().as_deref(), Some("Bearer box_test_key"));
}

#[tokio::test]
async fn a_default_ttl_is_sent_rather_than_nothing() {
    let (base, recorded) = start_stand_in(full_api(json!({"id": "box_abc"}))).await;
    boxes(&base).create(None).await.expect("create a box");
    // A box with no TTL is a box that bills forever if something forgets to stop it.
    assert_eq!(recorded.last().2["ttlSeconds"], 3600);
}

#[tokio::test]
async fn running_a_command_returns_its_output() {
    let (base, recorded) = start_stand_in(full_api(json!({"id": "box_abc"}))).await;
    let output = boxes(&base)
        .run("box_abc", "echo hello", 30)
        .await
        .expect("run a command");

    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout, "hello from the box");
    let (_, path, body) = recorded.last();
    assert_eq!(path, "/boxes/box_abc/commands");
    assert_eq!(body["command"], "echo hello");
    assert_eq!(body["detached"], false);
}

/// The API accepts 1–600. A larger value is rejected upstream, so it is clamped here.
#[tokio::test]
async fn an_over_long_timeout_is_clamped_to_what_the_api_accepts() {
    let (base, recorded) = start_stand_in(full_api(json!({"id": "box_abc"}))).await;
    boxes(&base)
        .run("box_abc", "sleep 9999", 100_000)
        .await
        .expect("run a command");
    assert_eq!(recorded.last().2["timeoutSeconds"], 600);
}

#[tokio::test]
async fn a_zero_timeout_is_raised_to_the_minimum() {
    let (base, recorded) = start_stand_in(full_api(json!({"id": "box_abc"}))).await;
    boxes(&base)
        .run("box_abc", "true", 0)
        .await
        .expect("run a command");
    assert_eq!(recorded.last().2["timeoutSeconds"], 1);
}

/// `start` and `watch` are separate because the box has no live stdout socket — a caller that
/// wants output while it happens must poll, and this proves both halves reach the right paths.
#[tokio::test]
async fn a_detached_command_starts_and_is_polled() {
    let (base, recorded) = start_stand_in(full_api(json!({"id": "box_abc"}))).await;
    let computer = boxes(&base);

    let started = computer
        .start("box_abc", "long-running")
        .await
        .expect("start a command");
    assert_eq!(started.process_id, "p1");
    assert!(started.running);
    assert_eq!(recorded.last().2["detached"], true);

    let polled = computer
        .watch("box_abc", "p1")
        .await
        .expect("poll a command");
    assert!(!polled.running);
    assert_eq!(polled.exit_code, Some(0));
    assert_eq!(recorded.last().1, "/boxes/box_abc/commands/p1");
}

#[tokio::test]
async fn files_are_read_and_written_at_the_documented_paths() {
    let (base, recorded) = start_stand_in(full_api(json!({"id": "box_abc"}))).await;
    let computer = boxes(&base);

    let content = computer
        .read_file("box_abc", "/tmp/a.txt")
        .await
        .expect("read a file");
    assert_eq!(content, "file contents");
    let (method, path, query) = recorded.last();
    assert_eq!(
        (method.as_str(), path.as_str()),
        ("GET", "/boxes/box_abc/files")
    );
    assert_eq!(query["path"], "/tmp/a.txt");
    assert_eq!(query["encoding"], "utf8");

    computer
        .write_file("box_abc", "/tmp/b.txt", "written")
        .await
        .expect("write a file");
    let (method, path, body) = recorded.last();
    assert_eq!(
        (method.as_str(), path.as_str()),
        ("PUT", "/boxes/box_abc/files")
    );
    assert_eq!(body["path"], "/tmp/b.txt");
    assert_eq!(body["content"], "written");
}

#[tokio::test]
async fn exposing_a_port_returns_a_url_a_person_can_open() {
    let (base, recorded) = start_stand_in(full_api(json!({"id": "box_abc"}))).await;
    let url = boxes(&base)
        .expose_port("box_abc", 3000, "the app")
        .await
        .expect("expose a port");
    assert!(url.starts_with("https://"), "{url}");
    let (_, path, body) = recorded.last();
    assert_eq!(path, "/boxes/box_abc/host");
    assert_eq!(body["port"], 3000);
    assert_eq!(body["title"], "the app");
}

/// Stop keeps the disk and pauses billing; resume brings the same filesystem back. That pair is
/// what lets an agent's computer sleep between turns instead of being rebuilt.
#[tokio::test]
async fn a_box_can_be_stopped_and_resumed() {
    let (base, recorded) = start_stand_in(full_api(json!({"id": "box_abc"}))).await;
    let computer = boxes(&base);

    computer.stop("box_abc").await.expect("stop a box");
    assert_eq!(recorded.last().1, "/boxes/box_abc/stop");

    computer.resume("box_abc").await.expect("resume a box");
    assert_eq!(recorded.last().1, "/boxes/box_abc/resume");
}

#[tokio::test]
async fn destroying_a_box_sends_a_confirmation_header() {
    let (base, recorded) = start_stand_in(full_api(json!({"id": "box_abc"}))).await;
    boxes(&base)
        .destroy("box_abc")
        .await
        .expect("destroy a box");
    let (method, path, body) = recorded.last();
    assert_eq!(
        (method.as_str(), path.as_str()),
        ("DELETE", "/boxes/box_abc")
    );
    assert_eq!(body["confirm"], "box_abc");
}

/// A missing box must be `NoSuchBox`, not a generic refusal — a caller retries one and not the
/// other.
#[tokio::test]
async fn a_missing_box_is_reported_as_missing() {
    let router: Router<Recorded> = Router::new().route(
        "/boxes/{id}/stop",
        post(|| async {
            (
                axum::http::StatusCode::NOT_FOUND,
                Json(json!({"error":"no"})),
            )
        }),
    );
    let (base, _) = start_stand_in(router).await;
    let error = boxes(&base)
        .stop("box_gone")
        .await
        .expect_err("should fail");
    assert!(matches!(error, BoxError::NoSuchBox), "{error:?}");
}

/// A refusal must carry the status and the body, because that is all a human has to debug with.
#[tokio::test]
async fn a_refusal_carries_its_status_and_body() {
    let router: Router<Recorded> = Router::new().route(
        "/boxes",
        post(|| async {
            (
                axum::http::StatusCode::TOO_MANY_REQUESTS,
                Json(json!({"error":"box creation rate limit reached"})),
            )
        }),
    );
    let (base, _) = start_stand_in(router).await;
    let error = boxes(&base).create(None).await.expect_err("should fail");
    match error {
        BoxError::Refused { status, body } => {
            assert_eq!(status, 429);
            assert!(body.contains("rate limit"), "{body}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// A create reply with no id must fail loudly here rather than producing a box id of "" that fails
/// somewhere far from the mistake.
#[tokio::test]
async fn a_create_reply_without_an_id_fails_loudly() {
    let (base, _) = start_stand_in(full_api(json!({"status": "provisioning"}))).await;
    let error = boxes(&base).create(None).await.expect_err("should fail");
    match error {
        BoxError::Refused { body, .. } => assert!(body.contains("no box id"), "{body}"),
        other => panic!("expected a refusal naming the missing id, got {other:?}"),
    }
}

/// An unreachable host is not a refusal: one is retried, the other is not.
#[tokio::test]
async fn an_unreachable_box_api_is_not_a_refusal() {
    // Port 1 on loopback: nothing listens, and the connection is refused immediately.
    let computer = AsciiBoxes::new("k").with_base_url("http://127.0.0.1:1");
    let error = computer.create(None).await.expect_err("should fail");
    assert!(matches!(error, BoxError::Unreachable(_)), "{error:?}");
}

/// Documented create envelope is `{ type: box.created, box: { id } }` (`docs/box/api/reference/boxes/create-box.md`).
#[tokio::test]
async fn create_reads_the_documented_box_id() {
    let (base, _) = start_stand_in(full_api(json!({
        "ok": true,
        "type": "box.created",
        "status": "provisioning",
        "ttlSeconds": 3600,
        "box": {
            "id": "bx_23456789",
            "name": "Box",
            "state": "provisioning",
            "desktopAvailable": false,
            "snapshotAvailable": false
        }
    })))
    .await;
    let id = boxes(&base).create(Some(3600)).await.expect("create");
    assert_eq!(id, "bx_23456789");
}

/// `Idempotency-Key` on create is what makes a lost 202 safe to retry.
#[tokio::test]
async fn create_sends_an_idempotency_key_when_asked() {
    let (base, recorded) = start_stand_in(full_api(json!({"id": "box_abc"}))).await;
    boxes(&base)
        .client()
        .create_box(
            &opengrok_box::ascii::CreateBoxRequest::with_ttl(60),
            Some("6f9619ff-8b86-d011-b42d-00cf4fc964ff"),
        )
        .await
        .expect("create");
    // The stand-in records Authorization; the idempotency header is on the request. Prove the
    // SDK accepted the key by succeeding against the same create path.
    let (_, path, body) = recorded.last();
    assert_eq!(path, "/boxes");
    assert_eq!(
        body["_idempotencyKey"],
        "6f9619ff-8b86-d011-b42d-00cf4fc964ff"
    );
}

/// A v1 error envelope's `code` must reach the caller, not only the HTTP status.
#[tokio::test]
async fn a_structured_error_names_the_vendor_code() {
    let router: Router<Recorded> = Router::new().route(
        "/boxes",
        post(|| async {
            (
                axum::http::StatusCode::CONFLICT,
                Json(json!({
                    "ok": false,
                    "type": "box.error",
                    "status": 409,
                    "code": "provider_not_configured",
                    "message": "Prompting is locked",
                    "error": {
                        "code": "provider_not_configured",
                        "message": "Prompting is locked",
                        "status": 409
                    },
                    "requestId": "req_1"
                })),
            )
        }),
    );
    let (base, _) = start_stand_in(router).await;
    let error = boxes(&base).create(None).await.expect_err("should fail");
    match error {
        BoxError::Refused { status, body } => {
            assert_eq!(status, 409);
            assert!(body.contains("provider_not_configured"), "{body}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}
