//! Saved site logins: the person's own rows, sealed passwords, one reveal door.
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
use opengrok_store::{PasskeyWrite, PgStore, SiteLoginWrite, Vault};
use serde_json::{Value, json};

const KEK: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
const PASSWORD: &str = "SuperSecretPassword!";

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

async fn seed_account(store: &PgStore, email: &str) -> AccountId {
    let id = AccountId::new();
    let at_ms = chrono::Utc::now().timestamp_millis();
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
    agui: AgUiState,
    store: PgStore,
    client: reqwest::Client,
}

async fn harness(database_url: &str, with_vault: bool) -> Harness {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let auth = AuthState::new(
        store.clone(),
        Arc::new(TokenMinter::new(b"site-logins-secret")),
        "host@og.local".to_string(),
    );
    let agui = AgUiState {
        auth,
        door: Arc::new(MockDoor::echoing()),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer: None,
        vault: with_vault.then(|| Arc::new(Vault::from_base64_key(KEK).expect("vault"))),
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
    async fn person(&self, email: &str) -> String {
        let account = seed_account(&self.store, email).await;
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
            "PATCH" => self.client.patch(url),
            "DELETE" => self.client.delete(url),
            _ => unreachable!(),
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

#[tokio::test]
async fn a_saved_login_is_listed_without_its_password_and_revealed_only_to_its_owner() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, true).await;
    let ada = h
        .person(&format!("ada-{}@og.local", uuid::Uuid::now_v7().simple()))
        .await;
    let bob = h
        .person(&format!("bob-{}@og.local", uuid::Uuid::now_v7().simple()))
        .await;

    let (status, list, _) = h.call(&ada, "GET", "/site-logins", None).await;
    assert_eq!(status, 200);
    assert_eq!(list, json!([]));

    let (status, row, text) = h
        .call(
            &ada,
            "POST",
            "/site-logins",
            Some(json!({ "origin": "https://The-Internet.herokuapp.com/login", "username": " tomsmith ", "password": PASSWORD })),
        )
        .await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(
        row["origin"], "the-internet.herokuapp.com",
        "the bare host, lowercase: {row}"
    );
    assert_eq!(row["username"], "tomsmith", "the name is trimmed: {row}");
    assert!(
        !text.contains(PASSWORD),
        "the save reply carries no password: {text}"
    );
    let id = row["id"].as_str().expect("id").to_string();

    let (_, list, text) = h.call(&ada, "GET", "/site-logins", None).await;
    assert_eq!(list.as_array().map(Vec::len), Some(1), "{list}");
    assert!(
        !text.contains(PASSWORD),
        "the list carries no password: {text}"
    );

    // Saving the same site and name again replaces the password, not the row.
    let (status, again, _) = h
        .call(
            &ada,
            "POST",
            "/site-logins",
            Some(json!({ "origin": "the-internet.herokuapp.com", "username": "tomsmith", "password": "changed!" })),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(again["id"], id, "{again}");
    let (status, opened, _) = h
        .call(&ada, "POST", &format!("/site-logins/{id}/reveal"), None)
        .await;
    assert_eq!(status, 200, "{opened}");
    assert_eq!(opened["password"], "changed!");

    // Bob sees nothing of Ada's, cannot open it, cannot delete it.
    let (_, bobs, _) = h.call(&bob, "GET", "/site-logins", None).await;
    assert_eq!(bobs, json!([]));
    let (status, _, _) = h
        .call(&bob, "POST", &format!("/site-logins/{id}/reveal"), None)
        .await;
    assert_eq!(status, 404);
    let (status, _, _) = h
        .call(&bob, "DELETE", &format!("/site-logins/{id}"), None)
        .await;
    assert_eq!(status, 404);

    // Ada deletes it: row and sealed secret both gone.
    let (status, _, _) = h
        .call(&ada, "DELETE", &format!("/site-logins/{id}"), None)
        .await;
    assert_eq!(status, 200);
    let (_, list, _) = h.call(&ada, "GET", "/site-logins", None).await;
    assert_eq!(list, json!([]));
    let sealed: Option<String> = sqlx::query_scalar("select id from secret_store where id like $1")
        .bind(format!("%{id}%"))
        .fetch_optional(h.store.pool())
        .await
        .expect("query");
    assert_eq!(sealed, None, "the sealed password is gone with the row");
}

#[tokio::test]
async fn a_bad_save_is_refused_and_a_server_without_a_vault_says_so() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, true).await;
    let ada = h
        .person(&format!("ada-{}@og.local", uuid::Uuid::now_v7().simple()))
        .await;
    let (status, body, _) = h
        .call(
            &ada,
            "POST",
            "/site-logins",
            Some(json!({ "origin": "x.com", "username": "" })),
        )
        .await;
    assert_eq!(status, 400, "{body}");
    let (status, _, _) = h.call("nope", "GET", "/site-logins", None).await;
    assert_eq!(status, 401);

    // A password is taken as typed: the spaces around it are part of it.
    let (status, row, _) = h
        .call(&ada, "POST", "/site-logins", Some(json!({ "origin": "spaces.example", "username": "a", "password": " pw with spaces " })))
        .await;
    assert_eq!(status, 200, "{row}");
    let id = row["id"].as_str().expect("id").to_string();
    let (_, opened, _) = h
        .call(&ada, "POST", &format!("/site-logins/{id}/reveal"), None)
        .await;
    assert_eq!(opened["password"], " pw with spaces ");
    // Two saves of the same login at once end with one row and one secret.
    let (a, b) = tokio::join!(
        h.call(
            &ada,
            "POST",
            "/site-logins",
            Some(json!({ "origin": "race.example", "username": "r", "password": "one" }))
        ),
        h.call(
            &ada,
            "POST",
            "/site-logins",
            Some(json!({ "origin": "race.example", "username": "r", "password": "two" }))
        ),
    );
    assert_eq!(a.1["id"], b.1["id"], "one row: {} {}", a.2, b.2);
    let secrets: i64 = sqlx::query_scalar("select count(*) from secret_store where id like $1")
        .bind(format!("%{}%", a.1["id"].as_str().expect("id")))
        .fetch_one(h.store.pool())
        .await
        .expect("count");
    assert_eq!(secrets, 1, "one secret for the raced row");
    // The console's cookie does not open the reveal door; the header does.
    let response = h
        .client
        .post(format!("{}/site-logins/{id}/reveal", h.base))
        .header("cookie", format!("og_access={ada}"))
        .send()
        .await
        .expect("send");
    assert_eq!(
        response.status().as_u16(),
        401,
        "cookie-only reveal is refused"
    );

    let bare = harness(&database_url, false).await;
    let ada = bare
        .person(&format!("ada-{}@og.local", uuid::Uuid::now_v7().simple()))
        .await;
    let (status, body, _) = bare
        .call(
            &ada,
            "POST",
            "/site-logins",
            Some(json!({ "origin": "x.com", "username": "a", "password": "b" })),
        )
        .await;
    assert_eq!(status, 503, "{body}");
    let (status, list, _) = bare.call(&ada, "GET", "/site-logins", None).await;
    assert_eq!(status, 200, "listing needs no vault: {list}");
}

