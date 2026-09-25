//! A hosted server never puts a coworker's computer on its own host (#190).
//!
//! Until 25 Sep 2026 `OG_HOSTED=1` was read only when a NEW computer's kind was chosen. The
//! provider lookup built a `DockerComputer` for `"local-docker"` unconditionally, so a hosted
//! server whose box.ascii.dev create failed (quota, 5xx, network) or whose box answered 403 on a
//! turn ran a bot container beside the token secret and the org vault — and the coworker moved
//! from its desktop to a headless debian with nothing saying why.
//!
//! `OG_HOSTED` CANNOT BE SET FROM A TEST: `set_var` is unsafe in Rust 2024 and `unsafe_code` is
//! forbidden. So these drive the seam the fix made the only door to a Local VM — the deployment's
//! own Docker provider, `AgUiState::computer`, which boot never installs when hosted. No provider,
//! or a box.ascii.dev one, is what a hosted server (and `OG_COMPUTER=none`) looks like from here.
//!
//! The org's box.ascii.dev is a stand-in on loopback (`AuthState::with_ascii_base_url`) that
//! answers a create with 429 or 503, or every box call with 403 — what a hosted server meets when
//! the vendor limits, fails or revokes. The vendor itself is never dialled.
//!
//! The first two need nothing; the rest need Postgres and skip loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use opengrok_box::{BoxError, BoxResult, CommandOutput, Computer, StartedCommand};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::AccountId;
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::agui::provision::{
    FELL_BACK, lookup_provider, provider_for, take_over_with_local_docker,
};
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::host_state::HostState;
use opengrok_store::{PgStore, Vault};
use serde_json::{Value, json};

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

/// A Local VM provider that remembers what it made and refuses the boxes it is told to, the way
/// box.ascii.dev answers a revoked key.
#[derive(Default)]
struct StubDocker {
    created: Mutex<Vec<String>>,
    refuses: Mutex<Vec<String>>,
}

