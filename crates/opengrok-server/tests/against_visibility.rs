//! A coworker shared with the org is one the org can MEET: on every member's roster, and
//! answering them — while management stays with the person who hired it.
//!
//! `PATCH visibility=org` answered 200 and shared nothing. The roster was the owner-only
//! `coworkers_for`, and the run door asked `policy_for`, which reads a grant row by principal —
//! and only the hirer ever gets one. So an invited teammate signed in to an empty sidebar, and
//! even holding the id was refused with "no grant lets … use coworker". These drive the real HTTP
//! surface with three people: the owner, a member of the same org, and somebody in another org.
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

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

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::now_v7().simple())
}

struct Person {
    id: AccountId,
    token: String,
}

struct Harness {
    base: String,
    client: reqwest::Client,
    store: PgStore,
    minter: Arc<TokenMinter>,
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
    let minter = Arc::new(TokenMinter::new(b"visibility-test-secret"));
    let auth = AuthState::new(store.clone(), minter.clone(), "owner@og.local".to_string());
    let agui = AgUiState {
        auth,
        door: Arc::new(MockDoor::echoing_the_system_prompt()),
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
        client: reqwest::Client::new(),
        store,
        minter,
    }
}

impl Harness {
    /// A signed-in person, in `org` or in none. The org lands on both the command and the view,
    /// because `may_use_coworker` reads the projection and the aggregate is what a later load
    /// would rebuild it from.
    async fn person(&self, first: &str, last: &str, org: Option<&str>) -> Person {
        let id = AccountId::new();
        let email = format!("{}@og.local", unique(&first.to_lowercase()));
        let at_ms = chrono::Utc::now().timestamp_millis();
        let events = Account::default()
            .decide(AccountCommand::Register {
                email: email.clone(),
                password_hash: "x".to_string(),
                first_name: first.to_string(),
                last_name: last.to_string(),
                org_id: org.unwrap_or_default().to_string(),
                plan: Plan::Ultra,
                verified: true,
                enabled: true,
                at_ms,
            })
            .expect("register");
        let view = AccountView {
            id: id.clone(),
            email: email.clone(),
            plan: Plan::Ultra,
            trial: false,
            updated_at_ms: at_ms,
            password_hash: Some("x".to_string()),
            first_name: first.to_string(),
            last_name: last.to_string(),
            org_id: org.map(str::to_string),
            verified: true,
            enabled: true,
            avatar_url: None,
        };
        self.store
            .append_account(&id, 0, &events, &view)
            .await
            .expect("append account");
        let token = self
            .minter
            .mint_access(
                id.as_str(),
                "sess-visibility",
                &email,
                "ultra",
                chrono::Utc::now().timestamp(),
                3600,
            )
            .expect("mint access");
        Person { id, token }
    }

    async fn send(
        &self,
        who: &Person,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base))
            .header("Authorization", format!("Bearer {}", who.token));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let res = request.send().await.expect("request");
        let status = res.status().as_u16();
        let text = res.text().await.unwrap_or_default();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn hire(&self, who: &Person, name: &str) -> String {
        let (status, body) = self
            .send(
                who,
                reqwest::Method::POST,
                "/coworkers",
                Some(json!({ "name": name })),
            )
            .await;
        assert_eq!(status, 201, "hire {name}: {body}");
        body["id"].as_str().expect("id").to_string()
    }

    async fn share(&self, who: &Person, id: &str, visibility: &str) -> (u16, Value) {
        self.send(
            who,
            reqwest::Method::PATCH,
            &format!("/coworkers/{id}"),
            Some(json!({ "visibility": visibility })),
        )
        .await
    }

    async fn roster(&self, who: &Person) -> Vec<Value> {
        let (status, body) = self
            .send(who, reqwest::Method::GET, "/coworkers", None)
            .await;
        assert_eq!(status, 200, "{body}");
        body.as_array()
            .cloned()
            .unwrap_or_else(|| panic!("the roster is an array, always: {body}"))
    }

    async fn row(&self, who: &Person, id: &str) -> Option<Value> {
        self.roster(who)
            .await
            .into_iter()
            .find(|row| row["id"] == id)
    }

    /// One turn on the AG-UI door, as the app sends it. The status and the whole SSE body.
    async fn turn(&self, who: &Person, id: &str) -> (u16, String) {
        let res = self
            .client
            .post(format!("{}/ag-ui", self.base))
            .header("Authorization", format!("Bearer {}", who.token))
            .json(&json!({
                "threadId": format!("gateway-{id}"),
                "runId": uuid::Uuid::now_v7().to_string(),
                "messages": [{
                    "id": unique("m"),
                    "role": "user",
                    "content": "hello",
                }],
                "forwardedProps": { "coworkerId": id },
            }))
            .send()
            .await
            .expect("ag-ui turn");
        let status = res.status().as_u16();
        (status, res.text().await.unwrap_or_default())
    }
}

