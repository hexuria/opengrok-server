//! Skills over HTTP: one person's named bundle, versioned, capped, and nobody else's.
//!
//! The routes refuse by `skills::may`; this file drives the routes rather than the store, because
//! the refusals ARE the feature — a store call that works for the wrong account is only a bug once
//! a route lets that account make it.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use base64::Engine as _;
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

async fn seed_account(store: &PgStore, email: &str, org: Option<&str>) -> AccountId {
    let id = AccountId::new();
    let at_ms = chrono::Utc::now().timestamp_millis();
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.to_string(),
            password_hash: "x".to_string(),
            first_name: "Skill".to_string(),
            last_name: String::new(),
            org_id: org.unwrap_or_default().to_string(),
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
        first_name: "Skill".to_string(),
        last_name: String::new(),
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

struct Harness {
    base: String,
    agui: AgUiState,
    store: PgStore,
    client: reqwest::Client,
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
        Arc::new(TokenMinter::new(b"skills-secret")),
        "host@og.local".to_string(),
    );
    let agui = AgUiState {
        auth,
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
    let gateway = HostState::new(agui.clone(), None);
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
    }
}

impl Harness {
    async fn person(&self, org: Option<&str>) -> String {
        let email = format!("skills-{}@og.local", uuid::Uuid::now_v7().simple());
        let account = seed_account(&self.store, &email, org).await;
        self.agui
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
            .expect("mint access")
    }

    async fn call(
        &self,
        token: &str,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Value, String) {
        let url = format!("{}{path}", self.base);
        let request = match method {
            "GET" => self.client.get(url),
            "POST" => self.client.post(url),
            "PUT" => self.client.put(url),
            "DELETE" => self.client.delete(url),
            _ => panic!("no such method in this harness: {method}"),
        }
        .header("authorization", format!("Bearer {token}"));
        let request = match body {
            Some(body) => request.json(&body),
            None => request,
        };
        let response = request.send().await.expect("send");
        let status = response.status().as_u16();
        let text = response.text().await.expect("text");
        let value = serde_json::from_str(&text).unwrap_or(Value::Null);
        (status, value, text)
    }
}

/// A unique name per run: skill names are unique per account, and the accounts here are fresh but
/// the assertions read better when the name is stable within one test.
fn a_name(stem: &str) -> String {
    format!("{stem}-{}", uuid::Uuid::now_v7().simple())
}

