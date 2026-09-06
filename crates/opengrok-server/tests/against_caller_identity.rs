//! A routine belongs to the person who set it, and a duplicate to the person who made it.
//!
//! Both used to belong to the DEPLOYMENT account: five seam-A handlers resolved
//! `account(state, &state.email)` and never looked at the caller. That pooled every member's
//! routines into one identity — invisible to their owners, and listable, editable and deletable
//! by anyone signed in. Needs Postgres; skips loudly without OG_DATABASE_URL.

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

/// The existing tests here are about which ACCOUNT owns a routine, not about how a caller is
/// identified, so they speak as the deployment account and opt into the fallback. The identity
/// tests at the bottom of this file use `strict_harness` and must never be switched to this one.
async fn harness(database_url: &str, email: &str) -> Harness {
    build_harness(database_url, email, true).await
}

/// A gateway with the fallback OFF — the shipped default since 5 Sep 2026.
async fn strict_harness(database_url: &str, email: &str) -> Harness {
    build_harness(database_url, email, false).await
}

async fn build_harness(database_url: &str, email: &str, identity_fallback: bool) -> Harness {
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
        Arc::new(TokenMinter::new(b"caller-identity-secret")),
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
    let gateway = if identity_fallback {
        gateway.allowing_identity_fallback()
    } else {
        gateway
    };
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
}

