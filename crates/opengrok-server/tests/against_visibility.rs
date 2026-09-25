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
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_harness::{DeltaStream, MockDoor, ModelDoor, ModelError, ModelRequest};
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

/// Whose spend each model call was: `(spend_scope, spend_actor)` as the run door built the
/// request. The points guard bills `spend_actor` against the coworker named by `spend_scope`
/// (`spend.rs`, `GuardedDoor`), and the per-person key is minted for the same pair, so this
/// pair is the payer.
struct RecordingDoor {
    inner: MockDoor,
    seen: Mutex<Vec<(Option<String>, Option<String>)>>,
}

#[async_trait]
impl ModelDoor for RecordingDoor {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
        if let Ok(mut seen) = self.seen.lock() {
            seen.push((request.spend_scope.clone(), request.spend_actor.clone()));
        }
        self.inner.stream(request).await
    }
}

struct Harness {
    base: String,
    client: reqwest::Client,
    store: PgStore,
    minter: Arc<TokenMinter>,
    door: Arc<RecordingDoor>,
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
    let door = Arc::new(RecordingDoor {
        inner: MockDoor::echoing_the_system_prompt(),
        seen: Mutex::new(Vec::new()),
    });
    let agui = AgUiState {
        auth,
        door: door.clone(),
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
        door,
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
        self.turn_as_run(who, id, &uuid::Uuid::now_v7().to_string())
            .await
    }