#[async_trait]
impl Computer for StubDocker {
    async fn create(&self, _ttl_seconds: Option<u64>) -> BoxResult<String> {
        let id = format!("bx_local_{}", uuid::Uuid::now_v7().simple());
        self.created.lock().unwrap().push(id.clone());
        Ok(id)
    }
    async fn run(&self, _box_id: &str, _command: &str, _timeout: u32) -> BoxResult<CommandOutput> {
        Ok(CommandOutput {
            exit_code: 0,
            stdout: "ran-on-a-box".to_string(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            timed_out: false,
        })
    }
    async fn start(&self, _box_id: &str, _command: &str) -> BoxResult<StartedCommand> {
        Ok(StartedCommand {
            process_id: "p".to_string(),
            running: false,
            stdout: "ran-on-a-box".to_string(),
            stderr: String::new(),
            exit_code: Some(0),
        })
    }
    async fn watch(&self, box_id: &str, _process_id: &str) -> BoxResult<StartedCommand> {
        self.start(box_id, "").await
    }
    async fn read_file(&self, _box_id: &str, _path: &str) -> BoxResult<String> {
        Ok(String::new())
    }
    async fn write_file(&self, _box_id: &str, _path: &str, _content: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn expose_port(&self, _box_id: &str, _port: u16, _title: &str) -> BoxResult<String> {
        Ok("http://stub.invalid".to_string())
    }
    async fn stop(&self, _box_id: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn resume(&self, _box_id: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn destroy(&self, _box_id: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn state(&self, box_id: &str) -> BoxResult<String> {
        if self.refuses.lock().unwrap().iter().any(|id| id == box_id) {
            return Err(forbidden());
        }
        Ok("running".to_string())
    }
}

fn forbidden() -> BoxError {
    BoxError::Refused {
        status: 403,
        body: "forbidden: this key was revoked".to_string(),
    }
}

fn state_with(store: PgStore, computer: Option<Arc<dyn Computer>>) -> AgUiState {
    AgUiState {
        auth: AuthState::new(
            store,
            Arc::new(TokenMinter::new(b"hosted-computer-test-secret")),
            "host@og.local".to_string(),
        ),
        door: Arc::new(MockDoor::asking_for_a_tool()),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer,
        vault: None,
        connectors: Connectors {
            providers: Arc::new(BTreeMap::new()),
            redirect_uri: "http://127.0.0.1/callback".to_string(),
        },
        plugins: Arc::new(BTreeMap::new()),
        host_settings: None,
    }
}

/// A pool that never answers: the Local VM lookup must not need the store to say no.
fn a_store_that_cannot_answer() -> PgStore {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_millis(400))
        .connect_lazy("postgres://nobody:nobody@127.0.0.1:1/nothing")
        .expect("a lazily-connected pool");
    PgStore::new(pool)
}

#[tokio::test]
async fn local_docker_is_not_served_when_the_deployment_brought_none() {
    let state = state_with(a_store_that_cannot_answer(), None);
    let lookup = lookup_provider(&state, None, "local-docker").await;
    assert!(
        lookup.computer.is_none(),
        "a server with no Docker provider (hosted, or OG_COMPUTER=none) must not build one"
    );
    let (code, message) = lookup.error.expect("the refusal says why");
    assert_eq!(code, "not_supported");
    assert!(
        message.contains("Local VM"),
        "the pane and the model need a sentence, not a bare code: {message}"
    );
}

#[tokio::test]
async fn an_ascii_deployment_never_falls_back_to_docker() {
    let ascii = opengrok_box::AsciiBoxes::new("k").with_base_url("http://127.0.0.1:1");
    let state = state_with(a_store_that_cannot_answer(), Some(Arc::new(ascii)));
    assert!(
        provider_for(&state, None, "local-docker").await.is_none(),
        "only the deployment's own Docker provider may serve a Local VM"
    );
}

async fn seed_account(store: &PgStore, email: &str) -> AccountId {
    seed_member(store, email, None).await
}

async fn seed_member(store: &PgStore, email: &str, org: Option<&str>) -> AccountId {
    let id = AccountId::new();
    let at_ms = chrono::Utc::now().timestamp_millis();
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.to_string(),
            password_hash: "x".to_string(),
            first_name: "Hosted".to_string(),
            last_name: String::new(),
            org_id: org.unwrap_or_default().to_string(),
            plan: Plan::Ultra,
            verified: true,
            enabled: true,
            at_ms,
        })
        .expect("register");
    let view = AccountView {
        id: id.clone(),
        email: email.to_string(),
        plan: Plan::Ultra,
        trial: false,
        updated_at_ms: at_ms,
        password_hash: Some("x".to_string()),
        first_name: "Hosted".to_string(),
        last_name: String::new(),
        org_id: org.map(str::to_string),
        verified: true,
        enabled: true,
        avatar_url: None,
    };
    store
        .append_account(&id, 0, &events, &view)
        .await
        .expect("append account");
    id
}

async fn a_real_store(database_url: &str) -> PgStore {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    PgStore::new(pool)
}

struct Harness {
    base: String,
    state: AgUiState,
    account: AccountId,
    token: String,
    stub: Arc<StubDocker>,
    client: reqwest::Client,
}

async fn harness(database_url: &str) -> Harness {
    let store = a_real_store(database_url).await;
    let email = format!("hosted-{}@og.local", uuid::Uuid::now_v7().simple());
    let account = seed_account(&store, &email).await;
    let stub = Arc::new(StubDocker::default());
    let state = state_with(store, Some(stub.clone()));
    let token = state
        .auth
        .minter
        .mint_access(
            account.as_str(),
            "sess-hosted",
            &email,
            "ultra",
            chrono::Utc::now().timestamp(),
            3600,
        )
        .expect("mint access");
    let gateway = HostState::new(state.clone(), None);
    let app = opengrok_server::router(state.clone(), gateway);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    Harness {
        base: format!("http://127.0.0.1:{}", addr.port()),
        state,
        account,
        token,
        stub,
        client: reqwest::Client::new(),
    }
}

impl Harness {
    async fn hire(&self) -> String {
        let response = self
            .client
            .post(format!("{}/coworkers", self.base))
            .bearer_auth(&self.token)
            .json(&json!({ "name": "Hosted" }))
            .send()
            .await
            .expect("hire");
        assert_eq!(response.status().as_u16(), 201, "hire");
        let body: Value = response.json().await.expect("hire body");
        body["id"].as_str().expect("id").to_string()
    }

    async fn screen(&self, coworker: &str) -> Value {
        self.client
            .get(format!("{}/coworkers/{coworker}/computer", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .expect("screen")
            .json()
            .await
            .expect("screen body")
    }

    async fn scope_row(&self) -> Option<(String, String)> {
        self.state
            .auth
            .store
            .scoped_computer("account", self.account.as_str())
            .await
            .expect("scope row")
    }

    async fn record(&self, box_id: &str, kind: &str) {
        self.state
            .auth
            .store
            .set_scoped_computer("account", self.account.as_str(), box_id, kind, None, 1)
            .await
            .expect("record the scope's box");
    }
}

#[tokio::test]
async fn a_refusal_with_no_local_vm_takes_nothing_over() {
    let database_url = database_or_skip!();
    let store = a_real_store(&database_url).await;
    let email = format!("hosted-none-{}@og.local", uuid::Uuid::now_v7().simple());
    let account = seed_account(&store, &email).await;
    let state = state_with(store, None);
    state
        .auth
        .store
        .set_scoped_computer(
            "account",
            account.as_str(),
            "bx_ascii_old",
            "ascii",
            None,
            1,
        )
        .await
        .expect("record");

    let taken = take_over_with_local_docker(
        &state,
        &account,
        "account",
        account.as_str(),
        None,
        &forbidden(),
    )
    .await;

    assert!(
        taken.is_none(),
        "a server with no Local VM has nothing to fall back on"
    );
    assert_eq!(
        state
            .auth
            .store
            .scoped_computer("account", account.as_str())
            .await
            .expect("row"),
        Some(("bx_ascii_old".to_string(), "ascii".to_string())),
        "the refused box stays the scope's computer; no Local VM is recorded"
    );
}

#[tokio::test]
async fn a_self_hosted_takeover_keeps_the_fallback_and_says_so() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let coworker = h.hire().await;
    h.record("bx_ascii_old", "ascii").await;

    let (_, new_id) = take_over_with_local_docker(
        &h.state,
        &h.account,
        "account",
        h.account.as_str(),
        None,
        &forbidden(),
    )
    .await
    .expect("a self-hosted server with Docker keeps the fallback");

    assert_eq!(
        h.scope_row().await,
        Some((new_id.clone(), "local-docker".to_string()))
    );
    let screen = h.screen(&coworker).await;
    assert_eq!(screen["state"], json!("running"), "{screen}");
    assert_eq!(screen["boxId"], json!(new_id), "{screen}");
    assert_eq!(
        screen["computerError"]["code"],
        json!("invalid_key"),
        "the pane must be able to say the box changed, and why: {screen}"
    );
    let message = screen["computerError"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(message.starts_with(FELL_BACK), "{screen}");
    assert!(
        screen["computerError"]["updatedAtMs"].is_i64(),
        "every computerError carries its stamp: {screen}"
    );
}

#[tokio::test]
async fn a_healthy_local_vm_does_not_wear_another_scopes_failure() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let coworker = h.hire().await;
    // The account's error row is shared by its scopes: in per-bot mode another bot's failed hire
    // lands here too. Only a takeover's own stamp belongs beside a running Local VM.
    h.state
        .auth
        .store
        .set_account_computer_error(
            h.account.as_str(),
            "quota_exceeded",
            "box.ascii.dev refused (429): box creation rate limit reached",
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .expect("stamp another scope's failure");

    let screen = h.screen(&coworker).await;
    assert_eq!(screen["state"], json!("running"), "{screen}");
    assert!(
        screen.get("computerError").is_none(),
        "a failure that is not this box's must not be shown as its own: {screen}"
    );
}

#[tokio::test]
async fn a_turn_whose_box_refuses_is_told_so_and_gets_no_new_box() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let coworker = h.hire().await;
    // A Local VM that refuses is never "taken over" by another Local VM. The refusing ascii box
    // on a hosted server has its own test below, against a stand-in box.ascii.dev.
    h.record("bx_refused", "local-docker").await;
    h.stub
        .refuses
        .lock()
        .unwrap()
        .push("bx_refused".to_string());
    let made_before = h.stub.created.lock().unwrap().len();

    let sse = h
        .client
        .post(format!("{}/ag-ui", h.base))
        .bearer_auth(&h.token)
        .json(&json!({
            "threadId": format!("thr-{}", uuid::Uuid::now_v7()),
            "runId": uuid::Uuid::now_v7().to_string(),
            "messages": [{ "id": "m1", "role": "user", "content": "check the box" }],
            "forwardedProps": { "coworkerId": coworker },
        }))
        .send()
        .await
        .expect("turn")
        .text()
        .await
        .expect("sse");

    assert_eq!(
        h.stub.created.lock().unwrap().len(),
        made_before,
        "a refused box must not be swapped for a new container on a turn"
    );
    assert_eq!(
        h.scope_row().await,
        Some(("bx_refused".to_string(), "local-docker".to_string()))
    );
    assert!(
        sse.contains(opengrok_tools::COMPUTER_DOWN) && sse.contains("403"),
        "the tool must answer with the refusal, not vanish or run elsewhere: {sse}"
    );
    assert!(!sse.contains("ran-on-a-box"), "{sse}");
}

const KEK: &str = "rIeYsJHlXEYIoRjZQfL73u7UuVMYxIrdlDT5tndh/kY=";
const ORG_KEY: &str = "box_org_key_for_the_stand_in";

/// box.ascii.dev as a hosted server meets it on a bad day. `POST /boxes` answers `create` (200
/// is a box); every other call answers `other`. Each request is kept as "METHOD /path bearer".
#[derive(Clone)]
struct StandInAscii {
    create: Arc<AtomicU16>,
    other: Arc<AtomicU16>,
    seen: Arc<Mutex<Vec<String>>>,
}

impl StandInAscii {
    async fn start(create: u16, other: u16) -> (String, Self) {
        let stand_in = Self {
            create: Arc::new(AtomicU16::new(create)),
            other: Arc::new(AtomicU16::new(other)),
            seen: Arc::new(Mutex::new(Vec::new())),
        };
        let serving = stand_in.clone();
        let app = axum::Router::new().fallback(move |request: axum::extract::Request| {
            let serving = serving.clone();
            async move {
                let bearer = request
                    .headers()
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                let path = request.uri().path().to_string();
                let method = request.method().to_string();
                serving
                    .seen
                    .lock()
                    .unwrap()
                    .push(format!("{method} {path} {bearer}"));
                let creating = method == "POST" && path.ends_with("/boxes");
                let status = if creating {
                    serving.create.load(Ordering::SeqCst)
                } else {
                    serving.other.load(Ordering::SeqCst)
                };
                let status = axum::http::StatusCode::from_u16(status).unwrap();
                let body = match status.as_u16() {
                    200 => json!({ "id": "box_hosted_1" }),
                    429 => json!({ "error": "box creation rate limit reached" }),
                    403 => json!({ "error": "forbidden: this key was revoked" }),
                    _ => json!({ "error": "upstream unavailable" }),
                };
                (status, axum::Json(body))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}/api/box/v1"), stand_in)
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }

    fn creates(&self) -> usize {
        self.seen()
            .iter()
            .filter(|line| line.starts_with("POST ") && line.contains("/boxes "))
            .count()
    }
}

/// A hosted server: its boot provider is box.ascii.dev (`OG_BOX_API_KEY`), so there is no Docker
/// to fall back on, and an org member whose org has sealed its own key. Returns the server's
/// base URL, its state, the member and the member's token.
async fn hosted(database_url: &str, ascii: &str) -> (String, AgUiState, AccountId, String) {
    let store = a_real_store(database_url).await;
    let vault = Arc::new(Vault::from_base64_key(KEK).expect("vault"));
    let org = format!("org_hosted_{}", uuid::Uuid::now_v7().simple());
    store
        .set_org_computer_secret(&vault, &org, "ascii", ORG_KEY, 1)
        .await
        .expect("seal the org's key");
    let email = format!("hosted-org-{}@og.local", uuid::Uuid::now_v7().simple());
    let account = seed_member(&store, &email, Some(&org)).await;
    let boot = opengrok_box::AsciiBoxes::new("box_boot_key_never_for_an_org").with_base_url(ascii);
    let mut state = state_with(store, Some(Arc::new(boot)));
    state.auth = state.auth.with_ascii_base_url(ascii);
    state.vault = Some(vault);
    let token = state
        .auth
        .minter
        .mint_access(
            account.as_str(),
            "sess-hosted-org",
            &email,
            "ultra",
            chrono::Utc::now().timestamp(),
            3600,
        )
        .expect("mint access");
    let gateway = HostState::new(state.clone(), None);
    let app = opengrok_server::router(state.clone(), gateway);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    (
        format!("http://127.0.0.1:{}", addr.port()),
        state,
        account,
        token,
    )
}

async fn hire_at(base: &str, token: &str) -> String {
    let response = reqwest::Client::new()
        .post(format!("{base}/coworkers"))
        .bearer_auth(token)
        .json(&json!({ "name": "Hosted" }))
        .send()
        .await
        .expect("hire");
    assert_eq!(
        response.status().as_u16(),
        201,
        "a boxless hire still stands"
    );
    let body: Value = response.json().await.expect("hire body");
    body["id"].as_str().expect("id").to_string()
}

#[tokio::test]
async fn a_hosted_hire_whose_ascii_create_fails_records_why_and_makes_no_box() {
    let database_url = database_or_skip!();
    for (status, code) in [(429, "quota_exceeded"), (503, "provider_error")] {
        let (ascii, stand_in) = StandInAscii::start(status, 500).await;
        let (base, state, account, token) = hosted(&database_url, &ascii).await;

        let coworker = hire_at(&base, &token).await;

        assert_eq!(
            stand_in.creates(),
            1,
            "{status}: box.ascii.dev was asked once, and nothing else was: {:?}",
            stand_in.seen()
        );
        assert!(
            stand_in
                .seen()
                .iter()
                .all(|line| line.ends_with(&format!("Bearer {ORG_KEY}"))),
            "{status}: only the org's own key is ever sent: {:?}",
            stand_in.seen()
        );
        assert_eq!(
            state
                .auth
                .store
                .scoped_computer("account", account.as_str())
                .await
                .expect("row"),
            None,
            "{status}: no box of any kind is recorded for the scope"
        );
        let screen: Value = reqwest::Client::new()
            .get(format!("{base}/coworkers/{coworker}/computer"))
            .bearer_auth(&token)
            .send()
            .await
            .expect("screen")
            .json()
            .await
            .expect("screen body");
        assert_eq!(screen["state"], json!("absent"), "{status}: {screen}");
        assert_eq!(
            screen["computerError"]["code"],
            json!(code),
            "{status}: the upstream code reaches the pane: {screen}"
        );
        assert!(
            screen["computerError"]["message"]
                .as_str()
                .is_some_and(|message| message.contains(&status.to_string())),
            "{status}: the message carries what box.ascii.dev said: {screen}"
        );
    }
}

#[tokio::test]
async fn a_hosted_turn_whose_ascii_box_answers_403_is_told_so_and_gets_no_box() {
    let database_url = database_or_skip!();
    let (ascii, stand_in) = StandInAscii::start(200, 403).await;
    let (base, state, account, token) = hosted(&database_url, &ascii).await;
    let coworker = hire_at(&base, &token).await;
    assert_eq!(
        state
            .auth
            .store
            .scoped_computer("account", account.as_str())
            .await
            .expect("row"),
        Some(("box_hosted_1".to_string(), "ascii".to_string())),
        "the hire made its box on box.ascii.dev"
    );
    // From here the key is revoked: every box call answers 403, and so would a new create.
    stand_in.create.store(403, Ordering::SeqCst);

    let sse = reqwest::Client::new()
        .post(format!("{base}/ag-ui"))
        .bearer_auth(&token)
        .json(&json!({
            "threadId": format!("thr-{}", uuid::Uuid::now_v7()),
            "runId": uuid::Uuid::now_v7().to_string(),
            "messages": [{ "id": "m1", "role": "user", "content": "check the box" }],
            "forwardedProps": { "coworkerId": coworker },
        }))
        .send()
        .await
        .expect("turn")
        .text()
        .await
        .expect("sse");

    assert!(
        sse.contains(opengrok_tools::COMPUTER_DOWN) && sse.contains("403"),
        "the tool answers with the refusal, a result the model can relay: {sse}"
    );
    assert_eq!(
        stand_in.creates(),
        1,
        "no second box is asked for: {:?}",
        stand_in.seen()
    );
    assert_eq!(
        state
            .auth
            .store
            .scoped_computer("account", account.as_str())
            .await
            .expect("row"),
        Some(("box_hosted_1".to_string(), "ascii".to_string())),
        "the refused box stays the scope's computer; no Local VM takes its place"
    );
}
