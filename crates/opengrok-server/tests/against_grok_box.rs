//! grok-box as an org computer: kind selection, sharing modes, and a real vncUrl.
//!
//! The Computer impl lives in `opengrok-box`. These tests prove the server selects it when
//! configured, that per-org / per-account / per-bot still share or isolate by SCOPE (not by
//! guest internals), and that `getForeverBoxStatus` carries a screen URL that does not contain
//! BOX_TOKEN. A recording stand-in is the provider — a real grok-box image is optional and
//! covered in `opengrok-box`'s own tests (skipped when the image is absent).
//!
//! Needs Postgres; skips loudly without it.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use opengrok_box::{BoxResult, CommandOutput, Computer, StartedCommand};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::coworker::{Coworker, CoworkerCommand, CoworkerView};
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::agui::provision::{ensure_computer_for, kind_for_new, provider_for};
use opengrok_server::auth::password::hash_password;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::gateway::GatewayState;
use opengrok_server::gateway::conversation::box_status;
use opengrok_store::{PgStore, Vault};
use serde_json::json;

macro_rules! database_or_skip {
    () => {
        match std::env::var("OG_DATABASE_URL") {
            Ok(url) => url,
            Err(_) => {
                eprintln!("skipping: OG_DATABASE_URL is not set");
                return;
            }
        }
    };
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

async fn store_from(database_url: &str) -> PgStore {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(database_url)
        .await
        .expect("connect");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    PgStore::new(pool)
}

fn vault() -> Vault {
    Vault::from_base64_key("rIeYsJHlXEYIoRjZQfL73u7UuVMYxIrdlDT5tndh/kY=").expect("vault")
}

/// A grok-box-shaped computer that never talks Docker. create() mints an id; screen_url is a
/// noVNC URL with an independent VNC password and no BOX_TOKEN.
struct RecordingGrok {
    created: Mutex<Vec<String>>,
    next: Mutex<u32>,
}

impl RecordingGrok {
    fn new() -> Self {
        Self {
            created: Mutex::new(Vec::new()),
            next: Mutex::new(1),
        }
    }
}

#[async_trait]
impl Computer for RecordingGrok {
    fn kind(&self) -> &'static str {
        "grok-box"
    }
    async fn create(&self, _ttl_seconds: Option<u64>) -> BoxResult<String> {
        let mut next = self.next.lock().expect("next");
        let id = format!("og-gb-rec{next:02}");
        *next += 1;
        self.created.lock().expect("created").push(id.clone());
        Ok(id)
    }
    async fn run(
        &self,
        _box_id: &str,
        _command: &str,
        _timeout_seconds: u32,
    ) -> BoxResult<CommandOutput> {
        Ok(CommandOutput {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            timed_out: false,
        })
    }
    async fn start(&self, _box_id: &str, _command: &str) -> BoxResult<StartedCommand> {
        Ok(StartedCommand {
            process_id: "p1".to_string(),
            running: false,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
        })
    }
    async fn watch(&self, _box_id: &str, process_id: &str) -> BoxResult<StartedCommand> {
        Ok(StartedCommand {
            process_id: process_id.to_string(),
            running: false,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
        })
    }
    async fn read_file(&self, _box_id: &str, _path: &str) -> BoxResult<String> {
        Ok(String::new())
    }
    async fn write_file(&self, _box_id: &str, _path: &str, _content: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn expose_port(&self, _box_id: &str, _port: u16, _title: &str) -> BoxResult<String> {
        Ok("http://127.0.0.1:1".to_string())
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
    async fn state(&self, _box_id: &str) -> BoxResult<String> {
        Ok("running".to_string())
    }
    async fn screen_url(&self, _box_id: &str) -> BoxResult<Option<String>> {
        Ok(Some(
            "http://127.0.0.1:6080/vnc.html?autoconnect=1&resize=scale&password=Ab3DefgH"
                .to_string(),
        ))
    }
}

fn agui(store: PgStore, computer: Arc<dyn Computer>, vault: Option<Arc<Vault>>) -> AgUiState {
    AgUiState {
        auth: AuthState::new(
            store,
            Arc::new(TokenMinter::new(b"grok-box-test-secret-grok-box-test")),
            "host@og.local".to_string(),
        ),
        door: Arc::new(MockDoor::echoing()),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer: Some(computer),
        vault,
        connectors: Connectors {
            providers: Arc::new(BTreeMap::new()),
            redirect_uri: "http://127.0.0.1/callback".to_string(),
        },
        plugins: Arc::new(BTreeMap::new()),
    }
}

async fn seed_account(store: &PgStore, email: &str, org: &str) -> AccountId {
    let id = AccountId::new();
    let hash = hash_password("password1").expect("hash");
    let at_ms = now_ms();
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.to_string(),
            password_hash: hash.clone(),
            first_name: "Test".to_string(),
            last_name: "User".to_string(),
            org_id: org.to_string(),
            plan: Plan::Ultra,
            verified: true,
            enabled: true,
            at_ms,
        })
        .expect("register");
    let account = Account::replay(&events);
    store
        .append_account(
            &id,
            0,
            &events,
            &AccountView {
                id: id.clone(),
                email: email.to_string(),
                plan: Plan::Ultra,
                trial: false,
                updated_at_ms: at_ms,
                password_hash: Some(hash),
                first_name: "Test".to_string(),
                last_name: "User".to_string(),
                org_id: Some(org.to_string()),
                verified: account.verified,
                enabled: account.enabled,
                avatar_url: None,
            },
        )
        .await
        .expect("append account");
    id
}

