//! What a Bot's screen is doing, said instead of a black box (hexuria/opengrok-server#386).
//!
//! `GET /coworkers/{id}/computer` carries `screenStatus` and `screenStatusText` for the Computer
//! pane and the screen window to show over the viewer until its first picture, and the account
//! events stream sends `computer.changed` when it moves, so a second viewer (the separate window,
//! or the pane when the Bot reset its own computer) hears it without polling.
//!
//! And `screen` tells the truth: a Bot that would have its own screen on a computer too old for
//! own screens is `own-pending`, with a sentence saying an update brings it, where it said `own`.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use opengrok_box::screen_status::ScreenProgress;
use opengrok_box::{BoxResult, CommandOutput, Computer, Screen, StartedCommand};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::AccountId;
use opengrok_harness::{DeltaStream, ModelDoor, ModelError, ModelRequest};
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::host_state::HostState;
use opengrok_store::PgStore;
use serde_json::{Value, json};
use tokio::sync::Notify;

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

/// A computer whose state, own-screen progress and removal the test sets: a reset can be held
/// partway, as a slow `docker rm` would hold it.
struct ScreenBox {
    state: Mutex<String>,
    progress: Mutex<ScreenProgress>,
    hold_destroy: AtomicBool,
    destroying: Notify,
    release: Notify,
}

impl Default for ScreenBox {
    fn default() -> Self {
        Self {
            state: Mutex::new("running".into()),
            progress: Mutex::new(ScreenProgress::Given),
            hold_destroy: AtomicBool::new(false),
            destroying: Notify::new(),
            release: Notify::new(),
        }
    }
}

fn started() -> StartedCommand {
    StartedCommand {
        process_id: "p1".into(),
        running: false,
        stdout: String::new(),
        stderr: String::new(),
        exit_code: Some(0),
    }
}

#[async_trait]
impl Computer for ScreenBox {
    async fn create(&self, _ttl: Option<u64>) -> BoxResult<String> {
        Ok(format!("bx_screen_{}", uuid::Uuid::now_v7().simple()))
    }
    async fn run(&self, _b: &str, _c: &str, _t: u32) -> BoxResult<CommandOutput> {
        Ok(CommandOutput {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            timed_out: false,
        })
    }
    async fn start(&self, _b: &str, _c: &str) -> BoxResult<StartedCommand> {
        Ok(started())
    }
    async fn watch(&self, _b: &str, _p: &str) -> BoxResult<StartedCommand> {
        Ok(started())
    }
    async fn read_file(&self, _b: &str, _p: &str) -> BoxResult<String> {
        Ok(String::new())
    }
    async fn write_file(&self, _b: &str, _p: &str, _c: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn expose_port(&self, _b: &str, _p: u16, _t: &str) -> BoxResult<String> {
        Ok("http://stub.invalid".into())
    }
    async fn stop(&self, _b: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn resume(&self, _b: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn destroy(&self, _b: &str) -> BoxResult<()> {
        if self.hold_destroy.load(Ordering::SeqCst) {
            self.destroying.notify_one();
            self.release.notified().await;
        }
        Ok(())
    }
    async fn state(&self, _b: &str) -> BoxResult<String> {
        Ok(self.state.lock().unwrap().clone())
    }
    async fn offers_a_screen(&self, _b: &str) -> bool {
        true
    }
    async fn screen_url(&self, _b: &str, _s: &Screen) -> BoxResult<Option<String>> {
        Ok(Some("http://screen.invalid/vnc.html?autoconnect=1".into()))
    }
    fn screen_progress(&self, _b: &str, screen: &Screen) -> ScreenProgress {
        match screen.bot() {
            Some(_) => *self.progress.lock().unwrap(),
            None => ScreenProgress::Given,
        }
    }
}

/// No model is asked anything here.
struct Silent;

#[async_trait]
impl ModelDoor for Silent {
    async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
        Err(ModelError::Unreachable("not used".into()))
    }
}

struct Harness {
    base: String,
    token: String,
    computer: Arc<ScreenBox>,
    client: reqwest::Client,
}

async fn harness(database_url: &str) -> Harness {
    let email = format!("screen-status-{}@og.local", uuid::Uuid::now_v7().simple());
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let account = AccountId::new();
    let at_ms = chrono::Utc::now().timestamp_millis();
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.clone(),
            password_hash: "x".into(),
            first_name: "Host".into(),
            last_name: String::new(),
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
        password_hash: Some("x".into()),
        first_name: "Host".into(),
        last_name: String::new(),
        org_id: None,
        verified: true,
        enabled: true,
        avatar_url: None,
    };
    store
        .append_account(&account, 0, &events, &view)
        .await
        .expect("account");
    let computer = Arc::new(ScreenBox::default());
    let auth = AuthState::new(
        store,
        Arc::new(TokenMinter::new(b"screen-status-secret")),
        email.clone(),
    );
    let token = auth
        .minter
        .mint_access(
            account.as_str(),
            "sess-test",
            &email,
            "ultra",
            chrono::Utc::now().timestamp(),
            3600,
        )
        .expect("token");
    let agui = AgUiState {
        auth,
        door: Arc::new(Silent),
        model: "oag/cheap".into(),
        auto_review_model: "oag/cheap".into(),
        computer: Some(computer.clone()),
        vault: None,
        connectors: Connectors {
            providers: Arc::new(BTreeMap::new()),
            redirect_uri: "http://127.0.0.1/callback".into(),
        },
        plugins: Arc::new(BTreeMap::new()),
        host_settings: None,
    };
    let gateway = HostState::new(agui.clone(), None);
    let app = opengrok_server::router(agui, gateway);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    Harness {
        base,
        token,
        computer,
        client: reqwest::Client::new(),
    }
}

impl Harness {
    async fn hire(&self, name: &str) -> String {
        let hired: Value = self
            .client
            .post(format!("{}/coworkers", self.base))
            .bearer_auth(&self.token)
            .json(&json!({ "name": name }))
            .send()
            .await
            .expect("hire")
            .json()
            .await
            .expect("hire json");
        hired["id"].as_str().expect("coworker id").to_string()
    }