fn ids(list: &Value) -> Vec<String> {
    list.as_array()
        .expect("a list")
        .iter()
        .filter_map(|row| row["id"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test]
async fn a_skill_is_written_read_back_and_listed_only_for_its_owner() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person(None).await;
    let bob = h.person(None).await;
    let name = a_name("inbox-triage");

    let (status, list, text) = h.call(&ada, "GET", "/skills?filter=mine", None).await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(list, json!([]), "a new account owns no skills");

    let (status, made, text) = h
        .call(
            &ada,
            "POST",
            "/skills",
            Some(json!({
                "name": name,
                "description": "sort the morning mail",
                "body": "Read the inbox top down. Answer what takes a line.",
            })),
        )
        .await;
    assert_eq!(status, 200, "{text}");
    let id = made["id"].as_str().expect("id").to_string();
    assert_eq!(made["name"], name.as_str());
    assert_eq!(made["description"], "sort the morning mail");
    assert_eq!(made["source"], "authored", "no files came with it: {made}");
    assert_eq!(made["version"], 1);
    assert_eq!(made["versionCount"], 1);
    assert_eq!(made["draft"], false, "it has a body, so it is not a draft");
    assert_eq!(made["enabled"], true);
    assert_eq!(made["files"], json!([]));
    assert!(
        made["body"].as_str().expect("body").starts_with("Read the"),
        "{made}"
    );

    let (status, read, text) = h.call(&ada, "GET", &format!("/skills/{id}"), None).await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(read["body"], made["body"], "what went in comes back");

    // ---- the listing is per owner ----
    let (_, mine, _) = h.call(&ada, "GET", "/skills?filter=mine", None).await;
    assert_eq!(ids(&mine), vec![id.clone()]);
    let (_, theirs, text) = h.call(&bob, "GET", "/skills?filter=mine", None).await;
    assert_eq!(theirs, json!([]), "another account owns none of it: {text}");

    // ---- a draft is a row with no body yet ----
    let (status, draft, text) = h
        .call(
            &ada,
            "POST",
            "/skills",
            Some(json!({ "name": a_name("not-written-yet") })),
        )
        .await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(draft["draft"], true, "no body yet: {draft}");
    assert_eq!(draft["versionCount"], 0);
    assert_eq!(draft["version"], 0);
    assert_eq!(draft["body"], "");

    // ---- one `/name` means one skill ----
    let (status, _, text) = h
        .call(
            &ada,
            "POST",
            "/skills",
            Some(json!({ "name": name, "body": "a second one" })),
        )
        .await;
    assert_eq!(status, 409, "the name is taken: {text}");
    assert!(text.contains("has to mean one thing"), "{text}");

    // ---- a name has to be sayable after a slash ----
    let (status, _, text) = h
        .call(
            &ada,
            "POST",
            "/skills",
            Some(json!({ "name": "Not A Slash Name", "body": "x" })),
        )
        .await;
    assert_eq!(status, 400, "{text}");
}

#[tokio::test]
async fn another_persons_skill_cannot_be_read_changed_or_deleted() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person(None).await;
    let bob = h.person(None).await;

    let (status, made, text) = h
        .call(
            &ada,
            "POST",
            "/skills",
            Some(json!({ "name": a_name("private-notes"), "body": "mine alone" })),
        )
        .await;
    assert_eq!(status, 200, "{text}");
    let id = made["id"].as_str().expect("id").to_string();

    // 404 rather than 403: telling a stranger the id exists is already an answer about
    // somebody else's account.
    let (status, _, text) = h.call(&bob, "GET", &format!("/skills/{id}"), None).await;
    assert_eq!(status, 404, "{text}");
    assert!(!text.contains("mine alone"), "no body leaked: {text}");

    let (status, _, text) = h
        .call(
            &bob,
            "PUT",
            &format!("/skills/{id}"),
            Some(json!({ "name": a_name("stolen") })),
        )
        .await;
    assert_eq!(status, 404, "{text}");

    let (status, _, text) = h
        .call(
            &bob,
            "POST",
            &format!("/skills/{id}/versions"),
            Some(json!({ "body": "a body that is not theirs to write" })),
        )
        .await;
    assert_eq!(status, 404, "{text}");

    let (status, _, text) = h.call(&bob, "DELETE", &format!("/skills/{id}"), None).await;
    assert_eq!(status, 404, "{text}");

    // And it is all still there, untouched.
    let (status, still, text) = h.call(&ada, "GET", &format!("/skills/{id}"), None).await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(still["body"], "mine alone");
    assert_eq!(still["version"], 1, "no version was written by a stranger");
}