#[tokio::test]
async fn an_org_visible_coworker_is_on_a_members_roster_and_answers_them() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let org = unique("org");
    let owner = h.person("Ann", "Owner", Some(&org)).await;
    let member = h.person("Ben", "Member", Some(&org)).await;
    let outsider = h.person("Cy", "Outsider", Some(&unique("other-org"))).await;

    let ada = h.hire(&owner, "Ada").await;
    let (status, shared) = h.share(&owner, &ada, "org").await;
    assert_eq!(status, 200, "{shared}");
    assert_eq!(shared["visibility"], "org", "{shared}");
    assert_eq!(shared["mine"], true, "{shared}");
    assert_eq!(shared["canManage"], true, "{shared}");

    // The member's roster lists it — and says, per row, that it is not theirs to manage.
    let row = h
        .row(&member, &ada)
        .await
        .expect("a coworker shared with the org is on a member's roster");
    assert_eq!(row["visibility"], "org", "{row}");
    assert_eq!(row["mine"], false, "{row}");
    assert_eq!(
        row["canManage"], false,
        "sharing is not a write grant: {row}"
    );
    assert_eq!(row["owner"]["id"], owner.id.as_str(), "{row}");
    assert_eq!(row["owner"]["name"], "Ann Owner", "whose it is: {row}");
    let own = h.row(&owner, &ada).await.expect("the owner still lists it");
    assert_eq!(own["mine"], true, "{own}");
    assert_eq!(own["canManage"], true, "{own}");

    // And it answers them.
    let (status, sse) = h.turn(&member, &ada).await;
    assert_eq!(status, 200, "a member may talk to a shared coworker: {sse}");
    assert!(sse.contains("RUN_FINISHED"), "{sse}");

    // Management stays with the owner. The member already knows the coworker exists — it is on
    // their roster — so the PATCH says why rather than pretending it is not there; the routes
    // gated on `owned_coworker` keep answering a non-owner 404.
    let (status, body) = h
        .send(
            &member,
            reqwest::Method::PATCH,
            &format!("/coworkers/{ada}"),
            Some(json!({ "name": "Mine now" })),
        )
        .await;
    assert_eq!(status, 403, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .starts_with("only the person who hired this coworker can change it"),
        "{body}"
    );
    let (status, body) = h
        .send(
            &member,
            reqwest::Method::PATCH,
            &format!("/coworkers/{ada}"),
            Some(json!({ "visibility": "private", "hiddenFromSidebar": true })),
        )
        .await;
    assert_eq!(status, 403, "a member cannot unshare it either: {body}");
    let (status, body) = h
        .send(
            &member,
            reqwest::Method::DELETE,
            &format!("/coworkers/{ada}"),
            None,
        )
        .await;
    assert_eq!(status, 404, "{body}");
    let (status, body) = h
        .send(
            &member,
            reqwest::Method::GET,
            &format!("/coworkers/{ada}/tools"),
            None,
        )
        .await;
    assert_eq!(status, 404, "{body}");
    // Nor can a member mint a grant of their own through the approvals door.
    let (status, body) = h
        .send(
            &member,
            reqwest::Method::POST,
            &format!("/coworkers/{ada}/approvals"),
            Some(json!({ "tools": [] })),
        )
        .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(
        h.row(&owner, &ada).await.expect("still listed")["name"],
        "Ada",
        "every refusal above stored nothing"
    );

    // The member's sidebar is theirs, though: hiding a shared coworker is their own choice and
    // touches nobody else's.
    let (status, hidden) = h
        .send(
            &member,
            reqwest::Method::PATCH,
            &format!("/coworkers/{ada}"),
            Some(json!({ "hiddenFromSidebar": true })),
        )
        .await;
    assert_eq!(status, 200, "{hidden}");
    assert_eq!(hidden["hiddenFromSidebar"], true, "{hidden}");
    assert_eq!(hidden["canManage"], false, "{hidden}");
    assert_eq!(
        h.row(&member, &ada).await.expect("listed")["hiddenFromSidebar"],
        true
    );
    assert_eq!(
        h.row(&owner, &ada).await.expect("listed")["hiddenFromSidebar"],
        false,
        "one member's hide is not the owner's"
    );

    // Somebody in another org never meets it.
    assert!(h.row(&outsider, &ada).await.is_none());
    let (status, body) = h.turn(&outsider, &ada).await;
    assert_eq!(status, 403, "{body}");
    let (status, body) = h
        .send(
            &outsider,
            reqwest::Method::PATCH,
            &format!("/coworkers/{ada}"),
            Some(json!({ "hiddenFromSidebar": true })),
        )
        .await;
    assert_eq!(status, 404, "a stranger learns nothing: {body}");

    // The grant the member's turn runs under is the OWNER's, read on this turn and addressed to
    // the member — and no grant row was written for them, so there is nothing to go stale.
    let ada_id = CoworkerId::from_stored(ada.clone());
    let usable = h
        .store
        .policy_to_use(&member.id, &ada_id)
        .await
        .expect("policy to use");
    assert!(
        opengrok_policy::decide(
            &member.id,
            &ada_id,
            opengrok_policy::Action::UseCoworker,
            &usable
        )
        .reason()
        .is_none(),
        "{usable:?}"
    );
    let strict = h
        .store
        .policy_for(&member.id, &ada_id)
        .await
        .expect("policy for");
    assert!(
        strict.grant.is_none(),
        "sharing writes no grant row: {strict:?}"
    );
}