async fn hire(state: &AgUiState, account: &AccountId, name: &str) -> (CoworkerId, Option<String>) {
    let id = CoworkerId::new();
    let mut coworker = Coworker::default();
    let mut events = Coworker::default()
        .decide(CoworkerCommand::Hire {
            name: name.to_string(),
            model: "oag/cheap".to_string(),
            at_ms: 1,
        })
        .expect("hire");
    for event in &events {
        coworker.apply(event);
    }
    let provisioned = ensure_computer_for(state, account, &id, &mut coworker, 2).await;
    assert!(
        provisioned.error.is_none(),
        "provision failed: {:?}",
        provisioned.error
    );
    events.extend(provisioned.events);
    let box_id = coworker.computer().map(|id| id.as_str().to_string());
    let view = CoworkerView {
        id: id.clone(),
        name: coworker.name.clone(),
        model: coworker.model.clone(),
        box_id: coworker.computer().cloned(),
        retired: false,
        members: Vec::new(),
        updated_at_ms: 2,
        role: None,
        visibility: Default::default(),
    };
    state
        .auth
        .store
        .append_coworker(&id, account, 0, &events, &view)
        .await
        .expect("append");
    (id, box_id)
}

#[tokio::test]
async fn kind_for_new_prefers_grok_box_when_the_org_enabled_it() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let vault = Arc::new(vault());
    let org = format!("org_{}", uuid::Uuid::now_v7().simple());
    store
        .set_org_computer_secret(&vault, &org, "ascii", "box_live_secret_key", 1)
        .await
        .expect("ascii");
    store
        .set_org_computer_secret(&vault, &org, "grok-box", "grok-box:local", 1)
        .await
        .expect("grok-box");

    let state = agui(
        store,
        Arc::new(opengrok_box::DockerComputer::new()),
        Some(vault),
    );
    assert_eq!(kind_for_new(&state, Some(&org)).await, "grok-box");
    let provider = provider_for(&state, Some(&org), "grok-box")
        .await
        .expect("provider");
    assert_eq!(provider.kind(), "grok-box");
    // ascii stays selectable for an existing mapping even when grok-box is preferred for new.
    let ascii = provider_for(&state, Some(&org), "ascii")
        .await
        .expect("ascii still builds");
    assert_eq!(ascii.kind(), "ascii");
}

#[tokio::test]
async fn kind_for_new_uses_boot_grok_box_without_an_org_secret() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let state = agui(store, Arc::new(RecordingGrok::new()), None);
    assert_eq!(kind_for_new(&state, None).await, "grok-box");
}

#[tokio::test]
async fn kind_for_new_falls_back_to_ascii_when_grok_box_is_not_enabled() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let vault = Arc::new(vault());
    let org = format!("org_{}", uuid::Uuid::now_v7().simple());
    store
        .set_org_computer_secret(&vault, &org, "ascii", "box_live_secret_key", 1)
        .await
        .expect("ascii");
    let state = agui(
        store,
        Arc::new(opengrok_box::DockerComputer::new()),
        Some(vault),
    );
    assert_eq!(kind_for_new(&state, Some(&org)).await, "ascii");
}