/// A colleague reads, and only reads. The org is the only thing that makes a skill visible to
/// somebody who does not own it, so `filter=org` and `filter=shared` both stand on it.
#[tokio::test]
async fn a_colleague_reads_an_org_skill_and_may_not_change_it() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let org = format!("org_{}", uuid::Uuid::now_v7().simple());
    let ada = h.person(Some(&org)).await;
    let bob = h.person(Some(&org)).await;
    let stranger = h.person(None).await;

    let (status, made, text) = h
        .call(
            &ada,
            "POST",
            "/skills",
            Some(json!({ "name": a_name("house-style"), "body": "Short sentences." })),
        )
        .await;
    assert_eq!(status, 200, "{text}");
    let id = made["id"].as_str().expect("id").to_string();

    let (status, read, text) = h.call(&bob, "GET", &format!("/skills/{id}"), None).await;
    assert_eq!(status, 200, "a colleague reads it: {text}");
    assert_eq!(read["body"], "Short sentences.");

    let (_, org_list, text) = h.call(&bob, "GET", "/skills?filter=org", None).await;
    assert_eq!(ids(&org_list), vec![id.clone()], "{text}");
    let (_, shared, _) = h.call(&bob, "GET", "/skills?filter=shared", None).await;
    assert_eq!(ids(&shared), vec![id.clone()], "shared stands on the org");
    let (_, bobs_own, _) = h.call(&bob, "GET", "/skills?filter=mine", None).await;
    assert_eq!(bobs_own, json!([]), "reading it does not make it theirs");

    // 403, not 404: a colleague already knows it exists, so the honest answer is why not.
    let (status, _, text) = h
        .call(
            &bob,
            "PUT",
            &format!("/skills/{id}"),
            Some(json!({ "description": "rewritten by somebody else" })),
        )
        .await;
    assert_eq!(status, 403, "{text}");
    assert!(text.contains("only the skill's owner"), "{text}");
    let (status, _, text) = h.call(&bob, "DELETE", &format!("/skills/{id}"), None).await;
    assert_eq!(status, 403, "{text}");

    let (status, _, text) = h
        .call(&stranger, "GET", &format!("/skills/{id}"), None)
        .await;
    assert_eq!(status, 404, "another org is not a colleague: {text}");

    // Switched off, it leaves the org's list and stops reading — the owner's switch, enforced here.
    let (status, _, text) = h
        .call(
            &ada,
            "PUT",
            &format!("/skills/{id}"),
            Some(json!({ "enabled": false })),
        )
        .await;
    assert_eq!(status, 200, "{text}");
    let (status, _, text) = h.call(&bob, "GET", &format!("/skills/{id}"), None).await;
    assert_eq!(status, 404, "a disabled skill is not the org's: {text}");
    let (_, org_list, _) = h.call(&bob, "GET", "/skills?filter=org", None).await;
    assert_eq!(org_list, json!([]));
    let (_, still_mine, _) = h.call(&ada, "GET", "/skills?filter=mine", None).await;
    assert_eq!(ids(&still_mine), vec![id], "the owner still has it");
}

#[tokio::test]
async fn a_body_over_the_cap_is_refused_with_the_limit_and_the_size() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person(None).await;
    let over = "x".repeat(8_001);

    let (status, _, text) = h
        .call(
            &ada,
            "POST",
            "/skills",
            Some(json!({ "name": a_name("too-long"), "body": over })),
        )
        .await;
    assert_eq!(status, 413, "{text}");
    assert!(text.contains("8000"), "the limit is named: {text}");
    assert!(text.contains("8001"), "the size is named: {text}");

    // And the row did not appear anyway.
    let (_, mine, _) = h.call(&ada, "GET", "/skills?filter=mine", None).await;
    assert_eq!(mine, json!([]), "a refused create writes nothing");

    // The same cap on a new version of a skill that already exists.
    let (status, made, text) = h
        .call(
            &ada,
            "POST",
            "/skills",
            Some(json!({ "name": a_name("short-enough"), "body": "fine" })),
        )
        .await;
    assert_eq!(status, 200, "{text}");
    let id = made["id"].as_str().expect("id").to_string();
    let (status, _, text) = h
        .call(
            &ada,
            "POST",
            &format!("/skills/{id}/versions"),
            Some(json!({ "body": "y".repeat(9_000) })),
        )
        .await;
    assert_eq!(status, 413, "{text}");
    assert!(text.contains("8000") && text.contains("9000"), "{text}");
    let (_, read, _) = h.call(&ada, "GET", &format!("/skills/{id}"), None).await;
    assert_eq!(read["version"], 1, "the refused version was not written");
    assert_eq!(read["body"], "fine");
}