    async fn turn_as_run(&self, who: &Person, id: &str, run_id: &str) -> (u16, String) {
        let res = self
            .client
            .post(format!("{}/ag-ui", self.base))
            .header("Authorization", format!("Bearer {}", who.token))
            .json(&json!({
                "threadId": format!("gateway-{id}"),
                "runId": run_id,
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

    /// The run ids this person's history of the coworker's thread lists — what the app hydrates
    /// the conversation from.
    async fn thread_runs(&self, who: &Person, id: &str) -> Vec<String> {
        let (status, body) = self
            .send(
                who,
                reqwest::Method::GET,
                &format!("/ag-ui/threads/gateway-{id}?events=false"),
                None,
            )
            .await;
        if status == 404 {
            return Vec::new();
        }
        assert_eq!(status, 200, "{body}");
        body["runs"]
            .as_array()
            .map(|runs| {
                runs.iter()
                    .filter_map(|run| run["runId"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
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
    // The owner's bot keys are the owner's: the listing is a 404, not an empty list that reads
    // as "this coworker has no keys".
    let (status, body) = h
        .send(
            &member,
            reqwest::Method::GET,
            &format!("/coworkers/{ada}/keys"),
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

    // Somebody in another org never meets it: every coworker route answers them exactly as it
    // answers an id that does not exist, the run door and the approvals door included — those
    // two said 403 "no grant lets …", which confirmed nothing but was the one place the answer
    // was not the 404 every other per-coworker route gives.
    assert!(h.row(&outsider, &ada).await.is_none());
    let ghost = format!("cw_{}", uuid::Uuid::now_v7().simple());
    for id in [ada.as_str(), ghost.as_str()] {
        let (status, body) = h.turn(&outsider, id).await;
        assert_eq!(
            (status, body.as_str()),
            (404, "no such coworker"),
            "the run door, {id}"
        );
        let (status, body) = h
            .send(
                &outsider,
                reqwest::Method::POST,
                &format!("/coworkers/{id}/approvals"),
                Some(json!({ "tools": [] })),
            )
            .await;
        assert_eq!(
            (status, body),
            (404, Value::String("no such coworker".to_string())),
            "the approvals door, {id}"
        );
        for (method, path, body) in [
            (
                reqwest::Method::PATCH,
                format!("/coworkers/{id}"),
                Some(json!({ "hiddenFromSidebar": true })),
            ),
            (reqwest::Method::DELETE, format!("/coworkers/{id}"), None),
            (reqwest::Method::GET, format!("/coworkers/{id}/tools"), None),
            (reqwest::Method::GET, format!("/coworkers/{id}/spend"), None),
            (reqwest::Method::GET, format!("/coworkers/{id}/usage"), None),
            (reqwest::Method::GET, format!("/coworkers/{id}/limit"), None),
            (reqwest::Method::POST, format!("/coworkers/{id}/keys"), None),
            (reqwest::Method::GET, format!("/coworkers/{id}/keys"), None),
            (
                reqwest::Method::DELETE,
                format!("/coworkers/{id}/keys/{ghost}"),
                None,
            ),
            (
                reqwest::Method::GET,
                format!("/coworkers/{id}/mcp-calls"),
                None,
            ),
            (
                reqwest::Method::GET,
                format!("/coworkers/{id}/computer"),
                None,
            ),
            (
                reqwest::Method::GET,
                format!("/coworkers/{id}/screen"),
                None,
            ),
        ] {
            let (status, answer) = h.send(&outsider, method.clone(), &path, body).await;
            assert_eq!(
                status, 404,
                "a stranger learns nothing: {method} {path}: {answer}"
            );
        }
    }

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
        status, 404,
        "access is decided on every turn, not copied when it was shared — and off the roster, \
         the run door answers as for an id that does not exist: {body}"
    );

    // Revoking the OWNER's grant cuts the member off too: the member's access is the owner's,
    // never wider. It is still on their roster, so the refusal says why.
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
    assert_eq!(status, 404, "{body}");

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
    assert_eq!(status, 404, "{body}");
}

/// A member talking to a shared coworker has a conversation of their own and pays for it.
///
/// Both hold through code older than sharing — the run door names the caller as the payer
/// (`spend_actor`) and mints the per-person key for (coworker, caller), and a thread's history
/// is filtered to the runs its reader owns — but #175 made them reachable by a second person,
/// so they are pinned here rather than assumed: one shared coworker must not become one shared
/// transcript or one shared bill.
#[tokio::test]
async fn a_member_sees_only_their_own_conversation_and_pays_for_it() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let org = unique("org");
    let owner = h.person("Ann", "Owner", Some(&org)).await;
    let member = h.person("Ben", "Member", Some(&org)).await;

    let ada = h.hire(&owner, "Ada").await;
    assert_eq!(h.share(&owner, &ada, "org").await.0, 200);

    let owners_run = uuid::Uuid::now_v7().to_string();
    let (status, sse) = h.turn_as_run(&owner, &ada, &owners_run).await;
    assert_eq!(status, 200, "{sse}");
    let members_run = uuid::Uuid::now_v7().to_string();
    let (status, sse) = h.turn_as_run(&member, &ada, &members_run).await;
    assert_eq!(status, 200, "{sse}");

    // The same thread id — the app names a coworker's conversation after the coworker — and two
    // histories, each holding only its reader's own turn.
    assert_eq!(
        h.thread_runs(&member, &ada).await,
        vec![members_run.clone()],
        "the member's history is the member's turn alone"
    );
    assert_eq!(
        h.thread_runs(&owner, &ada).await,
        vec![owners_run.clone()],
        "and the owner's is the owner's: sharing a coworker is not sharing a transcript"
    );

    // Every model call the member's turn made was billed to the member, on this coworker.
    let seen = h
        .door
        .seen
        .lock()
        .map(|seen| seen.clone())
        .unwrap_or_default();
    let for_ada: Vec<_> = seen
        .iter()
        .filter(|(scope, _)| scope.as_deref() == Some(ada.as_str()))
        .collect();
    assert!(
        for_ada
            .iter()
            .any(|(_, actor)| actor.as_deref() == Some(member.id.as_str())),
        "the member's turn is on the member's spend: {seen:?}"
    );
    assert!(
        for_ada.iter().all(|(_, actor)| {
            actor.as_deref() == Some(member.id.as_str())
                || actor.as_deref() == Some(owner.id.as_str())
        }),
        "each call names the person who asked, never nobody: {seen:?}"
    );
    assert_eq!(
        for_ada
            .iter()
            .filter(|(_, actor)| actor.as_deref() == Some(owner.id.as_str()))
            .count(),
        for_ada
            .iter()
            .filter(|(_, actor)| actor.as_deref() == Some(member.id.as_str()))
            .count(),
        "one turn each, one payer each — the member's turn did not land on the owner: {seen:?}"
    );
}

/// A member's policy read must not deadlock against a boot's schema migration.
///
/// `migrations::run` executes the whole schema as ONE transaction, and its
/// `alter table … if not exists` statements take ACCESS EXCLUSIVE on `coworker_view` first and
/// `grant_view` later, holding both until commit. `policy_to_use` was one statement that locked
/// `grant_view` first and then waited on `coworker_view`. When another replica booted, or
/// another test's harness migrated, Postgres broke the cycle by killing the read, and the turn
/// was refused as "no grant lets …" over a grant that was fine. This holds the migration's two
/// locks in its order, with the read in between.
#[tokio::test]
async fn a_members_policy_read_does_not_deadlock_against_a_migration() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let org = unique("org");
    let owner = h.person("Ann", "Owner", Some(&org)).await;
    let member = h.person("Ben", "Member", Some(&org)).await;
    let ada = h.hire(&owner, "Ada").await;
    assert_eq!(h.share(&owner, &ada, "org").await.0, 200);
    let ada_id = CoworkerId::from_stored(ada.clone());

    let mut migration = h.store.pool().begin().await.expect("begin");
    sqlx::query("set local lock_timeout = '20s'")
        .execute(&mut *migration)
        .await
        .expect("lock timeout");
    sqlx::query("lock table coworker_view in access exclusive mode")
        .execute(&mut *migration)
        .await
        .expect("the migration's first lock");

    let store = h.store.clone();
    let reader = member.id.clone();
    let read_id = ada_id.clone();
    let read = tokio::spawn(async move { store.policy_to_use(&reader, &read_id).await });
    // Long enough for the read to take whatever it takes before it has to wait.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let second = sqlx::query("lock table grant_view in access exclusive mode")
        .execute(&mut *migration)
        .await;
    let committed = migration.commit().await;
    let policy = read.await.expect("join");
    assert!(
        second.is_ok() && committed.is_ok(),
        "the migration was the deadlock's victim: {second:?} {committed:?}"
    );
    let policy = policy.expect("the member's policy read was the deadlock's victim");
    assert!(
        opengrok_policy::decide(
            &member.id,
            &ada_id,
            opengrok_policy::Action::UseCoworker,
            &policy
        )
        .reason()
        .is_none(),
        "and it still reads the owner's grant: {policy:?}"
    );
}
