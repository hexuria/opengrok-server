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
    seed_account_in(store, email, None).await
}

async fn seed_account_in(store: &PgStore, email: &str, org: Option<&str>) -> AccountId {
    let id = AccountId::new();
    let hash = hash_password("password1").expect("hash");
    let at_ms = now_ms();
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.to_string(),
            password_hash: hash.clone(),
            first_name: "Test".to_string(),
            last_name: "User".to_string(),
            org_id: org.unwrap_or("").to_string(),
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

/// One org for the whole file: sharing is org-scoped, so a test with no org can only ever show
/// the refusal half.
const ORG: &str = "org_visibility_test";

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
    let account = seed_account_in(&store, email, Some(ORG)).await;
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
        self.api_as(method, body, None).await
    }

    /// The same call, made BY somebody: seam A resolves the caller from this header and falls
    /// back to the deployment's email without it, which is why every test before sharing could
    /// only ever be one person.
    async fn api_as(&self, method: &str, body: Value, access: Option<&str>) -> (u16, Value) {
        let mut req = self
            .client
            .post(format!("{}/api/{method}", self.base))
            .header("authorization", "Bearer test-bearer");
        if let Some(access) = access {
            req = req.header("x-opengrok-account", access);
        }
        let res = req
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

    /// One person's whole transcript, as text, waiting for the turn to land.
    async fn transcript_of(&self, agent: &str, access: &str) -> String {
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let (_, entries) = self
                .api_as("getAgentTranscript", json!({ "id": agent }), Some(access))
                .await;
            let text = entries.to_string();
            if text.contains("send-message") {
                return text;
            }
        }
        panic!("no transcript in 10s");
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

    // Shared, and back again. Both directions are decisions the aggregate records.
    let (status, shared) = h
        .patch(&access, &agent, json!({ "visibility": "org" }))
        .await;
    assert_eq!(status, 200, "{shared}");
    assert_eq!(shared["visibility"], "org", "{shared}");
    assert_eq!(h.row(&agent).await["visibility"], "org");
    let (status, back) = h
        .patch(&access, &agent, json!({ "visibility": "private" }))
        .await;
    assert_eq!(status, 200, "{back}");
    assert_eq!(back["visibility"], "private", "{back}");

    // A word we do not offer is refused rather than defaulted: quietly storing "private" would
    // tell somebody they had shared a coworker they had not.
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

    // Model, role and visibility travel independently on the same route.
    let (status, both) = h
        .patch(
            &access,
            &agent,
            json!({ "role": "Keeps the changelog.", "visibility": "org" }),
        )
        .await;
    assert_eq!(status, 200, "{both}");
    assert_eq!(both["role"], "Keeps the changelog.");
    assert_eq!(both["visibility"], "org");
    let (status, only_role) = h.patch(&access, &agent, json!({ "role": null })).await;
    assert_eq!(status, 200, "{only_role}");
    assert_eq!(
        only_role["visibility"], "org",
        "clearing a role does not unshare a coworker"
    );

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

/// Sharing, end to end: a colleague sees the row, can talk to it, gets their OWN conversation,
/// and cannot manage it. Plus the half that matters more — the two people never see each other's
/// messages, which is the reason the transcript had to be re-keyed before the roster widened.
#[tokio::test]
async fn a_shared_coworker_is_one_coworker_with_a_conversation_each() {
    let database_url = database_or_skip!();
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let ada_email = format!("ada-{tag}@og.local");
    let h = harness(&database_url, &ada_email).await;
    let ada_access = h.access_token(&h.account.clone(), &ada_email);
    let agent = h.hire("Ada").await;

    let bo_email = format!("bo-{tag}@og.local");
    let bo = seed_account_in(&h.store, &bo_email, Some(ORG)).await;
    let bo_access = h.access_token(&bo, &bo_email);

    // An outsider: same server, no org. Sharing with "the org" must not mean everybody.
    let out_email = format!("out-{tag}@og.local");
    let outsider = seed_account_in(&h.store, &out_email, None).await;
    let out_access = h.access_token(&outsider, &out_email);

    let roster_of = |access: String| {
        let h = &h;
        let agent = agent.clone();
        async move {
            let (_, rows) = h.api_as("listAgents", json!({}), Some(&access)).await;
            rows.as_array()
                .and_then(|rows| rows.iter().find(|row| row["id"] == agent).cloned())
        }
    };

    // Private: the colleague cannot see it, and cannot reach it by knowing the id either. 404,
    // not 403 — somebody who may not use a coworker must not learn it exists.
    assert!(roster_of(bo_access.clone()).await.is_none());
    let (status, refused) = h
        .api_as(
            "sendPrompt",
            json!({ "agentId": agent, "prompt": "hi", "clientNonce": format!("bo-0-{tag}") }),
            Some(&bo_access),
        )
        .await;
    assert_eq!(status, 404, "{refused}");

    // Shared with the org.
    let (status, shared) = h
        .patch(&ada_access, &agent, json!({ "visibility": "org" }))
        .await;
    assert_eq!(status, 200, "{shared}");

    // Bo now sees it, and the row says whose it is and that it is not his to manage.
    let his_row = roster_of(bo_access.clone())
        .await
        .expect("a shared coworker is on a colleague's roster");
    assert_eq!(his_row["visibility"], "org", "{his_row}");
    assert_eq!(his_row["mine"], json!(false), "{his_row}");
    assert_eq!(his_row["canManage"], json!(false), "{his_row}");
    assert_eq!(
        his_row["owner"]["id"],
        json!(h.account.as_str()),
        "{his_row}"
    );
    let hers = roster_of(ada_access.clone()).await.expect("still hers");
    assert_eq!(hers["mine"], json!(true), "{hers}");
    assert_eq!(hers["canManage"], json!(true), "{hers}");

    // The outsider still sees nothing and still cannot reach it. An org is a boundary, not a
    // label — this is the assertion that would fail if `org_id` were compared loosely.
    assert!(roster_of(out_access.clone()).await.is_none());
    let (status, _) = h
        .api_as(
            "getAgentTranscript",
            json!({ "id": agent }),
            Some(&out_access),
        )
        .await;
    assert_eq!(status, 404, "an outsider does not learn it exists");

    // Both talk to it. Same coworker, same model, two conversations.
    for (who, access, prompt, nonce) in [
        (
            &ada_email,
            &ada_access,
            "ada speaks",
            format!("ada-1-{tag}"),
        ),
        (&bo_email, &bo_access, "bo speaks", format!("bo-1-{tag}")),
    ] {
        let (status, sent) = h
            .api_as(
                "sendPrompt",
                json!({ "agentId": agent, "prompt": prompt, "clientNonce": nonce }),
                Some(access),
            )
            .await;
        assert_eq!(
            status, 200,
            "{who} could not talk to a shared coworker: {sent}"
        );
    }

    // The whole point. Each reads their own thread and nobody else's.
    let hers = h.transcript_of(&agent, &ada_access).await;
    let his = h.transcript_of(&agent, &bo_access).await;
    assert!(
        hers.contains("ada speaks"),
        "Ada reads what Ada said: {hers}"
    );
    assert!(
        !hers.contains("bo speaks"),
        "and NOT what Bo said — one coworker, two conversations: {hers}"
    );
    assert!(his.contains("bo speaks"), "Bo reads his own: {his}");
    assert!(
        !his.contains("ada speaks"),
        "and not hers, which is the whole reason the entries were re-keyed: {his}"
    );

    // Sharing is not a write grant. Bo may talk to it; he may not repin, rename or unshare it.
    let (status, _) = h
        .patch(&bo_access, &agent, json!({ "visibility": "private" }))
        .await;
    assert_eq!(status, 404, "management stayed with the owner");
    let (status, _) = h
        .patch(&bo_access, &agent, json!({ "role": "mine now" }))
        .await;
    assert_eq!(status, 404);
    assert_eq!(h.row(&agent).await["visibility"], "org", "unchanged");

    // Unsharing takes it back: Bo loses the row and the id stops working again.
    let (status, _) = h
        .patch(&ada_access, &agent, json!({ "visibility": "private" }))
        .await;
    assert_eq!(status, 200);
    assert!(roster_of(bo_access.clone()).await.is_none());
    let (status, _) = h
        .api_as(
            "getAgentTranscript",
            json!({ "id": agent }),
            Some(&bo_access),
        )
        .await;
    assert_eq!(
        status, 404,
        "unsharing is immediate, not until his next sign-in"
    );

    // And Ada's own conversation survived all of it.
    assert!(
        h.transcript_of(&agent, &ada_access)
            .await
            .contains("ada speaks")
    );
}

/// `deleteAgents` takes a LIST of ids, not `agentId`, so the coworker check in the dispatch —
/// which reads `agentId` then `id` — never sees it. And `delete_agents` resolves the DEPLOYMENT
/// account rather than the caller, so it never compares the two either. A colleague who can see
/// a shared coworker can therefore retire it, which contradicts what the roster row promises
/// them (`canManage: false`) and what this file already asserts about repin and unshare.
///
/// Sharing is what makes it reachable: before it, a stranger had to guess an unguessable id.
#[tokio::test]
async fn a_colleague_cannot_retire_a_coworker_they_do_not_own() {
    let database_url = database_or_skip!();
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let ada_email = format!("ada-del-{tag}@og.local");
    let h = harness(&database_url, &ada_email).await;
    let ada_access = h.access_token(&h.account.clone(), &ada_email);
    let agent = h.hire("Ada").await;
    let (status, _) = h
        .patch(&ada_access, &agent, json!({ "visibility": "org" }))
        .await;
    assert_eq!(status, 200);

    let bo_email = format!("bo-del-{tag}@og.local");
    let bo = seed_account_in(&h.store, &bo_email, Some(ORG)).await;
    let bo_access = h.access_token(&bo, &bo_email);

    // He can see it — that is what sharing is for.
    let (_, rows) = h.api_as("listAgents", json!({}), Some(&bo_access)).await;
    assert!(
        rows.as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["id"] == agent)),
        "the colleague should see a shared coworker"
    );

    // He must not be able to end it.
    let (status, deleted) = h
        .api_as("deleteAgents", json!({ "ids": [agent] }), Some(&bo_access))
        .await;
    assert_eq!(
        deleted["deleted"],
        json!(0),
        "a colleague retired a coworker they do not own: {status} {deleted}"
    );

    // And it is still Ada's, still live, still on her roster.
    let (_, hers) = h.api_as("listAgents", json!({}), Some(&ada_access)).await;
    assert!(
        hers.as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["id"] == agent)),
        "the owner's coworker survived a colleague's delete: {hers}"
    );

    // The owner can, of course.
    let (_, mine) = h
        .api_as("deleteAgents", json!({ "ids": [agent] }), Some(&ada_access))
        .await;
    assert_eq!(
        mine["deleted"],
        json!(1),
        "the owner may retire her own: {mine}"
    );
}