impl Harness {
    async fn api_as(&self, method: &str, body: Value, access: &str) -> (u16, Value) {
        let res = self
            .client
            .post(format!("{}/api/{method}", self.base))
            .header("authorization", "Bearer test-bearer")
            .header("x-opengrok-account", access)
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
}

/// A routine is its author's. Before this, `createAgentAutomation` wrote it to the deployment
/// account, so the person who made it could not see it and everybody else could.
#[tokio::test]
async fn a_routine_belongs_to_whoever_set_it_and_to_nobody_else() {
    let database_url = database_or_skip!();
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let ada_email = format!("ada-auto-{tag}@og.local");
    let h = harness(&database_url, &ada_email).await;
    let ada = h.access_token(&h.account.clone(), &ada_email);
    let agent = h.hire("Ada").await;

    let bo_email = format!("bo-auto-{tag}@og.local");
    let bo_id = seed_account(&h.store, &bo_email).await;
    let bo = h.access_token(&bo_id, &bo_email);

    let (status, made) = h
        .api_as(
            "createAgentAutomation",
            json!({ "agentId": agent, "cron": "0 9 * * 1", "instruction": "weekly report" }),
            &ada,
        )
        .await;
    assert_eq!(status, 200, "{made}");
    let routine = made
        .as_array()
        .and_then(|rows| rows.first())
        .and_then(|row| row["id"].as_str())
        .expect("the routine's id")
        .to_string();

    // Hers, and she can see it. This is the half that was simply broken: her own routine was
    // written to somebody else's account and vanished from her list.
    let (_, hers) = h
        .api_as("getAgentAutomations", json!({ "id": agent }), &ada)
        .await;
    assert!(
        hers.as_array()
            .is_some_and(|rows| rows.iter().any(|r| r["id"] == routine.as_str())),
        "the author cannot see her own routine: {hers}"
    );

    // Not Bo's, in any of the four ways he could reach it.
    let (_, his) = h.api_as("listAllAutomations", json!({}), &bo).await;
    assert_eq!(
        his,
        json!([]),
        "another account's routines were listed to a stranger: {his}"
    );

    let (_, renamed) = h
        .api_as(
            "updateAgentAutomation",
            json!({
                "id": agent, "automationId": routine,
                "cron": "0 9 * * 1", "instruction": "changed by a stranger"
            }),
            &bo,
        )
        .await;
    assert!(
        !renamed.to_string().contains("changed by a stranger"),
        "a stranger edited a routine that is not theirs: {renamed}"
    );

    let (_, _) = h
        .api_as(
            "deleteAgentAutomation",
            json!({ "id": agent, "automationId": routine }),
            &bo,
        )
        .await;
    let (_, still) = h
        .api_as("getAgentAutomations", json!({ "id": agent }), &ada)
        .await;
    assert!(
        still
            .as_array()
            .is_some_and(|rows| rows.iter().any(|r| r["id"] == routine.as_str())),
        "a stranger deleted a routine that is not theirs: {still}"
    );

    // And its author still governs it.
    let (_, mine) = h
        .api_as(
            "setAgentAutomationEnabled",
            json!({ "id": routine, "enabled": false }),
            &ada,
        )
        .await;
    assert!(
        mine.as_array().is_some_and(|rows| rows
            .iter()
            .any(|r| r["id"] == routine.as_str() && r["enabled"] == json!(false))),
        "the author cannot disable her own routine: {mine}"
    );
}

/// A duplicate belongs to the person who asked for it. It used to be written to the DEPLOYMENT
/// account, so a member's copy landed on somebody else's roster and never appeared on their own.
///
/// The caller here is Bo, deliberately. The harness's own email IS the deployment email, so a
/// test where the owner duplicates their own coworker passes either way and demonstrates
/// nothing — the two identities coincide. Only a caller who is not the deployment can show it.
#[tokio::test]
async fn a_duplicate_belongs_to_the_caller_not_the_deployment() {
    let database_url = database_or_skip!();
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let host_email = format!("host-dup-{tag}@og.local");
    let h = harness(&database_url, &host_email).await;

    let bo_email = format!("bo-dup-{tag}@og.local");
    let bo_id = seed_account(&h.store, &bo_email).await;
    let bo = h.access_token(&bo_id, &bo_email);

    let (status, made) = h
        .api_as(
            "createAgent",
            json!({ "name": "Bo's own", "clientNonce": format!("n-{tag}") }),
            &bo,
        )
        .await;
    assert_eq!(status, 200, "{made}");
    let agent = made["agent"]["id"]
        .as_str()
        .or_else(|| made["id"].as_str())
        .expect("the new coworker's id")
        .to_string();

    let (status, copy) = h
        .api_as("duplicateAgent", json!({ "id": agent }), &bo)
        .await;
    assert_eq!(status, 200, "{copy}");
    let copy_id = copy["agent"]["id"]
        .as_str()
        .or_else(|| copy["id"].as_str())
        .expect("the copy's id")
        .to_string();
    assert_ne!(copy_id, agent, "a duplicate is a new coworker");

    let (_, his) = h.api_as("listAgents", json!({}), &bo).await;
    assert!(
        his.as_array()
            .is_some_and(|rows| rows.iter().any(|r| r["id"] == copy_id.as_str())),
        "the copy is not on the roster of the person who made it: {his}"
    );

    // And it did not land on the deployment's roster instead.
    let host = h.access_token(&h.account.clone(), &host_email);
    let (_, theirs) = h.api_as("listAgents", json!({}), &host).await;
    assert!(
        !theirs
            .as_array()
            .is_some_and(|rows| rows.iter().any(|r| r["id"] == copy_id.as_str())),
        "somebody else's copy appeared on the deployment account's roster: {theirs}"
    );
}

/// A seam-A call with NO account identity is refused, not served as the deployment account.
///
/// This is the 5 Sep 2026 incident as a test. The desktop attaches the account header once per
/// CONNECTION; one empty read of its token secret at connect time dropped the header for the whole
/// life of that connection, and every call over it — `listAgents` through `sendPrompt` — was served
/// as `OG_GATEWAY_EMAIL`, which on that deployment was the org admin. The person saw another
/// account's coworkers and could have written to that account's transcripts. Nothing degraded,
/// nothing retried, nothing logged.
///
/// The code matters as much as the status: the header is attached per connect, so the client must
/// REBUILD its connection (re-reading the secret) rather than retry the call, which would carry
/// the same absence. `account_identity_required` is what tells it which.
#[tokio::test]
async fn a_call_with_no_account_identity_is_refused_not_served_as_the_deployment() {
    let url = database_or_skip!();
    // A unique deployment email per test: the seed is an append and a repeat is a Conflict.
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let harness = strict_harness(&url, &format!("deployment-{tag}@og.local")).await;

    let (status, body) = harness.api("listAgents", json!({})).await;

    assert_eq!(status, 401, "{body}");
    assert_eq!(body["code"], json!("account_identity_required"), "{body}");
    assert!(
        !body["error"].as_str().unwrap_or_default().is_empty(),
        "a refusal must say why in words too: {body}"
    );
}

/// A header that does not verify is refused DIFFERENTLY, and never falls back.
///
/// Two codes, because they need different client behaviour. An absent header is a connection that
/// failed to read its secret — rebuilding fixes it. A present-but-invalid one is an expired or
/// wrong token, where rebuilding re-attaches the same dead credential and would spin forever. So
/// `account_identity_invalid` must surface rather than trigger a reconnect.
///
/// It is also refused even where the fallback is opted in: an invalid header is an active claim we
/// rejected, and answering it as somebody else would be the original bug wearing a signature.
#[tokio::test]
async fn an_unverifiable_account_header_is_refused_as_invalid() {
    let url = database_or_skip!();
    // A unique deployment email per test: the seed is an append and a repeat is a Conflict.
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let harness = strict_harness(&url, &format!("deployment-{tag}@og.local")).await;

    let (status, body) = harness
        .api_as("listAgents", json!({}), "not-a-real-token")
        .await;

    assert_eq!(status, 401, "{body}");
    assert_eq!(body["code"], json!("account_identity_invalid"), "{body}");
}

/// 401, never 403. 403 asserts we know who the caller is and are refusing them, which is a lie
/// about the absent case — not knowing is the entire condition being reported.
#[tokio::test]
async fn identity_refusals_are_401_and_never_403() {
    let url = database_or_skip!();
    // A unique deployment email per test: the seed is an append and a repeat is a Conflict.
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let harness = strict_harness(&url, &format!("deployment-{tag}@og.local")).await;

    for (label, status) in [
        ("absent", harness.api("listAgents", json!({})).await.0),
        (
            "invalid",
            harness.api_as("listAgents", json!({}), "nope").await.0,
        ),
    ] {
        assert_eq!(status, 401, "the {label} case must be 401, not 403");
    }
}

/// The event stream refuses an unidentified opener instead of adopting the deployment account.
///
/// The 5 Sep 2026 fail-closed change landed on the seam-A dispatch and left the stream open: it
/// resolved its audience through `caller_email`, which falls back to `OG_GATEWAY_EMAIL`. A stream
/// that offered no identity was therefore given the deployment account as its audience and
/// received that account's ADDRESSED frames — the delivery the audience check exists to prevent.
/// It never showed up in the `seam-A call` log lines either, because a stream open is not one.
#[tokio::test]
async fn the_event_stream_refuses_an_unidentified_opener() {
    let url = database_or_skip!();
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let harness = strict_harness(&url, &format!("deployment-{tag}@og.local")).await;

    let res = harness
        .client
        .get(format!("{}/events", harness.base))
        .header("authorization", "Bearer test-bearer")
        .send()
        .await
        .expect("stream open");

    assert_eq!(res.status().as_u16(), 401);
    let body: Value = serde_json::from_str(&res.text().await.expect("body")).expect("json");
    assert_eq!(body["code"], json!("account_identity_required"), "{body}");
}

/// A stream carrying a header that does not verify is refused as invalid, not as absent — the
/// client must renew rather than merely rebuild, and only the code distinguishes those.
#[tokio::test]
async fn the_event_stream_refuses_an_unverifiable_opener_as_invalid() {
    let url = database_or_skip!();
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let harness = strict_harness(&url, &format!("deployment-{tag}@og.local")).await;

    let res = harness
        .client
        .get(format!("{}/events", harness.base))
        .header("authorization", "Bearer test-bearer")
        .header("x-opengrok-account", "not-a-real-token")
        .send()
        .await
        .expect("stream open");

    assert_eq!(res.status().as_u16(), 401);
    let body: Value = serde_json::from_str(&res.text().await.expect("body")).expect("json");
    assert_eq!(body["code"], json!("account_identity_invalid"), "{body}");
}

/// `searchAgents` searches the CALLER's roster, not the deployment's.
///
/// It took no caller at all and filtered the deployment-wide read, so the command palette matched
/// against `OG_GATEWAY_EMAIL`'s coworkers for whoever typed in it. Metadata rather than message
/// content — rows carry `lastMessagePreview: null` and the ownership gate still refused the
/// transcripts — but somebody else's coworker names and descriptions all the same. It is also the
/// shape the authorisation gate above the dispatch cannot catch: no id in the arguments means
/// `names_a_coworker` has nothing to check, so the verb has to scope itself.
#[tokio::test]
async fn search_agents_matches_only_the_callers_own_coworkers() {
    let url = database_or_skip!();
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let deployment = format!("deployment-{tag}@og.local");
    let h = harness(&url, &deployment).await;

    // The deployment account hires one; a DIFFERENT signed-in person hires another.
    h.hire("Findme").await;
    let other_email = format!("other-{tag}@og.local");
    let other = seed_account(&h.store, &other_email).await;
    let other_token = h.access_token(&other, &other_email);
    let (status, created) = h
        .api_as(
            "createAgent",
            json!({ "name": "Findmetoo", "clientNonce": format!("s-{tag}") }),
            &other_token,
        )
        .await;
    assert_eq!(status, 200, "{created}");

    // "findme" matches both by name — but each caller may only see their own.
    let (status, hits) = h
        .api_as("searchAgents", json!({ "query": "findme" }), &other_token)
        .await;
    assert_eq!(status, 200, "{hits}");
    let names: Vec<&str> = hits
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|row| row["name"].as_str())
        .collect();
    assert_eq!(
        names,
        vec!["Findmetoo"],
        "the other account's search leaked: {hits}"
    );
}

