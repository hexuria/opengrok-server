//! A recipe held by two people — its owner and a colleague it was shared with — over HTTP.
//!
//! `against_a_shared_recipe` proves the rows; this file proves what a person and a bot see through
//! the routes and through a turn's offers, because the bugs here were all in what one person's
//! action left behind for the other: a share taken back that the colleague's bot kept running.
//!
//! NOT ONE BYTE LEAVES THE MACHINE: the box is a stand-in. Needs Postgres, and skips loudly
//! without OG_DATABASE_URL, the same bargain the other integration tests make.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use opengrok_box::{BoxResult, CommandOutput, Computer, StartedCommand};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::host_state::HostState;
use opengrok_store::PgStore;
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

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// A box that plays every recipe.
#[derive(Default)]
struct StubBox;

#[async_trait]
impl Computer for StubBox {
    async fn create(&self, _ttl_seconds: Option<u64>) -> BoxResult<String> {
        Ok(format!("bx_stub_{}", uuid::Uuid::now_v7().simple()))
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
            process_id: "p".to_string(),
            running: false,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
        })
    }
    async fn watch(&self, _box_id: &str, _process_id: &str) -> BoxResult<StartedCommand> {
        self.start("", "").await
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
    async fn state(&self, _box_id: &str) -> BoxResult<String> {
        Ok("running".to_string())
    }
    async fn run_recipe(&self, _box_id: &str, _request: &Value) -> BoxResult<Value> {
        Ok(json!({ "ok": true, "ran": 1 }))
    }
}

