//! `PATCH /coworkers/{id}` honours the WHOLE patch the app sends.
//!
//! The app's Save button puts the card in one body — name, title and role together — and this
//! route used to read four keys out of it. A rename was accepted, answered 200 with the old name
//! beside a role that had been stored, and the coworker went on introducing itself as "New Bot".
//! So these drive the real HTTP surface and then read the two homes back: the aggregate through
//! the roster, the decoration out of the seam-B profile blob. Needs Postgres; skips loudly
//! without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::password::hash_password;
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
    store: PgStore,
    account: AccountId,
    minter: Arc<TokenMinter>,
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
    let minter = Arc::new(TokenMinter::new(b"rename-test-secret"));
    let auth = AuthState::new(store.clone(), minter.clone(), email.to_string());
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
        account,
        minter,
    }
}

impl Harness {
    fn access(&self, email: &str) -> String {
        self.minter
            .mint_access(
                self.account.as_str(),
                "sess-rename",
                email,
                "ultra",
                chrono::Utc::now().timestamp(),
                3600,
            )
            .expect("mint access")
    }

    async fn hire(&self, access: &str, name: &str) -> String {
        let res = self
            .client
            .post(format!("{}/coworkers", self.base))
            .header("Authorization", format!("Bearer {access}"))
            .json(&json!({ "name": name }))
            .send()
            .await
            .expect("hire");
        assert_eq!(res.status().as_u16(), 201, "hire {name}");
        let body: Value = res.json().await.expect("hire body");
        body["id"].as_str().expect("id").to_string()
    }

    async fn patch(&self, access: &str, id: &str, body: Value) -> (u16, Value) {
        let res = self
            .client
            .patch(format!("{}/coworkers/{id}", self.base))
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

    /// The roster row the app lists — the aggregate's answer, not the reply's.
    async fn row(&self, access: &str, id: &str) -> Value {
        let res = self
            .client
            .get(format!("{}/coworkers", self.base))
            .header("Authorization", format!("Bearer {access}"))
            .send()
            .await
            .expect("list");
        assert_eq!(res.status().as_u16(), 200);
        let listed: Value = res.json().await.expect("list body");
        listed
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["id"] == id).cloned())
            .unwrap_or(Value::Null)
    }

    /// The seam-B profile blob — where the title, the shape and the colour actually live.
    async fn profile(&self, id: &str) -> Value {
        self.store
            .seamb_profile(&CoworkerId::from_stored(id.to_string()))
            .await
            .expect("read profile")
            .unwrap_or(Value::Null)
    }
}

