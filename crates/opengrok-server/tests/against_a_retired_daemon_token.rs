//! A daemon token retired by a revoke or a re-enrolment keeps no command stream (#299). The
//! `GET /local-exec/requests` stream it opened ends at once, sent nothing more; the token opens
//! nothing again; and a command meanwhile finds no stream to go down. The re-enrolled machine's
//! new token streams as before. #298 did the same for the relay stream.
//!
//! Over HTTP on the local-exec routes alone, so the wire corpus, which the full router records,
//! is left as it is. Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::AccountId;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_store::PgStore;
use serde_json::{Value, json};
use tokio::sync::mpsc;

macro_rules! database_or_skip {
    () => {
        match std::env::var("OG_DATABASE_URL") {
            Ok(url) => opengrok_store::gate_database_or_panic(url),
            Err(_) => {
                eprintln!("skipping: OG_DATABASE_URL is not set");
                return;
            }
        }
    };
}

/// The local-exec routes on loopback, and the bearer of a person signed in to them.
#[derive(Clone)]
struct Desk {
    base: String,
    token: String,
}

async fn desk(database_url: &str) -> Desk {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let (account, at_ms) = (AccountId::new(), chrono::Utc::now().timestamp_millis());
    let email = format!("retired-{}@og.local", uuid::Uuid::now_v7().simple());
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.clone(),
            password_hash: "x".to_string(),
            first_name: "Ada".to_string(),
            last_name: "Mac".to_string(),
            org_id: String::new(),
            plan: Plan::Ultra,
            verified: true,
            enabled: true,
            at_ms,
        })
        .expect("register");
    let view = AccountView {
        id: account.clone(),
        email: email.clone(),
        plan: Plan::Ultra,
        trial: false,
        updated_at_ms: at_ms,
        password_hash: Some("x".to_string()),
        first_name: "Ada".to_string(),
        last_name: "Mac".to_string(),
        org_id: None,
        verified: true,
        enabled: true,
        avatar_url: None,
    };
    store
        .append_account(&account, 0, &events, &view)
        .await
        .expect("append account");
    let minter = Arc::new(TokenMinter::new(b"a-retired-daemon-token"));
    let now = chrono::Utc::now().timestamp();
    let token = minter
        .mint_access(account.as_str(), "sess-retired", &email, "ultra", now, 3600)
        .expect("mint access");
    let app = opengrok_server::local_exec::router(AuthState::new(store, minter, email));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    Desk { base, token }
}

impl Desk {
    /// A route, as the person or, with its daemon token, as a machine.
    async fn send(
        &self,
        bearer: &str,
        method: reqwest::Method,
        path: &str,
        body: Value,
    ) -> (u16, Value) {
        let response = reqwest::Client::new()
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(bearer)
            .json(&body)
            .send()
            .await
            .expect("send");
        let status = response.status().as_u16();
        let text = response.text().await.expect("body");
        let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
        (status, body)
    }

    /// Enrol a machine, or re-enrol `machine`: its id and its new daemon token. Its mode is
    /// `bypass`, so a command of the person's own goes to it with no card.
    async fn enrol(&self, machine: Option<&str>) -> (String, String) {
        let body = json!({ "label": "Ada's Mac", "machineId": machine });
        let post = reqwest::Method::POST;
        let (status, enrolled) = self
            .send(&self.token, post, "/local-exec/daemon", body)
            .await;
        assert_eq!(status, 200, "{enrolled}");
        let id = enrolled["machineId"].as_str().unwrap().to_string();
        let mode = json!({ "machineId": id, "mode": "bypass" });
        let put = reqwest::Method::PUT;
        let set = self
            .send(&self.token, put, "/local-exec/policy", mode)
            .await;
        assert_eq!(set.0, 204, "{}", set.1);
        (id, enrolled["token"].as_str().unwrap().to_string())
    }