#[tokio::test]
async fn a_row_carries_its_kind_notes_and_code_and_the_icon_route_refuses_what_is_not_a_site() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, true).await;
    let ada = h
        .person(&format!("ada-{}@og.local", uuid::Uuid::now_v7().simple()))
        .await;

    // A password row with a title, notes and an authenticator seed.
    let (status, row, text) = h
        .call(
            &ada,
            "POST",
            "/site-logins",
            Some(json!({
                "origin": "github.com", "username": "ada", "password": PASSWORD,
                "label": "GitHub (work)", "notes": "recovery codes in the safe",
                "otpauth": "otpauth://totp/GitHub:ada?secret=JBSWY3DPEHPK3PXP&issuer=GitHub"
            })),
        )
        .await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(row["kind"], "password");
    assert_eq!(row["label"], "GitHub (work)");
    assert_eq!(row["notes"], "recovery codes in the safe");
    assert!(row["lastUsedAtMs"].is_null());
    assert!(
        !text.contains("JBSWY3DP"),
        "the seed is not in the reply: {text}"
    );
    let id = row["id"].as_str().expect("id").to_string();

    let (_, opened, _) = h
        .call(&ada, "POST", &format!("/site-logins/{id}/reveal"), None)
        .await;
    assert_eq!(opened["password"], PASSWORD);
    assert_eq!(
        opened["otpauth"],
        "otpauth://totp/GitHub:ada?secret=JBSWY3DPEHPK3PXP&issuer=GitHub"
    );

    // Notes and the title change in place; the secrets stay.
    let (status, _, _) = h
        .call(
            &ada,
            "PATCH",
            &format!("/site-logins/{id}"),
            Some(json!({ "notes": "moved the codes" })),
        )
        .await;
    assert_eq!(status, 200);
    let (_, list, _) = h.call(&ada, "GET", "/site-logins", None).await;
    assert_eq!(list[0]["notes"], "moved the codes", "{list}");
    let (_, opened, _) = h
        .call(&ada, "POST", &format!("/site-logins/{id}/reveal"), None)
        .await;
    assert_eq!(opened["password"], PASSWORD);

    // A code-only row needs a seed and no password; a password row needs a password.
    let (status, code, text) = h
        .call(
            &ada,
            "POST",
            "/site-logins",
            Some(json!({
                "origin": "aws.amazon.com", "username": "root", "kind": "code",
                "otpauth": "otpauth://totp/AWS:root?secret=JBSWY3DPEHPK3PXP"
            })),
        )
        .await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(code["kind"], "code");
    let (status, body, _) = h
        .call(
            &ada,
            "POST",
            "/site-logins",
            Some(json!({ "origin": "x.com", "username": "a", "kind": "code" })),
        )
        .await;
    assert_eq!(status, 400, "{body}");
    let (status, body, _) = h
        .call(&ada, "POST", "/site-logins", Some(json!({ "origin": "x.com", "username": "a", "otpauth": "not-a-uri", "password": "p" })))
        .await;
    assert_eq!(status, 400, "{body}");
    let (status, body, _) = h
        .call(
            &ada,
            "POST",
            "/site-logins",
            Some(json!({ "origin": "x.com", "username": "a", "password": "p", "kind": "passkey" })),
        )
        .await;
    assert_eq!(status, 400, "passkeys are not posted here: {body}");

    // Deleting a code row removes its seed.
    let code_id = code["id"].as_str().expect("id").to_string();
    let (status, _, _) = h
        .call(&ada, "DELETE", &format!("/site-logins/{code_id}"), None)
        .await;
    assert_eq!(status, 200);
    let sealed: i64 = sqlx::query_scalar("select count(*) from secret_store where id like $1")
        .bind(format!("%{code_id}%"))
        .fetch_one(h.store.pool())
        .await
        .expect("query");
    assert_eq!(sealed, 0);

    // The icon route takes a public host and nothing else.
    for bad in ["localhost", "127.0.0.1", "10.0.0.1", "x.local", "a"] {
        let (status, _, _) = h
            .call(&ada, "GET", &format!("/site-logins/icon/{bad}"), None)
            .await;
        assert_eq!(status, 400, "{bad}");
    }
    let (status, _, _) = h
        .call("nope", "GET", "/site-logins/icon/github.com", None)
        .await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn a_passkey_is_sealed_beside_the_password_row_and_the_reveal_door_takes_no_cookie() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, true).await;
    let ada = h
        .person(&format!(
            "ada-pk-{}@og.local",
            uuid::Uuid::now_v7().simple()
        ))
        .await;
    let account =
        AccountId::from_stored(h.agui.auth.minter.verify_access(&ada).expect("claims").sub);
    let vault = h.agui.vault.as_deref().expect("vault");

    // A password row for ada on github.com...
    let (status, password_row, text) = h
        .call(
            &ada,
            "POST",
            "/site-logins",
            Some(json!({ "origin": "github.com", "username": "ada", "password": "pw-1" })),
        )
        .await;
    assert_eq!(status, 200, "{text}");
    // ...and a passkey the site made for the same name: a second row, the key sealed.
    let passkey = h
        .store
        .upsert_site_login(
            vault,
            &account,
            &SiteLoginWrite {
                origin: "github.com",
                username: "ada",
                label: "github.com passkey",
                kind: "passkey",
                notes: "",
                password: None,
                otpauth: None,
                passkey: Some(PasskeyWrite {
                    credential_id_b64: "AQID".to_string(),
                    rp_id: "github.com".to_string(),
                    user_handle_b64: "dXNlcg==".to_string(),
                    private_key_b64: "PKCS8-TEST".to_string(),
                }),
            },
            1,
        )
        .await
        .expect("upsert passkey");
    assert_ne!(passkey.id, password_row["id"], "two rows, not one flipped");
    assert_eq!(passkey.kind, "passkey");
    let (status, rows, _) = h.call(&ada, "GET", "/site-logins", None).await;
    assert_eq!(status, 200);
    let kinds: Vec<&str> = rows
        .as_array()
        .expect("rows")
        .iter()
        .filter(|r| r["origin"] == "github.com" && r["username"] == "ada")
        .filter_map(|r| r["kind"].as_str())
        .collect();
    assert_eq!(kinds.len(), 2, "{rows}");
    assert!(
        kinds.contains(&"password") && kinds.contains(&"passkey"),
        "{rows}"
    );
    let secrets = h
        .store
        .open_site_login(vault, &account, &passkey.id)
        .await
        .expect("open")
        .expect("row");
    assert_eq!(secrets.passkey_key.as_deref(), Some("PKCS8-TEST"));
    assert_eq!(secrets.password, None);
    // The password row still opens as itself.
    let password_id = password_row["id"].as_str().expect("id");
    let secrets = h
        .store
        .open_site_login(vault, &account, password_id)
        .await
        .expect("open")
        .expect("row");
    assert_eq!(secrets.password.as_deref(), Some("pw-1"));
    assert_eq!(secrets.passkey_key, None);

    // The reveal never gives out the passkey's key, and never opens for the console's
    // cookie, whatever else is in the request.
    let (status, body, _) = h
        .call(
            &ada,
            "POST",
            &format!("/site-logins/{}/reveal", passkey.id),
            None,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.get("passkeyKey").is_none() && body.get("privateKey").is_none(),
        "{body}"
    );
    assert_eq!(body["password"], Value::Null);
    let response = h
        .client
        .post(format!("{}/site-logins/{password_id}/reveal", h.base))
        .header("authorization", "x")
        .header("cookie", format!("og_access={ada}"))
        .send()
        .await
        .expect("send");
    assert_eq!(
        response.status().as_u16(),
        401,
        "the cookie does not open the reveal"
    );
    // The list, by contrast, takes the cookie: it carries no secret.
    let response = h
        .client
        .get(format!("{}/site-logins", h.base))
        .header("cookie", format!("og_access={ada}"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status().as_u16(), 200);

    // Deleting the passkey row takes its key with it.
    let (status, _, _) = h
        .call(
            &ada,
            "DELETE",
            &format!("/site-logins/{}", passkey.id),
            None,
        )
        .await;
    assert_eq!(status, 200);
    let sealed: i64 = sqlx::query_scalar("select count(*) from secret_store where id like $1")
        .bind(format!("%{}%", passkey.id))
        .fetch_one(h.store.pool())
        .await
        .expect("query");
    assert_eq!(sealed, 0);
}
