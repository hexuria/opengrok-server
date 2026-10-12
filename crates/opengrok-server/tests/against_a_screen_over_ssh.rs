//! One Bot's screen over SSH (#386, part 2): `POST /coworkers/{id}/computer/ssh` mints a code for
//! that Bot's screen and hands back the command that uses it, and the gateway's check
//! (`SshScreens`) finds that screen again for the code, and nothing else.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use opengrok_box::screen_status::ScreenProgress;
use opengrok_box::{BoxResult, CommandOutput, Computer, Screen, StartedCommand};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::AccountId;
use opengrok_harness::{DeltaStream, ModelDoor, ModelError, ModelRequest};
use opengrok_screen_ssh::Screens as _;
use opengrok_server::agui::AgUiState;
use opengrok_server::agui::screen_proxy::SshScreens;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::host_state::HostState;
use opengrok_store::PgStore;
use serde_json::{Value, json};
use tokio::sync::Notify;

/// The box's noVNC page as the Docker provider gives it for a Bot's own screen on display :2.
const PAGE: &str = "http://127.0.0.1:46080/vnc.html?autoconnect=true&resize=scale&reconnect=true&password=pw-1&path=websockify%3Ftoken%3Ds2";

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
        Ok(Some(PAGE.into()))
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
    agui: AgUiState,
    base: String,
    token: String,
    computer: Arc<ScreenBox>,
    client: reqwest::Client,
}

async fn harness(database_url: &str) -> Harness {
    let email = format!("screen-ssh-{}@og.local", uuid::Uuid::now_v7().simple());
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
        Arc::new(TokenMinter::new(b"screen-ssh-secret")),
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
    let app = opengrok_server::router(agui.clone(), gateway);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    Harness {
        agui,
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

impl Harness {
    async fn ask(&self, bot: &str) -> (u16, Value) {
        let res = self
            .client
            .post(format!("{}/coworkers/{bot}/computer/ssh", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .expect("ssh");
        let status = res.status().as_u16();
        let text = res.text().await.expect("body");
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }
}

/// THE CODE AND ITS COMMAND. Off until the gateway listens; then a code for one Bot's screen, the
/// command naming the gateway's port and the host the app reached the server on, the screen's VNC
/// password, and fifteen minutes. The code is the Bot's and its box's, and the gateway's check
/// finds that Bot's own screen for it, and refuses it once the Bot is on another box.
#[tokio::test]
async fn a_code_reaches_one_bots_screen_and_the_command_says_how() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let bot = h.hire("Ada").await;

    let (status, body) = h.ask(&bot).await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"], opengrok_screen_ssh::OFF, "{body}");

    opengrok_screen_ssh::set_listening(2299);
    let (status, offer) = h.ask(&bot).await;
    assert_eq!(status, 200, "{offer}");
    let code = offer["user"].as_str().expect("a code").to_string();
    assert_eq!(
        offer["command"],
        format!("ssh -N -p 2299 -L 5902:screen:5900 {code}@127.0.0.1")
    );
    assert_eq!(offer["vncPassword"], "pw-1");
    assert_eq!(offer["localPort"], 5902);
    let left = offer["expiresAtMs"].as_i64().unwrap() - chrono::Utc::now().timestamp_millis();
    assert!((14 * 60_000..=15 * 60_000).contains(&left), "{left}");

    let grant = opengrok_screen_ssh::codes()
        .redeem(&code)
        .expect("a live code");
    let computer = h.computer(&bot).await;
    assert_eq!(grant.coworker, bot);
    assert_eq!(Some(grant.box_id.as_str()), computer["boxId"].as_str());
    let screens = SshScreens(h.agui.clone());
    assert_eq!(
        screens.screen_for(&grant).await,
        Some((46080, "websockify?token=s2".to_string())),
        "the Bot's own screen, through the box's viewer"
    );
    let moved = opengrok_screen_ssh::Grant {
        box_id: "bx_elsewhere".into(),
        ..grant.clone()
    };
    assert_eq!(
        screens.screen_for(&moved).await,
        None,
        "a computer that moved is refused"
    );
    let stranger = opengrok_screen_ssh::Grant {
        account: AccountId::new().to_string(),
        ..grant
    };
    assert_eq!(
        screens.screen_for(&stranger).await,
        None,
        "another account's code is refused"
    );

    let (status, _) = h.ask("cw_not_this_accounts").await;
    assert_eq!(status, 404, "another account's Bot reads as no such Bot");
}