#[tokio::test]
async fn per_account_shares_one_grok_box_and_per_bot_does_not() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let recorder = Arc::new(RecordingGrok::new());
    let state = agui(store, recorder.clone(), None);
    let account = AccountId::new();

    let (_, box1) = hire(&state, &account, "One").await;
    let (_, box2) = hire(&state, &account, "Two").await;
    assert_eq!(box1, box2, "per-account (default) shares one grok-box");
    assert_eq!(recorder.created.lock().expect("created").len(), 1);

    let other = AccountId::new();
    state
        .auth
        .store
        .set_sharing_mode("account", other.as_str(), "per-bot", 1)
        .await
        .expect("mode");
    let (_, bot1) = hire(&state, &other, "BotOne").await;
    let (_, bot2) = hire(&state, &other, "BotTwo").await;
    assert_ne!(bot1, bot2, "per-bot: each bot gets its own grok-box");
    assert_eq!(recorder.created.lock().expect("created").len(), 3);
}

#[tokio::test]
async fn per_org_shares_one_grok_box_across_members() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let org = format!("org_{tag}");
    let a = seed_account(&store, &format!("a-{tag}@og.local"), &org).await;
    let b = seed_account(&store, &format!("b-{tag}@og.local"), &org).await;
    store
        .set_sharing_mode("org", &org, "per-org", 1)
        .await
        .expect("mode");
    let recorder = Arc::new(RecordingGrok::new());
    let state = agui(store, recorder.clone(), None);

    let (_, box_a) = hire(&state, &a, "A").await;
    let (_, box_b) = hire(&state, &b, "B").await;
    assert_eq!(
        box_a, box_b,
        "per-org: two members share one grok-box (one filesystem)"
    );
    assert_eq!(recorder.created.lock().expect("created").len(), 1);
}

#[tokio::test]
async fn a_member_override_isolates_from_the_org_box() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let org = format!("org_{tag}");
    let a = seed_account(&store, &format!("a-{tag}@og.local"), &org).await;
    let b = seed_account(&store, &format!("b-{tag}@og.local"), &org).await;
    store
        .set_sharing_mode("org", &org, "per-org", 1)
        .await
        .expect("org mode");
    store
        .set_sharing_mode("account", b.as_str(), "per-bot", 1)
        .await
        .expect("member override");
    let recorder = Arc::new(RecordingGrok::new());
    let state = agui(store, recorder.clone(), None);

    let (_, org_box_1) = hire(&state, &a, "A1").await;
    let (_, org_box_2) = hire(&state, &a, "A2").await;
    assert_eq!(
        org_box_1, org_box_2,
        "org-default member still shares the org box"
    );

    let (_, bot_1) = hire(&state, &b, "B1").await;
    let (_, bot_2) = hire(&state, &b, "B2").await;
    assert_ne!(
        bot_1, bot_2,
        "per-bot override: each of B's bots is its own guest"
    );
    assert_ne!(
        bot_1, org_box_1,
        "the override must not reuse the org filesystem"
    );
    assert_eq!(recorder.created.lock().expect("created").len(), 3);
}

#[tokio::test]
async fn get_forever_box_status_carries_a_screen_url_without_the_box_token() {
    let database_url = database_or_skip!();
    let store = store_from(&database_url).await;
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let email = format!("screen-{tag}@og.local");
    let org = format!("org_{tag}");
    let account = seed_account(&store, &email, &org).await;
    let recorder = Arc::new(RecordingGrok::new());
    let state = agui(store, recorder, None);
    let (coworker_id, box_id) = hire(&state, &account, "Hexuria").await;
    let box_id = box_id.expect("box");
    assert!(box_id.starts_with("og-gb-"), "{box_id}");

    let gateway = GatewayState::new(state, Some("test-bearer".into()), email.clone(), None);
    let args = json!({ "agentId": coworker_id.as_str() });
    let (code, body) = box_status(&gateway, &args, &email).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["state"].as_str(), Some("running"), "{body}");
    let vnc = body["vncUrl"].as_str().expect("vncUrl");
    assert!(vnc.contains("/vnc.html"), "{body}");
    assert!(vnc.contains("password=Ab3DefgH"), "{body}");
    assert!(!vnc.to_lowercase().contains("box_token"), "{body}");
    assert!(!format!("{body}").contains("BOX_TOKEN"), "{body}");
}