#[tokio::test]
async fn an_uploaded_bundle_round_trips_with_its_supporting_file() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person(None).await;
    let name = a_name("release-notes");

    // A byte sequence that does not survive a trip through a String, so the base64 leg is real.
    let sheet: Vec<u8> = vec![0xC3, 0x28, 0x00, 0x1A, 0xFF, b'h', b'i'];
    let encoded = base64::engine::general_purpose::STANDARD.encode(&sheet);
    let uploaded = format!(
        "---\nname: {name}\ndescription: what to write when we ship\n---\nOpen the changelog.\n"
    );

    let (status, made, text) = h
        .call(
            &ada,
            "POST",
            "/skills",
            Some(json!({
                "body": uploaded,
                "source": "uploaded",
                "files": [{ "path": "reference/checklist.bin", "bytes": encoded }],
            })),
        )
        .await;
    assert_eq!(status, 200, "{text}");
    let id = made["id"].as_str().expect("id").to_string();
    assert_eq!(
        made["name"],
        name.as_str(),
        "the name came out of the frontmatter: {made}"
    );
    assert_eq!(made["description"], "what to write when we ship");
    assert_eq!(made["source"], "uploaded");
    assert_eq!(
        made["body"], "Open the changelog.",
        "the frontmatter is stripped, not stored: {made}"
    );

    let (status, read, text) = h.call(&ada, "GET", &format!("/skills/{id}"), None).await;
    assert_eq!(status, 200, "{text}");
    let files = read["files"].as_array().expect("files");
    assert_eq!(files.len(), 1, "{read}");
    assert_eq!(files[0]["path"], "reference/checklist.bin");
    let back = base64::engine::general_purpose::STANDARD
        .decode(files[0]["bytes"].as_str().expect("bytes"))
        .expect("decode");
    assert_eq!(back, sheet, "the bytes survive the round trip");

    // A path that climbs out is refused rather than cleaned up: these files land on a computer.
    // 400, not 413 — "payload too large" would tell the caller to send less of a path it must
    // never send at all.
    let (status, _, text) = h
        .call(
            &ada,
            "POST",
            &format!("/skills/{id}/versions"),
            Some(json!({
                "body": "still fine",
                "files": [{ "path": "../../.ssh/authorized_keys", "bytes": encoded }],
            })),
        )
        .await;
    assert_eq!(status, 400, "{text}");
    assert!(text.contains("climbs out"), "{text}");

    // A home-directory path and a trailing slash past the SKILL.md guard, through the route.
    for path in ["~/.ssh/authorized_keys", "SKILL.md/", "a//b", "-rf"] {
        let (status, _, text) = h
            .call(
                &ada,
                "POST",
                &format!("/skills/{id}/versions"),
                Some(json!({
                    "body": "still fine",
                    "files": [{ "path": path, "bytes": encoded }],
                })),
            )
            .await;
        assert_eq!(status, 400, "{path} should be refused: {text}");
    }

    // The same file twice is a bundle that means two different things on a case-folding disk.
    let (status, _, text) = h
        .call(
            &ada,
            "POST",
            &format!("/skills/{id}/versions"),
            Some(json!({
                "body": "still fine",
                "files": [
                    { "path": "Notes.md", "bytes": encoded },
                    { "path": "notes.md", "bytes": encoded },
                ],
            })),
        )
        .await;
    assert_eq!(status, 400, "{text}");
    assert!(text.contains("twice"), "{text}");

    // One file past the count cap.
    let many: Vec<_> = (0..=32)
        .map(|n| json!({ "path": format!("f{n}.md"), "bytes": encoded }))
        .collect();
    let (status, _, text) = h
        .call(
            &ada,
            "POST",
            &format!("/skills/{id}/versions"),
            Some(json!({ "body": "still fine", "files": many })),
        )
        .await;
    assert_eq!(status, 413, "{text}");
    assert!(text.contains("32"), "the cap is named: {text}");

    // A bundle past the cap is refused, and the refusal names the cap.
    let fat = base64::engine::general_purpose::STANDARD.encode(vec![0u8; 300 * 1024]);
    let (status, _, text) = h
        .call(
            &ada,
            "POST",
            &format!("/skills/{id}/versions"),
            Some(json!({ "body": "still fine", "files": [{ "path": "big.bin", "bytes": fat }] })),
        )
        .await;
    assert_eq!(status, 413, "{text}");
    assert!(text.contains(&(256 * 1024).to_string()), "{text}");
}