#[tokio::test]
async fn a_name_only_patch_renames_the_coworker() {
    let database_url = database_or_skip!();
    let email = format!("rename-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let access = h.access(&email);
    let agent = h.hire(&access, "New Bot").await;

    let (status, patched) = h
        .patch(&access, &agent, json!({ "name": "Greendale" }))
        .await;
    assert_eq!(status, 200, "a name is a change: {patched}");
    assert_eq!(patched["name"], "Greendale", "the reply says the new name");
    assert_eq!(
        h.row(&access, &agent).await["name"],
        "Greendale",
        "and so does the roster the app reads next"
    );

    // Trimmed, the way a role is: a name typed with a stray space is the same name.
    let (status, patched) = h
        .patch(&access, &agent, json!({ "name": "  Greendale II  " }))
        .await;
    assert_eq!(status, 200, "{patched}");
    assert_eq!(patched["name"], "Greendale II");
}

#[tokio::test]
async fn a_blank_name_is_refused_and_the_stored_name_stands() {
    let database_url = database_or_skip!();
    let email = format!("blank-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let access = h.access(&email);
    let agent = h.hire(&access, "New Bot").await;

    for blank in [json!(""), json!("   ")] {
        let (status, refused) = h.patch(&access, &agent, json!({ "name": blank })).await;
        assert_eq!(status, 400, "{refused}");
        assert_eq!(
            refused["error"], "name: a coworker needs a name to answer to",
            "a sentence, not a shrug: {refused}"
        );
    }
    assert_eq!(
        h.row(&access, &agent).await["name"],
        "New Bot",
        "a refused write stores nothing: a nameless coworker has no identity line to say"
    );
}

#[tokio::test]
async fn a_name_and_a_role_together_apply_both() {
    let database_url = database_or_skip!();
    let email = format!("card-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let access = h.access(&email);
    let agent = h.hire(&access, "New Bot").await;

    // The exact body the app's Save button sends, and the exact case that was broken: the role
    // was stored, the name was dropped, and the 200 echoed the old one.
    let (status, patched) = h
        .patch(
            &access,
            &agent,
            json!({
                "name": "Greendale",
                "title": "the study group's own dean",
                "role": "Keep the changelog honest.",
            }),
        )
        .await;
    assert_eq!(status, 200, "{patched}");
    assert_eq!(patched["name"], "Greendale", "{patched}");
    assert_eq!(patched["role"], "Keep the changelog honest.", "{patched}");
    assert_eq!(patched["title"], "the study group's own dean", "{patched}");

    let row = h.row(&access, &agent).await;
    assert_eq!(row["name"], "Greendale", "{row}");
    assert_eq!(row["role"], "Keep the changelog honest.", "{row}");
    assert_eq!(
        h.profile(&agent).await["title"],
        "the study group's own dean",
        "the title lives in the blob, beside the description the desktop writes"
    );
}

#[tokio::test]
async fn the_decoration_lands_in_the_profile_blob_and_comes_back() {
    let database_url = database_or_skip!();
    let email = format!("title-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let access = h.access(&email);
    let agent = h.hire(&access, "Ada").await;

    let (status, patched) = h
        .patch(
            &access,
            &agent,
            json!({ "title": "  a release engineer  " }),
        )
        .await;
    assert_eq!(status, 200, "a title alone is a change: {patched}");
    assert_eq!(patched["title"], "a release engineer", "{patched}");
    assert_eq!(
        patched["name"], "Ada",
        "the reply is the whole post-patch truth, not only what moved"
    );
    assert_eq!(h.profile(&agent).await["title"], "  a release engineer  ");

    // The avatar is the same blob and the same rule: a key absent leaves what is stored alone.
    let (status, patched) = h
        .patch(
            &access,
            &agent,
            json!({ "avatarShape": "hex", "avatarColor": "amber" }),
        )
        .await;
    assert_eq!(status, 200, "{patched}");
    assert_eq!(patched["avatarShape"], "hex", "{patched}");
    assert_eq!(patched["avatarColor"], "amber", "{patched}");
    assert_eq!(
        patched["title"], "a release engineer",
        "an avatar patch is not a title edit: {patched}"
    );
    let profile = h.profile(&agent).await;
    assert_eq!(profile["avatarShape"], "hex", "{profile}");
    assert_eq!(profile["title"], "  a release engineer  ", "{profile}");

    // An explicit empty string is how the app's Reset clears a field, and blank reads as absent
    // on the way out — a cleared title is a coworker with no title, not one called "".
    let (status, patched) = h
        .patch(
            &access,
            &agent,
            json!({ "avatarShape": "", "avatarColor": "" }),
        )
        .await;
    assert_eq!(status, 200, "{patched}");
    assert!(patched["avatarShape"].is_null(), "{patched}");
    assert!(patched["avatarColor"].is_null(), "{patched}");
}

#[tokio::test]
async fn a_wrong_typed_or_unknown_field_is_refused_like_the_others() {
    let database_url = database_or_skip!();
    let email = format!("typed-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let access = h.access(&email);
    let agent = h.hire(&access, "Ada").await;

    let (status, refused) = h.patch(&access, &agent, json!({ "name": 7 })).await;
    assert_eq!(status, 400, "{refused}");
    assert_eq!(refused["error"], "name: expected a string");
    // Null is a wrong type here rather than "clear it": a role is nullable and a name is not.
    let (status, refused) = h.patch(&access, &agent, json!({ "name": null })).await;
    assert_eq!(status, 400, "{refused}");
    assert_eq!(refused["error"], "name: expected a string");
    let (status, refused) = h.patch(&access, &agent, json!({ "title": 7 })).await;
    assert_eq!(status, 400, "{refused}");
    assert_eq!(refused["error"], "title: expected a string");
    let (status, refused) = h
        .patch(&access, &agent, json!({ "avatarColor": true }))
        .await;
    assert_eq!(status, 400, "{refused}");
    assert_eq!(refused["error"], "avatarColor: expected a string");

    // A key this route does not know is not silently accepted: the body named nothing it can
    // change, and the sentence lists what it could have named.
    let (status, refused) = h.patch(&access, &agent, json!({ "colour": "amber" })).await;
    assert_eq!(status, 400, "{refused}");
    assert!(
        refused["error"]
            .as_str()
            .unwrap_or_default()
            .starts_with("nothing to change: send a name, a model, a role, a title,"),
        "{refused}"
    );

    assert_eq!(
        h.row(&access, &agent).await["name"],
        "Ada",
        "every refusal above stored nothing"
    );
}

/// The roster answers the same resource the PATCH reply does, in the same spelling.
///
/// It used to serialize the core `CoworkerView` straight onto the wire: `box_id`,
/// `updated_at_ms`, `retired`, `members`, and none of the decoration the PATCH had just stored.
/// The app overwrites its row from the PATCH reply, then relaunches onto the roster — and a
/// snake_case key is one it silently reads as absent (#27), so the sort key, the title and the
/// avatar all went missing the first time it restarted.
#[tokio::test]
async fn the_roster_row_speaks_the_same_camelcase_as_the_patch_reply() {
    let database_url = database_or_skip!();
    let email = format!("roster-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let access = h.access(&email);
    let agent = h.hire(&access, "Ada").await;

    let (status, patched) = h
        .patch(
            &access,
            &agent,
            json!({
                "title": "a release engineer",
                "avatarShape": "hex",
                "avatarColor": "amber",
            }),
        )
        .await;
    assert_eq!(status, 200, "{patched}");

    let row = h.row(&access, &agent).await;
    let keys: Vec<&String> = row
        .as_object()
        .expect("a row is an object")
        .keys()
        .collect();
    assert!(
        keys.iter().all(|key| !key.contains('_')),
        "a snake_case key on the roster is one the app reads as absent: {row}"
    );
    assert!(
        row["updatedAtMs"].as_i64().is_some(),
        "the client's sort key: {row}"
    );
    assert!(
        row.as_object().is_some_and(|row| row.contains_key("boxId")),
        "present even when null, so absent and unassigned read the same: {row}"
    );
    for key in [
        "id",
        "name",
        "model",
        "role",
        "title",
        "avatarShape",
        "avatarColor",
        "visibility",
        "hiddenFromSidebar",
        "boxId",
    ] {
        assert_eq!(
            row[key], patched[key],
            "{key}: roster {row} vs reply {patched}"
        );
    }
    assert_eq!(row["title"], "a release engineer", "{row}");
    assert_eq!(row["avatarShape"], "hex", "{row}");
    assert_eq!(row["visibility"], "private", "{row}");
    assert_eq!(row["hiddenFromSidebar"], false, "{row}");
    assert_eq!(row["isGroup"], false, "{row}");
    assert_eq!(row["memberIds"], json!([]), "{row}");

    // A coworker with no profile row still carries the keys, as null: a key that is sometimes
    // missing is a shape the app has to guess about.
    let bare = h.hire(&access, "Bob").await;
    let row = h.row(&access, &bare).await;
    for key in ["title", "avatarShape", "avatarColor"] {
        assert!(
            row.as_object().is_some_and(|row| row.contains_key(key)) && row[key].is_null(),
            "{key} on an undecorated coworker: {row}"
        );
    }
}
