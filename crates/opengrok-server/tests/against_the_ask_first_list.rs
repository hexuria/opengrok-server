//! Auto-review's third list: "Ask first", beside allow and block (#354).
//!
//! Over HTTP, the way NativeChat's Auto-review modal drives it: the list is saved, read back as
//! stored and as resolved, inherits and clears like the other two, shares their length limit, and
//! meets the scope rules they meet. What the judge does with the lists is
//! `against_the_three_lists_on_a_turn.rs`.
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

struct Harness {
    base: String,
    client: reqwest::Client,
    token: String,
}

/// A server, and an account signed in to it.
async fn harness(database_url: &str) -> Harness {
    let email = format!("ask_first_{}@og.local", uuid::Uuid::now_v7().simple());
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let account = seed_account(&store, &email).await;
    let auth = AuthState::new(
        store,
        Arc::new(TokenMinter::new(b"ask_first_secret")),
        email.clone(),
    );
    let agui = AgUiState {
        auth,
        door: Arc::new(MockDoor::echoing()),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/judge".to_string(),
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
    let token = agui
        .auth
        .minter
        .mint_access(
            account.as_str(),
            "sess-ask-first",
            &email,
            "ultra",
            chrono::Utc::now().timestamp(),
            3600,
        )
        .expect("mint access");
    Harness {
        base: format!("http://127.0.0.1:{}", addr.port()),
        client: reqwest::Client::new(),
        token,
    }
}

impl Harness {
    /// One call, signed in; the status and the body as JSON (a plain-text body is a string).
    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base))
            .header("authorization", format!("Bearer {}", self.token));
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

    async fn get(&self, path: &str) -> Value {
        let (status, body) = self.call(reqwest::Method::GET, path, None).await;
        assert_eq!(status, 200, "GET {path}: {body}");
        body
    }

    async fn put(&self, body: Value) -> u16 {
        self.call(reqwest::Method::PUT, "/auto-review/policy", Some(body))
            .await
            .0
    }

    async fn hire(&self, name: &str) -> String {
        let hired = json!({ "name": name });
        let (status, hired) = self
            .call(reqwest::Method::POST, "/coworkers", Some(hired))
            .await;
        assert_eq!(status, 201, "hire {name}: {hired}");
        hired["id"].as_str().expect("coworker id").to_string()
    }
}

/// A row as `GET /auto-review/policy` shows it, with its clock taken off to compare the rest.
fn without_clock(mut row: Value) -> Value {
    let clock = row
        .as_object_mut()
        .and_then(|row| row.remove("updatedAtMs"));
    assert!(clock.is_some_and(|clock| clock.is_number()), "{row}");
    row
}

/// THE CASE NATIVECHAT'S MODAL IS BUILT FROM, pinned in the wire corpus (`ALSO_KEEP`): a global
/// row and a Bot's own with an ask-first list each, read back as stored, and the resolved view.
/// One call of each kind on each route, so the recording is this one.
#[tokio::test]
async fn a_person_writes_an_ask_first_list_and_reads_it_back_stored_and_resolved() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let bot = h.hire("Ada").await;

    // The text is trimmed on the way in, and the other two lists are left to inherit or to say.
    let saved = h
        .put(json!({
            "scopeKind": "global", "scopeId": "", "enabled": true,
            "allowInstructions": "git is fine",
            "askInstructions": "  check with me before sending email  ",
            "blockInstructions": "never delete backups",
        }))
        .await;
    assert_eq!(saved, 204);
    let saved = h
        .put(json!({
            "scopeKind": "coworker", "scopeId": bot,
            "enabled": null, "allowInstructions": null,
            "askInstructions": "check with me before any purchase",
            "blockInstructions": null,
        }))
        .await;
    assert_eq!(saved, 204);

    let stored = h.get("/auto-review/policy").await;
    assert_eq!(
        without_clock(stored["global"].clone()),
        json!({
            "enabled": true,
            "allowInstructions": "git is fine",
            "askInstructions": "check with me before sending email",
            "blockInstructions": "never delete backups",
        })
    );
    assert_eq!(
        stored["coworkers"].as_object().map(|rows| rows.len()),
        Some(1)
    );
    assert_eq!(
        without_clock(stored["coworkers"][&bot].clone()),
        json!({
            "enabled": null,
            "allowInstructions": null,
            "askInstructions": "check with me before any purchase",
            "blockInstructions": null,
        })
    );

    // Resolved for the Bot: its own ask-first list, and global's everything else.
    let effective = h
        .get(&format!("/auto-review/effective?coworkerId={bot}"))
        .await;
    assert_eq!(
        effective,
        json!({
            "enabled": true,
            "allowInstructions": "git is fine",
            "askInstructions": "check with me before any purchase",
            "blockInstructions": "never delete backups",
            "decidedBy": {
                "enabled": "global",
                "allowInstructions": "global",
                "askInstructions": "coworker",
                "blockInstructions": "global",
            },
        })
    );
}