#[tokio::test]
async fn a_new_version_increments_and_the_detail_returns_the_newest() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person(None).await;

    let (status, made, text) = h
        .call(
            &ada,
            "POST",
            "/skills",
            Some(json!({ "name": a_name("standup"), "body": "first words" })),
        )
        .await;
    assert_eq!(status, 200, "{text}");
    let id = made["id"].as_str().expect("id").to_string();

    let (status, second, text) = h
        .call(
            &ada,
            "POST",
            &format!("/skills/{id}/versions"),
            Some(json!({ "body": "second words", "note": "shorter" })),
        )
        .await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(second["version"], 2, "{second}");
    assert_eq!(second["kind"], "authored");
    assert_eq!(second["note"], "shorter");
    assert!(second["createdAtMs"].as_i64().unwrap_or(0) > 0, "{second}");

    let (status, third, text) = h
        .call(
            &ada,
            "POST",
            &format!("/skills/{id}/versions"),
            Some(json!({ "body": "third words" })),
        )
        .await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(third["version"], 3);

    let (_, read, text) = h.call(&ada, "GET", &format!("/skills/{id}"), None).await;
    assert_eq!(read["version"], 3, "{text}");
    assert_eq!(read["body"], "third words", "the newest body is the one");
    assert_eq!(read["versionCount"], 3);
    assert_eq!(read["draft"], false);

    // A client cannot claim a body was taught: that word is the server's, for a turn's own writing.
    let (status, _, text) = h
        .call(
            &ada,
            "POST",
            &format!("/skills/{id}/versions"),
            Some(json!({ "body": "fourth words", "kind": "taught" })),
        )
        .await;
    assert_eq!(status, 400, "{text}");
    assert!(text.contains("written by the server"), "{text}");

    // Files belong to the version that brought them, so a body written without any has none.
    let (_, read, _) = h.call(&ada, "GET", &format!("/skills/{id}"), None).await;
    assert_eq!(read["files"], json!([]));
}

#[tokio::test]
async fn a_deleted_skill_leaves_the_list_and_stops_taking_writes() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person(None).await;
    let name = a_name("retired");

    let (status, made, text) = h
        .call(
            &ada,
            "POST",
            "/skills",
            Some(json!({ "name": name, "body": "for a while" })),
        )
        .await;
    assert_eq!(status, 200, "{text}");
    let id = made["id"].as_str().expect("id").to_string();

    let (status, _, text) = h.call(&ada, "DELETE", &format!("/skills/{id}"), None).await;
    assert_eq!(status, 204, "{text}");

    let (_, mine, text) = h.call(&ada, "GET", "/skills?filter=mine", None).await;
    assert_eq!(mine, json!([]), "it is out of the list: {text}");

    // Soft, not gone: the row is still readable by its owner, so a run that cited this skill can
    // still say what it cited.
    let (status, read, text) = h.call(&ada, "GET", &format!("/skills/{id}"), None).await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(read["body"], "for a while");
    assert!(
        h.store
            .skill(&id)
            .await
            .expect("read")
            .expect("the row is still there")
            .deleted_at_ms
            .is_some(),
        "the delete is a timestamp, not a DELETE"
    );

    // And it takes no more writes.
    let (status, _, text) = h
        .call(
            &ada,
            "POST",
            &format!("/skills/{id}/versions"),
            Some(json!({ "body": "one more" })),
        )
        .await;
    assert_eq!(status, 410, "{text}");
    let (status, _, text) = h
        .call(
            &ada,
            "PUT",
            &format!("/skills/{id}"),
            Some(json!({ "description": "back from the dead" })),
        )
        .await;
    assert_eq!(status, 410, "{text}");

    // The name it held is free again — the unique index is partial for exactly this.
    let (status, again, text) = h
        .call(
            &ada,
            "POST",
            "/skills",
            Some(json!({ "name": name, "body": "a fresh start" })),
        )
        .await;
    assert_eq!(status, 200, "{text}");
    assert_ne!(again["id"], made["id"], "a new row, not a resurrection");
}

