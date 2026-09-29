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
use opengrok_server::connections::oauth::ProviderConfig;
use opengrok_server::connections::routes::Connectors;
use opengrok_server::host_state::HostState;
use opengrok_store::{PgStore, Vault};
use serde_json::{Value, json};

const KEK: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

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
    vault: Arc<Vault>,
    /// Follows no redirect, so `/authorize`'s answer can be read the way a browser receives it.
    client: reqwest::Client,
}

/// A stand-in for Google's token endpoint. The access token names the code it was exchanged for,
/// so a test can tell which round trip a stored credential came from.
async fn start_provider() -> String {
    let app = axum::Router::new().route(
        "/token",
        axum::routing::post(
            |axum::Form(form): axum::Form<BTreeMap<String, String>>| async move {
                let code = form.get("code").cloned().unwrap_or_default();
                axum::Json(json!({
                    "access_token": format!("token-for-{code}"),
                    "token_type": "Bearer",
                    "expires_in": 3599,
                }))
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the provider");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve the provider");
    });
    base
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
    let vault = Arc::new(Vault::from_base64_key(KEK).expect("vault"));
    let provider = start_provider().await;
    let mut gmail = ProviderConfig::google("gmail", "client-id", "client-secret", &["gmail.send"]);
    gmail.authorize_url = format!("{provider}/authorize");
    gmail.token_url = format!("{provider}/token");
    let agui = AgUiState {
        auth: AuthState::new(store.clone(), minter.clone(), "host@og.local".to_string()),
        door: Arc::new(MockDoor::echoing()),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer: None,
        vault: Some(vault.clone()),
        connectors: Connectors {
            providers: Arc::new(BTreeMap::from([("gmail".to_string(), gmail)])),
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
        vault,
        client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client"),
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

    async fn hire(&self, token: &str) -> String {
        let hired: Value = self
            .post(token, "/coworkers", json!({ "name": "Ada" }))
            .await
            .json()
            .await
            .expect("hire json");
        hired["id"].as_str().expect("coworker id").to_string()
    }

    async fn disconnect(&self, token: &str, id: &str) -> reqwest::Response {
        self.client
            .delete(format!("{}/connections/{id}", self.base))
            .header("authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("disconnect")
    }

    /// Gmail connected the way a browser does it: our `/authorize` sends it to the provider with a
    /// signed state, and the provider sends it back to `/callback` with that state and a code.
    async fn connect_through_the_browser(&self, token: &str, code: &str) -> reqwest::Response {
        let sent = self
            .client
            .get(format!("{}/connections/gmail/authorize", self.base))
            .header("authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("authorize");
        assert!(sent.status().is_redirection(), "{}", sent.status());
        let to_provider = sent
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|location| location.to_str().ok())
            .and_then(|location| reqwest::Url::parse(location).ok())
            .expect("a redirect to the provider");
        let state = to_provider
            .query_pairs()
            .find_map(|(key, value)| (key == "state").then(|| value.into_owned()))
            .expect("a signed state");
        self.client
            .get(format!("{}/connections/callback", self.base))
            .query(&[("code", code), ("state", state.as_str())])
            .send()
            .await
            .expect("callback")
    }
}

fn row<'a>(listed: &'a Value, id: &str) -> &'a Value {
    listed
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["id"] == id))
        .unwrap_or_else(|| panic!("{id} is listed: {listed}"))
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

    // Disconnecting leaves nothing to show: a 204, and the list no longer names it.
    let gone = h
        .client
        .delete(format!("{}/connections/{id}", h.base))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("disconnect");
    assert_eq!(gone.status().as_u16(), 204);
    assert_eq!(h.list(&token).await, json!([]));
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
    let body: Value = refused.json().await.expect("a JSON refusal");
    assert_eq!(body, json!({ "error": "no such connection" }));
    let listed = h.list(&token).await;
    assert_eq!(
        listed,
        json!([]),
        "nothing of the owner's shows to another account"
    );
}

/// A loan hands a coworker the key, so it may only go to a coworker the lender may use. Another
/// account's is refused with the same 404 as no coworker at all, and nothing is lent.
#[tokio::test]
async fn a_connection_cannot_be_lent_to_a_coworker_its_owner_may_not_use() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let email = format!("lender-{}@og.local", uuid::Uuid::now_v7().simple());
    let account = seed_account(&h.store, &email).await;
    let token = h.token(&account, &email);
    let id = h.connect(&account, &email).await;
    let stranger_email = format!("stranger-{}@og.local", uuid::Uuid::now_v7().simple());
    let stranger = seed_account(&h.store, &stranger_email).await;
    let theirs = h.hire(&h.token(&stranger, &stranger_email)).await;

    let refused = h
        .post(
            &token,
            &format!("/connections/{id}/lend"),
            json!({ "coworker_id": theirs }),
        )
        .await;
    assert_eq!(refused.status().as_u16(), 404);
    let body: Value = refused.json().await.expect("a JSON refusal");
    assert_eq!(body, json!({ "error": "no such coworker" }));
    let listed = h.list(&token).await;
    assert_eq!(row(&listed, &id)["loans"], json!([]), "nothing was lent");
}

/// Gmail connected again after a disconnect is kept again. The callback used to refresh the
/// disconnected connection and swallow the refusal: the person read "gmail is connected" and
/// nothing was kept.
#[tokio::test]
async fn a_disconnected_connection_can_be_connected_again() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let email = format!("again-{}@og.local", uuid::Uuid::now_v7().simple());
    let account = seed_account(&h.store, &email).await;
    let token = h.token(&account, &email);
    let id = format!("conn_gmail_{}", account.as_str());

    let first = h.connect_through_the_browser(&token, "first").await;
    assert_eq!(first.status().as_u16(), 200);
    let coworker = h.hire(&token).await;
    let lent = h
        .post(
            &token,
            &format!("/connections/{id}/lend"),
            json!({ "coworker_id": coworker }),
        )
        .await;
    assert_eq!(lent.status().as_u16(), 200);
    assert_eq!(h.disconnect(&token, &id).await.status().as_u16(), 204);
    assert_eq!(h.list(&token).await, json!([]));

    let again = h.connect_through_the_browser(&token, "second").await;
    assert_eq!(again.status().as_u16(), 200);
    let said = again.text().await.expect("callback text");
    assert!(said.starts_with("gmail is connected"), "{said}");
    let listed = h.list(&token).await;
    let kept = row(&listed, &id);
    assert_eq!(
        kept["owner"],
        json!({ "scope": "user", "id": account.as_str() }),
        "{kept}"
    );
    // A loan that outlived its connection would be a key to whatever is put behind that name next.
    assert_eq!(kept["loans"], json!([]), "{kept}");
    assert_eq!(
        h.store
            .open_credential(&h.vault, &id)
            .await
            .expect("open the credential")
            .as_deref(),
        Some("token-for-second"),
        "the new round trip's token is the one kept"
    );
}