    /// Open the command stream as a daemon: the status, and each `data:` frame it is sent,
    /// until the stream ends and the receiver closes.
    async fn open(&self, daemon: &str) -> (u16, mpsc::UnboundedReceiver<Value>) {
        let response = reqwest::Client::new()
            .get(format!("{}/local-exec/requests", self.base))
            .bearer_auth(daemon)
            .send()
            .await
            .expect("open");
        let (status, mut body) = (response.status().as_u16(), response.bytes_stream());
        let (sent, frames) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut pending = String::new();
            while let Some(Ok(chunk)) = body.next().await {
                pending.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(end) = pending.find("\n\n") {
                    let event: String = pending.drain(..end + 2).collect();
                    for data in event.lines().filter_map(|line| line.strip_prefix("data: ")) {
                        let _ = sent.send(serde_json::from_str(data).unwrap_or(Value::Null));
                    }
                }
            }
        });
        (status, frames)
    }

    /// `POST /local-exec/run`: the person's own command on `machine`, waited for.
    async fn run(self, machine: String) -> (u16, Value) {
        let body = json!({ "machineId": machine, "command": "echo hi" });
        let post = reqwest::Method::POST;
        self.send(&self.token, post, "/local-exec/run", body).await
    }
}

/// Every frame left on a stream, until it ends. One that has not ended in five seconds was not
/// closed.
async fn rest_of(mut frames: mpsc::UnboundedReceiver<Value>) -> Vec<Value> {
    let ending = async {
        let mut rest = Vec::new();
        while let Some(frame) = frames.recv().await {
            rest.push(frame);
        }
        rest
    };
    let ended = tokio::time::timeout(Duration::from_secs(5), ending).await;
    ended.expect("the stream ends")
}

/// RE-ENROLMENT ROTATES THE TOKEN, AND THE OLD ONE'S STREAM GOES WITH IT (#299). It ran on, and
/// the machine's commands went down it until the new token's daemon connected, though no result
/// from it was taken.
#[tokio::test]
async fn re_enrolling_a_machine_closes_the_command_stream_its_old_token_opened() {
    let database_url = database_or_skip!();
    let d = desk(&database_url).await;
    let (machine, old) = d.enrol(None).await;
    let (status, mut frames) = d.open(&old).await;
    assert_eq!(status, 200);
    assert_eq!(frames.recv().await.unwrap()["kind"], "welcome");

    let (again, new) = d.enrol(Some(&machine)).await;
    assert_eq!(again, machine);
    assert_eq!(
        rest_of(frames).await,
        Vec::<Value>::new(),
        "ends, sent nothing"
    );
    let (status, refused) = d.clone().run(machine.clone()).await;
    assert_eq!(status, 403, "no stream holds the machine: {refused}");
    assert!(refused.to_string().contains("not connected"), "{refused}");
    assert_eq!(d.open(&old).await.0, 401, "the old token opens nothing");

    let (status, mut frames) = d.open(&new).await;
    assert_eq!(status, 200);
    assert_eq!(frames.recv().await.unwrap()["kind"], "welcome");
    let ran = tokio::spawn(d.clone().run(machine.clone()));
    let exec = frames.recv().await.unwrap();
    assert_eq!(exec["kind"], "exec", "{exec}");
    let result = json!({ "shellResult": { "success": { "exitCode": 0, "stdout": "hi\n" } } });
    let frame = json!({ "kind": "client", "requestId": exec["requestId"], "message": result });
    let answer = json!({ "providerId": machine, "frames": [frame] });
    let post = reqwest::Method::POST;
    let taken = d.send(&new, post, "/local-exec/responses", answer).await;
    assert_eq!(taken.0, 204, "the new token's result is taken");
    let (status, outcome) = ran.await.unwrap();
    assert_eq!(status, 200, "{outcome}");
    assert_eq!(outcome["outcome"], "success", "{outcome}");
}

/// A REVOKED MACHINE'S STREAM IS CLOSED (#299). It ran on, and the person's own commands still
/// went down it: the machine's mode outlives the revoke.
#[tokio::test]
async fn a_revoked_machines_command_stream_is_closed() {
    let database_url = database_or_skip!();
    let d = desk(&database_url).await;
    let (machine, token) = d.enrol(None).await;
    let (status, mut frames) = d.open(&token).await;
    assert_eq!(status, 200);
    assert_eq!(frames.recv().await.unwrap()["kind"], "welcome");

    let path = format!("/local-exec/daemon/{machine}");
    let delete = reqwest::Method::DELETE;
    assert_eq!(d.send(&d.token, delete, &path, Value::Null).await.0, 204);
    assert_eq!(
        rest_of(frames).await,
        Vec::<Value>::new(),
        "ends, sent nothing"
    );
    let (status, refused) = d.clone().run(machine.clone()).await;
    assert_eq!(status, 403, "no stream holds the machine: {refused}");
    assert!(refused.to_string().contains("not connected"), "{refused}");
    assert_eq!(d.open(&token).await.0, 401, "a revoked token opens nothing");
}