/// Read `data:` frames from an open SSE response until one arrives or the wait runs out.
async fn next_frame(res: &mut reqwest::Response, buffer: &mut String, secs: u64) -> Option<Value> {
    loop {
        if let Some(start) = buffer.find("data: ")
            && let Some(end) = buffer[start..].find("\n\n")
        {
            let line = buffer[start + 6..start + end].to_string();
            buffer.replace_range(..start + end + 2, "");
            return serde_json::from_str(&line).ok();
        }
        let chunk =
            match tokio::time::timeout(std::time::Duration::from_secs(secs), res.chunk()).await {
                Ok(Ok(Some(bytes))) => bytes,
                // Timed out, or the stream ended: no frame, which for this test is the point.
                _ => return None,
            };
        buffer.push_str(&String::from_utf8_lossy(&chunk));
    }
}

/// A stream opens on its OWN roster, and never receives another account's frames.
///
/// #58, and it was two bugs sharing a mechanism. The stream's opening snapshot was built from the
/// deployment account, so every subscriber's first frame was somebody else's roster; and every
/// stamped emit went out with `audience: None`, so roster deltas AND transcript frames — message
/// content included — reached every open stream. The only thing standing in front of it was the
/// client declining to render an agent it did not recognise, which is obscurity rather than a
/// check, and it stopped being even that when a coworker could be shared.
///
/// The old comment said scoping meant per-person SEQUENCES and therefore a change to the replica
/// contract. It meant per-person COUNTERS: the wire `replicaKey` is still `"roster"`, the split
/// lives in the server's map, and no account can see a gap because no account receives the frames
/// that would have skipped its numbers.
#[tokio::test]
async fn a_stream_sees_only_its_own_account() {
    let url = database_or_skip!();
    let tag = uuid::Uuid::now_v7().simple().to_string();
    let h = strict_harness(&url, &format!("deployment-{tag}@og.local")).await;

    let a_email = format!("a-{tag}@og.local");
    let b_email = format!("b-{tag}@og.local");
    let a = seed_account(&h.store, &a_email).await;
    let b = seed_account(&h.store, &b_email).await;
    let a_token = h.access_token(&a, &a_email);
    let b_token = h.access_token(&b, &b_email);

    // A hires; B hires nothing.
    let (status, made) = h
        .api_as(
            "createAgent",
            json!({ "name": "AlphasBot", "clientNonce": format!("a-{tag}") }),
            &a_token,
        )
        .await;
    assert_eq!(status, 200, "{made}");

    // B opens a stream. Its OPENING SNAPSHOT must be B's roster — empty — not A's or the
    // deployment's. Before the fix this frame carried whatever OG_GATEWAY_EMAIL could see.
    let mut b_stream = h
        .client
        .get(format!("{}/events?channels=agents", h.base))
        .header("authorization", "Bearer test-bearer")
        .header("x-opengrok-account", &b_token)
        .header("accept", "text/event-stream")
        .send()
        .await
        .expect("b stream");
    assert_eq!(b_stream.status().as_u16(), 200);

    let mut buffer = String::new();
    let opening = next_frame(&mut b_stream, &mut buffer, 5)
        .await
        .expect("an opening snapshot");
    assert_eq!(opening["channel"], "agents");
    let names: Vec<&str> = opening["payload"]["agents"]
        .as_array()
        .expect("agents")
        .iter()
        .filter_map(|row| row["name"].as_str())
        .collect();
    assert!(
        names.is_empty(),
        "B's stream opened on somebody else's roster: {names:?}"
    );

    // Now A changes its roster. B must hear NOTHING — not the delta, not a snapshot.
    let (status, second) = h
        .api_as(
            "createAgent",
            json!({ "name": "AlphasSecond", "clientNonce": format!("a2-{tag}") }),
            &a_token,
        )
        .await;
    assert_eq!(status, 200, "{second}");

    let leaked = next_frame(&mut b_stream, &mut buffer, 3).await;
    assert!(
        leaked.is_none(),
        "A's roster change reached B's stream: {leaked:?}"
    );
}