/// Null inherits, `""` is an explicit "none" that stops inheritance, a delete restores all of it,
/// and a save that does not mention the list (a client from before it) stores null for it, as
/// every PUT stores the whole row.
#[tokio::test]
async fn the_ask_first_list_inherits_clears_and_deletes_like_the_other_two() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let bot = h.hire("Ada").await;
    let effective = || async {
        h.get(&format!("/auto-review/effective?coworkerId={bot}"))
            .await
    };
    let own = |ask: Value| {
        json!({ "scopeKind": "coworker", "scopeId": bot, "enabled": null,
                "allowInstructions": null, "askInstructions": ask, "blockInstructions": null })
    };
    // Before anything is written: no rows, and every list empty and decided by the default.
    let nothing = h.get("/auto-review/policy").await;
    assert_eq!(nothing, json!({ "global": null, "coworkers": {} }));
    let seen = effective().await;
    assert_eq!(seen["askInstructions"], "");
    assert_eq!(seen["decidedBy"]["askInstructions"], "default");
    let global = json!({ "scopeKind": "global", "scopeId": "", "enabled": true,
                         "askInstructions": "check first" });
    assert_eq!(h.put(global).await, 204);

    // No row of its own: global's list, decided by global.
    let seen = effective().await;
    assert_eq!(seen["askInstructions"], "check first");
    assert_eq!(seen["decidedBy"]["askInstructions"], "global");

    // Its own list replaces global's whole, and decides.
    assert_eq!(h.put(own(json!("my own"))).await, 204);
    let seen = effective().await;
    assert_eq!(seen["askInstructions"], "my own");
    assert_eq!(seen["decidedBy"]["askInstructions"], "coworker");

    // Null is "inherit", not "keep what was there".
    assert_eq!(h.put(own(Value::Null)).await, 204);
    let seen = effective().await;
    assert_eq!(seen["askInstructions"], "check first");
    assert_eq!(seen["decidedBy"]["askInstructions"], "global");

    // '' is "none, and stop asking global": it stays '' and global's does not leak back in.
    assert_eq!(h.put(own(json!("   "))).await, 204);
    let seen = effective().await;
    assert_eq!(seen["askInstructions"], "");
    assert_eq!(seen["decidedBy"]["askInstructions"], "coworker");
    let stored = h.get("/auto-review/policy").await;
    assert_eq!(stored["coworkers"][&bot]["askInstructions"], "");

    // A save with no `askInstructions` key at all is a save of null for it.
    let old_client = json!({ "scopeKind": "coworker", "scopeId": bot, "enabled": true,
                             "allowInstructions": "git is fine", "blockInstructions": null });
    assert_eq!(h.put(old_client).await, 204);
    let stored = h.get("/auto-review/policy").await;
    assert_eq!(stored["coworkers"][&bot]["askInstructions"], Value::Null);
    assert_eq!(effective().await["askInstructions"], "check first");

    // Deleting the row gives back everything, the ask-first list included.
    assert_eq!(h.put(own(json!("my own again"))).await, 204);
    let gone = json!({ "scopeKind": "coworker", "scopeId": bot });
    let (status, _) = h
        .call(reqwest::Method::DELETE, "/auto-review/policy", Some(gone))
        .await;
    assert_eq!(status, 204);
    let seen = effective().await;
    assert_eq!(seen["askInstructions"], "check first");
    assert_eq!(seen["decidedBy"]["askInstructions"], "global");
    let stored = h.get("/auto-review/policy").await;
    assert_eq!(stored["coworkers"], json!({}));
    assert_eq!(stored["global"]["askInstructions"], "check first");

    // Nobody's but the caller's: another id is just a scope with no row.
    let elsewhere = h
        .get("/auto-review/effective?coworkerId=cw_someone_else")
        .await;
    assert_eq!(elsewhere["askInstructions"], "check first");
}

