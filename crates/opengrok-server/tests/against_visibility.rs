//! Visibility, the roster's permission fields, and whose consent a remembered "allow once" is.
//!
//! The roster still returns only the caller's own coworkers — widening it before transcripts are
//! keyed per member would put two people in one conversation, which is the thing this slice
//! forbids. So the fields are proven honest now and the widening lands with the entry re-key.
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::AccountId;
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::password::hash_password;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::gateway::GatewayState;
use opengrok_store::PgStore;
use serde_json::{Value, json};

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

async fn seed_account(store: &PgStore, email: &str) -> AccountId {
    let id = AccountId::new();
    let hash = hash_password("password1").expect("hash");
    let at_ms = now_ms();
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.to_string(),
            password_hash: hash.clone(),
            first_name: "Test".to_string(),
            last_name: "User".to_string(),
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
        password_hash: Some(hash),
        first_name: "Test".to_string(),
        last_name: "User".to_string(),
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

struct Harness {
    base: String,
    client: reqwest::Client,
    agui: AgUiState,
    store: PgStore,
    account: AccountId,
}

async fn harness(database_url: &str, email: &str) -> Harness {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let account = seed_account(&store, email).await;
    let auth = AuthState::new(
        store.clone(),
        Arc::new(TokenMinter::new(b"visibility-test-secret")),
        email.to_string(),
    );
    let agui = AgUiState {
        auth,
        // Says back its system prompt: what the model was TOLD is what it answers.
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
    };
    let gateway = GatewayState::new(
        agui.clone(),
        Some("test-bearer".to_string()),
        email.to_string(),
        Some("http://opengrok.lan:1447".to_string()),
    );
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
        client: reqwest::Client::new(),
        agui,
        store,
        account,
    }
}

impl Harness {
    async fn api(&self, method: &str, body: Value) -> (u16, Value) {
        let res = self
            .client
            .post(format!("{}/api/{method}", self.base))
            .header("authorization", "Bearer test-bearer")
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .expect("api call");
        let status = res.status().as_u16();
        let text = res.text().await.expect("body");
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn hire(&self, name: &str) -> String {
        let (status, created) = self
            .api(
                "createAgent",
                json!({ "name": name, "clientNonce": format!("hire-{name}-{}", now_ms()) }),
            )
            .await;
        assert_eq!(status, 200, "{created}");
        created["agent"]["id"].as_str().expect("id").to_string()
    }
}

impl Harness {
    /// The signed-in person's token — what the account API takes, unlike /api/* which takes the
    /// gateway bearer.
    fn access_token(&self, account: &AccountId, email: &str) -> String {
        self.agui
            .auth
            .minter
            .mint_access(
                account.as_str(),
                "sess-test",
                email,
                "ultra",
                chrono::Utc::now().timestamp(),
                3600,
            )
            .expect("mint access")
    }