#[tokio::test]
async fn unsharing_takes_it_back_on_the_next_turn() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let org = unique("org");
    let owner = h.person("Ann", "Owner", Some(&org)).await;
    let member = h.person("Ben", "Member", Some(&org)).await;

    let ada = h.hire(&owner, "Ada").await;
    assert_eq!(h.share(&owner, &ada, "org").await.0, 200);
    assert_eq!(h.turn(&member, &ada).await.0, 200);

    let (status, body) = h.share(&owner, &ada, "private").await;
    assert_eq!(status, 200, "{body}");
    assert!(
        h.row(&member, &ada).await.is_none(),
        "a private coworker is off the member's roster"
    );
    let (status, body) = h.turn(&member, &ada).await;
    assert_eq!(
        status, 403,
        "access is decided on every turn, not copied when it was shared: {body}"
    );

    // Revoking the OWNER's grant cuts the member off too: the member's access is the owner's,
    // never wider.
    assert_eq!(h.share(&owner, &ada, "org").await.0, 200);
    assert_eq!(h.turn(&member, &ada).await.0, 200);
    h.store
        .revoke_access(
            &owner.id,
            &CoworkerId::from_stored(ada.clone()),
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .expect("revoke");
    let (status, body) = h.turn(&member, &ada).await;
    assert_eq!(status, 403, "{body}");
}

#[tokio::test]
async fn a_private_coworker_and_an_orgless_owner_share_with_nobody() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let org = unique("org");
    let owner = h.person("Ann", "Owner", Some(&org)).await;
    let member = h.person("Ben", "Member", Some(&org)).await;

    // Private is the default: an org-mate neither sees it nor reaches it.
    let bob = h.hire(&owner, "Bob").await;
    assert!(h.row(&member, &bob).await.is_none());
    let (status, body) = h.turn(&member, &bob).await;
    assert_eq!(status, 403, "{body}");

    // Two people with no org share nothing — and the one who asks to share is told so rather
    // than answered 200 for a sharing that reaches nobody.
    let loner = h.person("Dee", "Loner", None).await;
    let other = h.person("Eve", "Other", None).await;
    let dot = h.hire(&loner, "Dot").await;
    let (status, refused) = h.share(&loner, &dot, "org").await;
    assert_eq!(status, 400, "{refused}");
    assert!(
        refused["error"]
            .as_str()
            .unwrap_or_default()
            .starts_with("visibility: "),
        "a sentence the person can act on: {refused}"
    );
    let row = h.row(&loner, &dot).await.expect("listed");
    assert_eq!(row["visibility"], "private", "nothing was stored: {row}");
    assert!(h.row(&other, &dot).await.is_none());
    let (status, body) = h.turn(&other, &dot).await;
    assert_eq!(status, 403, "{body}");
}