/// The gate asks `may_use`, which is true for a colleague on a SHARED coworker. That is right
/// for talking to it and wrong for changing it: seam A's `updateAgent` renames a coworker, and
/// nothing downstream compares the caller to the owner either — it resolves the DEPLOYMENT
/// account. So sharing would hand over the write surface, which is the exact thing the roster's
/// `canManage: false` tells the colleague it does not.
#[tokio::test]
async fn a_colleague_cannot_rename_a_coworker_they_do_not_own() {
    let database_url = database_or_skip!();
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let ada_email = format!("ada-ren-{tag}@og.local");
    let h = harness(&database_url, &ada_email).await;
    let ada_access = h.access_token(&h.account.clone(), &ada_email);
    let agent = h.hire("Ada").await;
    let (status, _) = h
        .patch(&ada_access, &agent, json!({ "visibility": "org" }))
        .await;
    assert_eq!(status, 200);

    let bo_email = format!("bo-ren-{tag}@og.local");
    let bo = seed_account_in(&h.store, &bo_email, Some(ORG)).await;
    let bo_access = h.access_token(&bo, &bo_email);

    let (_, renamed) = h
        .api_as(
            "updateAgent",
            json!({ "id": agent, "profile": { "name": "Bo's now" } }),
            Some(&bo_access),
        )
        .await;
    assert_eq!(
        renamed,
        Value::Null,
        "a colleague renamed a coworker they do not own: {renamed}"
    );

    let (_, rows) = h.api_as("listAgents", json!({}), Some(&ada_access)).await;
    let name = rows
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["id"] == agent))
        .map(|row| row["name"].clone())
        .unwrap_or(Value::Null);
    assert_eq!(name, json!("Ada"), "the owner's coworker kept its name");
}