    async fn patch(&self, access: &str, id: &str, body: Value) -> (u16, Value) {
        let res = self
            .client
            .patch(format!("{}/coworkers/{}", self.base, id))
            .header("Authorization", format!("Bearer {access}"))
            .json(&body)
            .send()
            .await
            .expect("patch");
        let status = res.status().as_u16();
        let text = res.text().await.unwrap_or_default();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn row(&self, id: &str) -> Value {
        let (_, rows) = self.api("listAgents", json!({})).await;
        rows.as_array()
            .and_then(|rows| rows.iter().find(|row| row["id"] == id).cloned())
            .unwrap_or(Value::Null)
    }
}

#[tokio::test]
async fn visibility_is_private_until_its_owner_shares_it_and_the_roster_says_who_may_manage() {
    let database_url = database_or_skip!();
    let email = format!("vis-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let access = h.access_token(&h.account.clone(), &email);
    let agent = h.hire("Ada").await;

    // Private by default. Nobody has to opt out of sharing.
    let row = h.row(&agent).await;
    assert_eq!(row["visibility"], "private", "{row}");
    assert_eq!(row["mine"], json!(true), "{row}");
    assert_eq!(row["canManage"], json!(true), "{row}");
    assert_eq!(row["owner"]["id"], json!(h.account.as_str()), "{row}");
    assert!(
        row["owner"]["name"].as_str().is_some(),
        "a shared row has to be able to say whose it is: {row}"
    );

    // `org` is refused, and the refusal is the point of this PR. Nothing reads visibility yet:
    // the roster is still owner-scoped and a transcript is still one thread per coworker. A 200
    // here would report `visibility: "org"` back and share nothing — telling somebody their work
    // is visible to their org when it is not. A recognised word that cannot take effect is
    // refused exactly like a word we do not have.
    let (status, refused) = h
        .patch(&access, &agent, json!({ "visibility": "org" }))
        .await;
    assert_eq!(status, 400, "{refused}");
    let sentence = refused["error"].as_str().unwrap_or_default();
    assert!(
        sentence.contains("sharing is not switched on yet"),
        "the refusal has to say why, not just no: {refused}"
    );
    assert!(
        sentence.contains("shared when it is not"),
        "and it has to say what the refusal is protecting: {refused}"
    );
    assert_eq!(
        h.row(&agent).await["visibility"],
        "private",
        "a refused share stores nothing"
    );

    // Setting it to what it already is still answers, so a client that sends the whole row back
    // is not refused for saying "private".
    let (status, same) = h
        .patch(&access, &agent, json!({ "visibility": "private" }))
        .await;
    assert_eq!(status, 200, "{same}");
    assert_eq!(same["visibility"], "private", "{same}");

    // A word we do not offer at all is refused differently, and says so.
    let (status, refused) = h
        .patch(&access, &agent, json!({ "visibility": "public" }))
        .await;
    assert_eq!(status, 400, "{refused}");
    assert_eq!(
        refused["error"],
        "visibility: 'public' is not one of private, org"
    );
    let (status, _) = h.patch(&access, &agent, json!({ "visibility": 1 })).await;
    assert_eq!(status, 400);
    assert_eq!(h.row(&agent).await["visibility"], "private", "unchanged");

    // Model, role and visibility travel independently on the same route, and a refused
    // visibility refuses the WHOLE patch rather than half-applying the role beside it.
    let (status, refused) = h
        .patch(
            &access,
            &agent,
            json!({ "role": "Keeps the changelog.", "visibility": "org" }),
        )
        .await;
    assert_eq!(status, 400, "{refused}");
    assert_eq!(
        h.row(&agent).await["role"],
        Value::Null,
        "the role did not land beside a refused share"
    );
    let (status, only_role) = h
        .patch(&access, &agent, json!({ "role": "Keeps the changelog." }))
        .await;
    assert_eq!(status, 200, "{only_role}");
    assert_eq!(only_role["role"], "Keeps the changelog.");
    assert_eq!(only_role["visibility"], "private");

    // Another account still cannot touch it, shared or not — sharing is not a write grant, and
    // the roster does not widen until transcripts are per member.
    let stranger = format!("stranger-{}@og.local", uuid::Uuid::now_v7().simple());
    let stranger_id = seed_account(&h.store, &stranger).await;
    let stranger_access = h.access_token(&stranger_id, &stranger);
    let (status, _) = h
        .patch(&stranger_access, &agent, json!({ "visibility": "private" }))
        .await;
    assert_eq!(status, 404);
}

/// A remembered "allow once" belongs to the person who gave it. Without this, sharing a coworker
/// would let one member's yes authorise another member's command — a consent record that fails
/// open, which CLAUDE.md #8 forbids.
#[tokio::test]
async fn one_members_allow_once_cannot_be_spent_by_another() {
    let database_url = database_or_skip!();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);

    let coworker = opengrok_core::id::CoworkerId::new();
    let args = json!({ "command": "echo hi" });
    let at_ms = chrono::Utc::now().timestamp_millis();
    let ttl = 10 * 60 * 1_000;
    let ada = "acct_ada";
    let bo = "acct_bo";

    store
        .remember_mcp_allow_once(
            opengrok_store::AllowOnce {
                coworker: &coworker,
                account: Some(ada),
                tool: "shell",
                arguments: &args,
                call_id: "call-1",
                gate: true,
                at_ms,
            },
            ttl,
        )
        .await
        .expect("remember");

    // Bo cannot spend Ada's yes, and neither can an anonymous caller.
    assert!(
        store
            .take_mcp_allow_once(&coworker, Some(bo), "shell", &args, at_ms, ttl)
            .await
            .expect("take")
            .is_none(),
        "another member's yes is not this member's consent"
    );
    assert!(
        store
            .take_mcp_allow_once(&coworker, None, "shell", &args, at_ms, ttl)
            .await
            .expect("take")
            .is_none(),
        "and an unauthenticated caller cannot spend it either"
    );

    // Ada can, exactly once.
    let taken = store
        .take_mcp_allow_once(&coworker, Some(ada), "shell", &args, at_ms, ttl)
        .await
        .expect("take")
        .expect("her own yes");
    assert_eq!(taken.0, "call-1");
    assert!(taken.1, "the gate flag survives");
    assert!(
        store
            .take_mcp_allow_once(&coworker, Some(ada), "shell", &args, at_ms, ttl)
            .await
            .expect("take")
            .is_none(),
        "once means once"
    );

    // A row written before consent carried an account (NULL) stays takeable only by a caller
    // with no account — the pre-sharing behaviour — and never by somebody else.
    store
        .remember_mcp_allow_once(
            opengrok_store::AllowOnce {
                coworker: &coworker,
                account: None,
                tool: "shell",
                arguments: &args,
                call_id: "call-legacy",
                gate: false,
                at_ms,
            },
            ttl,
        )
        .await
        .expect("remember");
    assert!(
        store
            .take_mcp_allow_once(&coworker, Some(ada), "shell", &args, at_ms, ttl)
            .await
            .expect("take")
            .is_none(),
        "an accountless yes is not everybody's"
    );
    assert_eq!(
        store
            .take_mcp_allow_once(&coworker, None, "shell", &args, at_ms, ttl)
            .await
            .expect("take")
            .expect("the legacy row")
            .0,
        "call-legacy"
    );
}