/// The three lists are bounded together, as the two were: what the judge reads on every reviewed
/// call is one scope's worth of text, however it is split. A refused save stores nothing.
#[tokio::test]
async fn the_three_lists_share_one_length_limit_and_a_refused_save_stores_nothing() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let bot = h.hire("Ada").await;
    let save = |allow: &str, ask: &str, block: &str| {
        json!({ "scopeKind": "global", "scopeId": "", "enabled": true,
                "allowInstructions": allow, "askInstructions": ask, "blockInstructions": block })
    };

    // Exactly at the limit, split three ways, with padding that is not counted.
    let padded = format!("  {}  ", "b".repeat(7_000));
    assert_eq!(
        h.put(save(&"a".repeat(6_000), &padded, &"c".repeat(7_000)))
            .await,
        204
    );
    // One character over: refused, in words, with the stored row as it was.
    let over = save(&"a".repeat(6_000), &"b".repeat(7_000), &"c".repeat(7_001));
    let (status, said) = h
        .call(reqwest::Method::PUT, "/auto-review/policy", Some(over))
        .await;
    assert_eq!(status, 422);
    assert!(
        said.to_string().contains("too long for one scope"),
        "{said}"
    );
    let stored = h.get("/auto-review/policy").await;
    assert_eq!(stored["global"]["blockInstructions"], "c".repeat(7_000));
    // The ask-first list alone can be refused too, and it counts characters, not bytes.
    assert_eq!(h.put(save("", &"é".repeat(20_000), "")).await, 204);
    assert_eq!(h.put(save("", &"é".repeat(20_001), "")).await, 422);

    // The scope rules are the same whatever the body carries, and refuse before anything is stored.
    let before = h.get("/auto-review/policy").await["global"].clone();
    let body = |kind: &str, id: &str| json!({ "scopeKind": kind, "scopeId": id, "askInstructions": "check first" });
    for (kind, id) in [
        ("machine", "mac_1"),
        ("global", "cw_1"),
        ("coworker", ""),
        ("coworker", "cw_not_mine"),
    ] {
        assert_eq!(h.put(body(kind, id)).await, 422, "{kind} {id}");
    }
    assert_eq!(h.get("/auto-review/policy").await["global"], before);
    assert_eq!(h.put(body("coworker", &bot)).await, 204);

    // Signed out, none of it is reachable (with a body each route would accept signed in).
    for (method, path) in [
        (reqwest::Method::GET, "/auto-review/policy"),
        (reqwest::Method::PUT, "/auto-review/policy"),
        (reqwest::Method::DELETE, "/auto-review/policy"),
        (reqwest::Method::GET, "/auto-review/effective"),
    ] {
        let res = h
            .client
            .request(method.clone(), format!("{}{path}", h.base))
            .json(&json!({ "scopeKind": "global", "scopeId": "" }))
            .send()
            .await
            .expect("request");
        assert_eq!(res.status().as_u16(), 401, "{method} {path}");
    }
}