/// TWO PEOPLE IN NO ORG ARE NOT COLLEAGUES.
///
/// `Register` carries `org_id` as a plain `String`, so an account made without one used to replay
/// to `Some("")` — and `Some("") == Some("")`, so `relation_to` made every orgless person an
/// `OrgMember` of every other orgless person's skills. Both accounts here are deliberately in NO
/// org, which is the condition the bug needed; the earlier tests pass only because 404 happens to
/// be the answer either way for a skill they never look at twice.
#[tokio::test]
async fn two_people_in_no_org_are_not_colleagues() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person(None).await;
    let bob = h.person(None).await;

    let (status, made, text) = h
        .call(
            &ada,
            "POST",
            "/skills",
            Some(json!({ "name": a_name("orgless"), "body": "nobody else's business" })),
        )
        .await;
    assert_eq!(status, 200, "{text}");
    let id = made["id"].as_str().expect("id").to_string();

    let (status, _, text) = h.call(&bob, "GET", &format!("/skills/{id}"), None).await;
    assert_eq!(status, 404, "no org is not a shared org: {text}");
    assert!(!text.contains("nobody else's business"), "{text}");

    // And it is in none of their listings, under any filter.
    for filter in ["org", "shared", "mine"] {
        let (status, list, text) = h
            .call(&bob, "GET", &format!("/skills?filter={filter}"), None)
            .await;
        assert_eq!(status, 200, "{text}");
        assert_eq!(list, json!([]), "filter={filter} leaked a stranger's skill");
    }
    let (_, everything, text) = h.call(&bob, "GET", "/skills", None).await;
    assert_eq!(
        everything,
        json!([]),
        "no filter is not every skill: {text}"
    );
}

/// The gaps between the relations: a colleague may read but not write, cannot see a deleted one,
/// and the owner's own switch never hides a skill from the owner.
#[tokio::test]
async fn a_colleague_may_add_no_version_and_sees_no_deleted_skill() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let org = format!("org_{}", uuid::Uuid::now_v7().simple());
    let ada = h.person(Some(&org)).await;
    let bob = h.person(Some(&org)).await;

    let (status, made, text) = h
        .call(
            &ada,
            "POST",
            "/skills",
            Some(json!({ "name": a_name("shared-style"), "body": "ours" })),
        )
        .await;
    assert_eq!(status, 200, "{text}");
    let id = made["id"].as_str().expect("id").to_string();

    // Readable, and 403 rather than 404 on a write: a colleague already knows it is there.
    let (status, _, text) = h
        .call(
            &bob,
            "POST",
            &format!("/skills/{id}/versions"),
            Some(json!({ "body": "a body that is not theirs to write" })),
        )
        .await;
    assert_eq!(status, 403, "{text}");
    assert!(text.contains("only the skill's owner"), "{text}");
    let (_, read, _) = h.call(&ada, "GET", &format!("/skills/{id}"), None).await;
    assert_eq!(read["version"], 1, "nothing was written");

    // Whose skill it is, on every row: a colleague's `review` lists beside your own.
    let (_, org_list, text) = h.call(&bob, "GET", "/skills?filter=org", None).await;
    let rows = org_list.as_array().expect("a list");
    assert_eq!(rows.len(), 1, "{text}");
    assert!(
        rows[0]["ownerId"]
            .as_str()
            .is_some_and(|id| id.starts_with("acct_")),
        "a listing says whose each row is: {text}"
    );

    // The owner's own switch does not hide the skill from the owner.
    let (status, off, text) = h
        .call(
            &ada,
            "PUT",
            &format!("/skills/{id}"),
            Some(json!({ "enabled": false })),
        )
        .await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(off["enabled"], false);
    let (status, mine, text) = h.call(&ada, "GET", &format!("/skills/{id}"), None).await;
    assert_eq!(status, 200, "switching it off is not hiding it: {text}");
    assert_eq!(mine["body"], "ours");
    let (_, my_list, _) = h.call(&ada, "GET", "/skills?filter=mine", None).await;
    assert_eq!(ids(&my_list), vec![id.clone()], "still in the owner's list");

    // Deleted, it is gone for the colleague entirely — not 403, which would confirm it existed.
    let (_, _, _) = h
        .call(
            &ada,
            "PUT",
            &format!("/skills/{id}"),
            Some(json!({ "enabled": true })),
        )
        .await;
    let (status, _, text) = h.call(&ada, "DELETE", &format!("/skills/{id}"), None).await;
    assert_eq!(status, 204, "{text}");
    let (status, _, text) = h.call(&bob, "GET", &format!("/skills/{id}"), None).await;
    assert_eq!(
        status, 404,
        "a deleted skill is nobody's but its owner's: {text}"
    );
    let (_, org_list, _) = h.call(&bob, "GET", "/skills?filter=org", None).await;
    assert_eq!(org_list, json!([]));
}

