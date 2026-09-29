//! A person's connection can be kept, listed, lent to a coworker and taken back (#267).
//!
//! THE BUG THIS EXISTS FOR: the connection's owner did not serialize for a person or a coworker
//! (an internally tagged newtype over a string), so every such `Connected` failed at append and
//! only deployment-wide connections could ever be stored. Nothing tested a person's connection
//! end to end; this is that test, and the wire corpus records its replies for NativeChat.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::AccountId;
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

async fn seed_account(store: &PgStore, email: &str) -> AccountId {
    let id = AccountId::new();
    let at_ms = now_ms();
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.to_string(),
            password_hash: "x".to_string(),
            first_name: "Host".to_string(),
            last_name: String::new(),
            org_id: String::new(),
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
        first_name: "Host".to_string(),
        last_name: String::new(),
        org_id: None,
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

use opengrok_core::connection::{Connection, ConnectionCommand, Owner};

struct Harness {
    base: String,
    store: PgStore,
    minter: Arc<TokenMinter>,
    client: reqwest::Client,
}

async fn harness(database_url: &str) -> Harness {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let minter = Arc::new(TokenMinter::new(b"connections-secret"));
    let agui = AgUiState {
        auth: AuthState::new(store.clone(), minter.clone(), "host@og.local".to_string()),
        door: Arc::new(MockDoor::echoing()),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer: None,
        vault: None,
        connectors: Connectors {
            providers: Arc::new(BTreeMap::new()),
            redirect_uri: "http://127.0.0.1/callback".to_string(),
        },
        plugins: Arc::new(BTreeMap::new()),
        host_settings: None,
    };
    let host = HostState::new(agui.clone(), None);
    let app = opengrok_server::router(agui, host);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base = format!(
        "http://127.0.0.1:{}",
        listener.local_addr().expect("addr").port()
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    Harness {
        base,
        store,
        minter,
        client: reqwest::Client::new(),
    }
}

impl Harness {
    fn token(&self, account: &AccountId, email: &str) -> String {
        self.minter
            .mint_access(
                account.as_str(),
                "sess-connections",
                email,
                "ultra",
                chrono::Utc::now().timestamp(),
                3600,
            )
            .expect("mint access")
    }

    /// A person's own connection, stored the way the OAuth callback stores one.
    async fn connect(&self, account: &AccountId, label: &str) -> String {
        let id = format!("conn_{}", uuid::Uuid::now_v7().simple());
        let mut connection = Connection::default();
        let events = connection
            .decide(ConnectionCommand::Connect {
                connector: "gmail".to_string(),
                owner: Owner::User(account.clone()),
                label: label.to_string(),
                at_ms: now_ms(),
            })
            .expect("connect");
        for event in &events {
            connection.apply(event);
        }
        self.store
            .append_connection(
                &id,
                0,
                &events,
                &connection,
                &opengrok_store::CredentialUpdate::none(now_ms()),
            )
            .await
            .expect("a person's connection can be stored");
        id
    }

    async fn post(&self, token: &str, path: &str, body: Value) -> reqwest::Response {
        self.client
            .post(format!("{}{path}", self.base))
            .header("authorization", format!("Bearer {token}"))
            .json(&body)
            .send()
            .await
            .expect("post")
    }

    async fn list(&self, token: &str) -> Value {
        let res = self
            .client
            .get(format!("{}/connections", self.base))
            .header("authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("list");
        assert_eq!(res.status().as_u16(), 200);
        res.json().await.expect("list json")
    }
}

#[tokio::test]
async fn a_persons_connection_is_kept_listed_lent_and_taken_back() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let email = format!("conn-{}@og.local", uuid::Uuid::now_v7().simple());
    let account = seed_account(&h.store, &email).await;
    let token = h.token(&account, &email);
    let id = h.connect(&account, &email).await;
    let hired: Value = h
        .post(&token, "/coworkers", json!({ "name": "Ada" }))
        .await
        .json()
        .await
        .expect("hire json");
    let coworker = hired["id"].as_str().expect("coworker id").to_string();

    let lent = h
        .post(
            &token,
            &format!("/connections/{id}/lend"),
            json!({ "coworker_id": coworker }),
        )
        .await;
    assert_eq!(lent.status().as_u16(), 200);
    let lent: Value = lent.json().await.expect("lend json");
    assert_eq!(lent["id"], id.as_str(), "{lent}");
    assert_eq!(
        lent["owner"],
        json!({ "scope": "user", "id": account.as_str() }),
        "{lent}"
    );
    assert_eq!(lent["loans"], json!([coworker]), "{lent}");
    assert!(
        lent.get("lentTo").is_none(),
        "one shape for the list and the lend: {lent}"
    );
    assert!(lent["updatedAtMs"].is_i64(), "{lent}");

    let listed = h.list(&token).await;
    let row = listed
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["id"] == id.as_str()))
        .unwrap_or_else(|| panic!("the connection is listed: {listed}"));
    assert_eq!(row, &lent, "the list and the lend read the same");

    let revoked: Value = h
        .post(
            &token,
            &format!("/connections/{id}/revoke"),
            json!({ "coworker_id": coworker }),
        )
        .await
        .json()
        .await
        .expect("revoke json");
    assert_eq!(revoked["loans"], json!([]), "{revoked}");
}

/// Somebody else's connection id lends nothing and says nothing: the same 404 as no connection.
#[tokio::test]
async fn another_accounts_connection_cannot_be_lent() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let owner_email = format!("owner-{}@og.local", uuid::Uuid::now_v7().simple());
    let owner = seed_account(&h.store, &owner_email).await;
    let id = h.connect(&owner, &owner_email).await;
    let other_email = format!("other-{}@og.local", uuid::Uuid::now_v7().simple());
    let other = seed_account(&h.store, &other_email).await;
    let token = h.token(&other, &other_email);

    let refused = h
        .post(
            &token,
            &format!("/connections/{id}/lend"),
            json!({ "coworker_id": "cw_x" }),
        )
        .await;
    assert_eq!(refused.status().as_u16(), 404);
    let listed = h.list(&token).await;
    assert_eq!(
        listed,
        json!([]),
        "nothing of the owner's shows to another account"
    );
}