    async fn computer(&self, bot: &str) -> Value {
        let res = self
            .client
            .get(format!("{}/coworkers/{bot}/computer", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .expect("computer");
        assert_eq!(res.status().as_u16(), 200);
        res.json().await.expect("computer json")
    }

    async fn listen(&self) -> reqwest::Response {
        let res = self
            .client
            .get(format!("{}/ag-ui/events", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .expect("events");
        assert_eq!(res.status().as_u16(), 200);
        res
    }
}

/// The `computer.changed` notes for `bot` the stream carries within `ms`.
async fn computer_notes(stream: &mut reqwest::Response, bot: &str, ms: u64) -> usize {
    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
    while let Ok(Ok(Some(chunk))) = tokio::time::timeout_at(deadline, stream.chunk()).await {
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    seen.split("\n\n")
        .filter(|block| block.contains("event: computer.changed"))
        .filter(|block| block.contains(&format!(r#""coworkerId":"{bot}""#)))
        .count()
}

/// THE LABEL THAT LIED: a Bot on a shared computer gets a screen of its own, but on an image from
/// before own screens it works on the shared one, and the pane still said `own`. Now it says
/// `own-pending` and why; a full computer says `shared` and why; one that gave it says `own`.
#[tokio::test]
async fn a_bot_on_a_computer_too_old_for_own_screens_is_not_told_it_has_one() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let bot = h.hire("Ada").await;

    *h.computer.progress.lock().unwrap() = ScreenProgress::OldImage;
    let old = h.computer(&bot).await;
    assert_eq!(old["screen"], "own-pending", "{old}");
    let note = old["screenNote"]
        .as_str()
        .expect("why it is on the shared screen");
    assert!(
        note.contains("Ada") && note.contains("computer update"),
        "{note}"
    );
    assert_eq!(
        old["screenStatus"], "ready",
        "the shared screen it is on works: {old}"
    );

    *h.computer.progress.lock().unwrap() = ScreenProgress::Full;
    let full = h.computer(&bot).await;
    assert_eq!(full["screen"], "shared", "{full}");
    assert!(
        full["screenNote"]
            .as_str()
            .expect("why")
            .contains("room for")
    );

    *h.computer.progress.lock().unwrap() = ScreenProgress::Given;
    let own = h.computer(&bot).await;
    assert_eq!(own["screen"], "own", "{own}");
    assert!(own.get("screenNote").is_none(), "{own}");
    assert_eq!(own["screenStatus"], "ready");
    assert_eq!(own["screenStatusText"], "");
}

/// A computer coming up, and a Bot's own screen coming up on it, each say so by name, and the
/// screen page is not handed out until the Bot's own screen is there to show.
#[tokio::test]
async fn the_status_says_what_the_screen_is_doing() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let bot = h.hire("Ada").await;

    *h.computer.state.lock().unwrap() = "starting".into();
    let opening = h.computer(&bot).await;
    assert_eq!(opening["screenStatus"], "starting-computer", "{opening}");
    assert_eq!(opening["screenStatusText"], "Opening Ada's computer…");

    *h.computer.state.lock().unwrap() = "running".into();
    *h.computer.progress.lock().unwrap() = ScreenProgress::Starting;
    let screen = h.computer(&bot).await;
    assert_eq!(screen["screenStatus"], "starting-screen", "{screen}");
    assert_eq!(screen["screenStatusText"], "Starting Ada's screen…");
    assert_eq!(screen["vncUrl"], Value::Null, "{screen}");

    *h.computer.state.lock().unwrap() = "stopped".into();
    let stopped = h.computer(&bot).await;
    assert_eq!(stopped["screenStatus"], "stopped", "{stopped}");
}

/// A reset held partway: a second viewer reading the computer meanwhile is told it is resetting
/// (the reset runs inside the first request, and nothing said so before), and the app's events
/// stream hears `computer.changed` for the Bot when it begins and when it ends.
#[tokio::test]
async fn a_reset_is_seen_by_a_second_viewer_and_heard_on_the_events_stream() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let bot = h.hire("Ada").await;
    let mut stream = h.listen().await;

    h.computer.hold_destroy.store(true, Ordering::SeqCst);
    let resetting = {
        let (client, base, token, bot) = (
            h.client.clone(),
            h.base.clone(),
            h.token.clone(),
            bot.clone(),
        );
        tokio::spawn(async move {
            client
                .post(format!("{base}/coworkers/{bot}/computer/reset"))
                .bearer_auth(token)
                .send()
                .await
                .expect("reset")
                .status()
                .as_u16()
        })
    };
    tokio::time::timeout(Duration::from_secs(10), h.computer.destroying.notified())
        .await
        .expect("the reset reached the removal");

    let meanwhile = h.computer(&bot).await;
    assert_eq!(meanwhile["screenStatus"], "resetting", "{meanwhile}");
    assert_eq!(meanwhile["screenStatusText"], "Resetting Ada's computer…");

    h.computer.hold_destroy.store(false, Ordering::SeqCst);
    h.computer.release.notify_one();
    assert_eq!(resetting.await.expect("reset task"), 200);
    let after = h.computer(&bot).await;
    assert_ne!(
        after["screenStatus"], "resetting",
        "the mark goes with the work: {after}"
    );

    let heard = computer_notes(&mut stream, &bot, 1500).await;
    assert!(
        heard >= 2,
        "computer.changed when the reset began and when it ended: {heard}"
    );
}