/// A mistyped fence and a Windows line ending are the two ways a real SKILL.md arrives broken.
/// Neither may be answered with a 200.
#[tokio::test]
async fn an_uploaded_skill_md_survives_crlf_and_refuses_an_unclosed_fence() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person(None).await;
    let name = a_name("windows-written");

    // CRLF: the offsets used to be rebuilt a byte short per line, so the body began inside the
    // closing fence and the error grew with every frontmatter line.
    let uploaded = format!(
        "---\r\nname: {name}\r\nversion: 2\r\nauthor: someone\r\n\
         description: written on Windows\r\n---\r\nOpen the changelog.\r\n"
    );
    let (status, made, text) = h
        .call(&ada, "POST", "/skills", Some(json!({ "body": uploaded })))
        .await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(made["name"], name.as_str());
    assert_eq!(made["description"], "written on Windows");
    assert_eq!(
        made["body"], "Open the changelog.",
        "no fence and no frontmatter leaked into the body: {made}"
    );

    // An unclosed fence used to answer 200 with an empty body and `draft: true` — a real skill
    // uploaded, named, described, and silently thrown away.
    let broken = "---\nname: half-written\ndescription: d\nThe instructions, with no fence.\n";
    let (status, _, text) = h
        .call(
            &ada,
            "POST",
            "/skills",
            Some(json!({ "name": a_name("half"), "body": broken })),
        )
        .await;
    assert_eq!(status, 400, "{text}");
    assert!(text.contains("closing `---`"), "it says what to do: {text}");

    // And nothing was written, so the name is not squatted by a failed upload.
    let (_, mine, _) = h.call(&ada, "GET", "/skills?filter=mine", None).await;
    assert_eq!(ids(&mine).len(), 1, "only the good one landed");

    // The same refusal on a new version of a skill that already exists.
    let id = made["id"].as_str().expect("id");
    let (status, _, text) = h
        .call(
            &ada,
            "POST",
            &format!("/skills/{id}/versions"),
            Some(json!({ "body": broken })),
        )
        .await;
    assert_eq!(status, 400, "{text}");
}

/// Two replies that are easy to get wrong because they look like success: a filter nobody
/// implements, and a caller with no token at all.
#[tokio::test]
async fn an_unknown_filter_is_refused_and_no_token_is_a_401() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person(None).await;

    let (status, _, text) = h.call(&ada, "GET", "/skills?filter=everything", None).await;
    assert_eq!(status, 400, "a typo is not an empty account: {text}");
    assert!(
        text.contains("mine"),
        "the refusal names the real ones: {text}"
    );

    // Absent and blank both mean "everything visible", which is not the same as unknown.
    for path in ["/skills", "/skills?filter="] {
        let (status, list, text) = h.call(&ada, "GET", path, None).await;
        assert_eq!(status, 200, "{path}: {text}");
        assert_eq!(list, json!([]));
    }

    // Every other test in this file carries a bearer, so nothing here proved the door was shut.
    let no_token = h.client.get(format!("{}/skills", h.base));
    let response = no_token.send().await.expect("send");
    assert_eq!(response.status().as_u16(), 401, "no token, no skills");

    let (status, made, _) = h
        .call(
            &ada,
            "POST",
            "/skills",
            Some(json!({ "name": a_name("guarded"), "body": "mine" })),
        )
        .await;
    assert_eq!(status, 200);
    let id = made["id"].as_str().expect("id");
    for (method, path) in [
        ("GET", format!("/skills/{id}")),
        ("PUT", format!("/skills/{id}")),
        ("DELETE", format!("/skills/{id}")),
        ("POST", format!("/skills/{id}/versions")),
    ] {
        let url = format!("{}{path}", h.base);
        let request = match method {
            "GET" => h.client.get(url),
            "PUT" => h.client.put(url).json(&json!({ "name": "x" })),
            "DELETE" => h.client.delete(url),
            _ => h.client.post(url).json(&json!({ "body": "x" })),
        };
        let status = request.send().await.expect("send").status().as_u16();
        assert_eq!(status, 401, "{method} {path} answered an unsigned caller");
    }
}