async fn seed_account(store: &PgStore, email: &str, org: &str) -> AccountId {
    let id = AccountId::new();
    let at_ms = now_ms();
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.to_string(),
            password_hash: "x".to_string(),
            first_name: "Recipe".to_string(),
            last_name: String::new(),
            org_id: org.to_string(),
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
        first_name: "Recipe".to_string(),
        last_name: String::new(),
        org_id: Some(org.to_string()),
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

struct Harness {
    base: String,
    agui: AgUiState,
    store: PgStore,
    client: reqwest::Client,
    org: String,
}

/// One person: their account id, and a token that signs as them.
struct Person {
    id: String,
    token: String,
}

async fn harness(database_url: &str) -> Harness {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let auth = AuthState::new(
        store.clone(),
        Arc::new(TokenMinter::new(b"two-hands-test-secret-two-hands!!")),
        "host@og.local".to_string(),
    );
    let agui = AgUiState {
        auth,
        door: Arc::new(MockDoor::echoing()),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer: Some(Arc::new(StubBox)),
        vault: None,
        connectors: Connectors {
            providers: Arc::new(BTreeMap::new()),
            redirect_uri: "http://127.0.0.1/callback".to_string(),
        },
        plugins: Arc::new(BTreeMap::new()),
        host_settings: None,
    };
    let gateway = HostState::new(agui.clone(), Some("http://opengrok.lan:1447".to_string()));
    let app = opengrok_server::router(agui.clone(), gateway);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    Harness {
        base: format!("http://127.0.0.1:{}", addr.port()),
        agui,
        store,
        client: reqwest::Client::new(),
        org: format!("org_two_hands_{}", uuid::Uuid::now_v7().simple()),
    }
}

impl Harness {
    /// A person in this harness's org: sharing is only ever inside one.
    async fn person(&self) -> Person {
        let email = format!("two-hands-{}@og.local", uuid::Uuid::now_v7().simple());
        let account = seed_account(&self.store, &email, &self.org).await;
        let token = self
            .agui
            .auth
            .minter
            .mint_access(
                account.as_str(),
                "sess-test",
                &email,
                "ultra",
                chrono::Utc::now().timestamp(),
                3600,
            )
            .expect("mint access");
        Person {
            id: account.as_str().to_string(),
            token,
        }
    }

    async fn call(
        &self,
        who: &Person,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let url = format!("{}{path}", self.base);
        let request = match method {
            "GET" => self.client.get(url),
            "POST" => self.client.post(url),
            "DELETE" => self.client.delete(url),
            _ => panic!("no such method in this harness: {method}"),
        }
        .header("authorization", format!("Bearer {}", who.token));
        let request = match body {
            Some(body) => request.json(&body),
            None => request,
        };
        let response = request.send().await.expect("send");
        let status = response.status().as_u16();
        let text = response.text().await.expect("text");
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    /// A bot of this person's, with the stand-in computer behind it.
    async fn hire(&self, who: &Person) -> String {
        let (status, hired) = self
            .call(who, "POST", "/coworkers", Some(json!({ "name": "Ada" })))
            .await;
        assert_eq!(status, 201, "{hired}");
        assert!(
            hired["boxId"]
                .as_str()
                .is_some_and(|id| id.starts_with("bx_stub_")),
            "the stand-in computer was assigned: {hired}"
        );
        hired["id"].as_str().expect("coworker id").to_string()
    }

    /// A taught recipe, written straight to the store: filtering a tape is proved elsewhere.
    async fn taught(&self, owner: &Person, name: &str) -> String {
        let id = format!("rcp_{}", uuid::Uuid::now_v7());
        self.store
            .create_recipe(
                &id,
                &owner.id,
                Some(&self.org),
                name,
                "",
                (1280, 800),
                now_ms(),
            )
            .await
            .expect("create recipe");
        self.store
            .add_recipe_version(
                &id,
                "filtered",
                &json!({
                    "steps": [{"op": "click", "x": 10, "y": 10, "button": 1}],
                    "stop_on_error": true, "screenshot": "end"
                }),
                "filtered",
                &owner.id,
                now_ms(),
            )
            .await
            .expect("version");
        id
    }

    /// What a turn with this bot would offer `run_recipe`, by recipe id.
    async fn offered(&self, bot: &str) -> Vec<String> {
        opengrok_server::recipes::offers_for(&self.agui, &CoworkerId::from_stored(bot.to_string()))
            .await
            .into_iter()
            .map(|offer| offer.id)
            .collect()
    }

    /// What the tool asks at the moment a recipe would play, after the turn's offers were read.
    async fn still_granted(&self, id: &str, bot: &str) -> bool {
        let source = opengrok_server::recipes::StoreRecipes {
            store: self.store.clone(),
        };
        opengrok_tools::RecipeSource::still_granted(
            &source,
            id,
            &CoworkerId::from_stored(bot.to_string()),
        )
        .await
        .is_ok()
    }

    /// Share `id` with `with`, have them accept it, and have them grant it to `bot`.
    async fn share_accept_grant(&self, owner: &Person, with: &Person, id: &str, bot: &str) {
        let (status, body) = self
            .call(
                owner,
                "POST",
                &format!("/recipes/{id}/share"),
                Some(json!({ "scope": "account", "scopeId": with.id })),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        let (status, body) = self
            .call(with, "POST", &format!("/recipes/{id}/accept"), None)
            .await;
        assert_eq!(status, 200, "{body}");
        let (status, body) = self
            .call(
                with,
                "POST",
                &format!("/recipes/{id}/grants"),
                Some(json!({ "coworkerId": bot })),
            )
            .await;
        assert_eq!(status, 200, "{body}");
    }
}

fn granted_bots(detail: &Value) -> Vec<String> {
    detail["grants"]
        .as_array()
        .expect("grants")
        .iter()
        .filter_map(|grant| grant["coworkerId"].as_str().map(str::to_string))
        .collect()
}

// -----------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_share_taken_back_leaves_the_next_turns_offers_and_the_run_route() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let owner = h.person().await;
    let colleague = h.person().await;
    let id = h.taught(&owner, "Pay the rent").await;
    let owners_bot = h.hire(&owner).await;
    let bot = h.hire(&colleague).await;

    let (status, body) = h
        .call(
            &owner,
            "POST",
            &format!("/recipes/{id}/grants"),
            Some(json!({ "coworkerId": owners_bot })),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    h.share_accept_grant(&owner, &colleague, &id, &bot).await;
    assert!(h.offered(&bot).await.contains(&id), "granted, so offered");
    assert!(h.still_granted(&id, &bot).await);

    // A recipient sees the grants of their own bots, not the owner's bot ids.
    let (status, theirs) = h
        .call(&colleague, "GET", &format!("/recipes/{id}"), None)
        .await;
    assert_eq!(status, 200, "{theirs}");
    assert_eq!(granted_bots(&theirs), vec![bot.clone()]);

    // ---- the owner takes it back ----
    let (status, body) = h
        .call(
            &owner,
            "DELETE",
            &format!("/recipes/{id}/share/account/{}", colleague.id),
            None,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        !h.offered(&bot).await.contains(&id),
        "the next turn does not offer a recipe whose share was taken back"
    );
    assert!(
        !h.still_granted(&id, &bot).await,
        "and a turn that read its offers before the share was taken back cannot play it either"
    );
    let (status, why) = h
        .call(
            &colleague,
            "POST",
            &format!("/recipes/{id}/run"),
            Some(json!({ "coworkerId": bot })),
        )
        .await;
    assert!(
        status == 403 || status == 404,
        "and the run route refuses it: {status} {why}"
    );
    let (_, detail) = h.call(&owner, "GET", &format!("/recipes/{id}"), None).await;
    assert_eq!(
        granted_bots(&detail),
        vec![owners_bot.clone()],
        "the owner's page lists only grants that still run"
    );
    assert!(h.offered(&owners_bot).await.contains(&id));

    // ---- shared again, and this time the colleague declines ----
    h.share_accept_grant(&owner, &colleague, &id, &bot).await;
    assert!(h.offered(&bot).await.contains(&id));
    let (status, body) = h
        .call(&colleague, "POST", &format!("/recipes/{id}/decline"), None)
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        !h.offered(&bot).await.contains(&id),
        "a declined recipe is not offered to the bot it was granted to"
    );
    let (status, why) = h
        .call(
            &colleague,
            "POST",
            &format!("/recipes/{id}/run"),
            Some(json!({ "coworkerId": bot })),
        )
        .await;
    assert!(status == 403 || status == 404, "{status} {why}");
    let (_, detail) = h.call(&owner, "GET", &format!("/recipes/{id}"), None).await;
    assert_eq!(granted_bots(&detail), vec![owners_bot]);
}
