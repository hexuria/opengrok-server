//! #356: a registry install is pinned, encrypted, and account-owned at the HTTP and run boundaries.
//!
//! Tests drive the registry HTTP routes and an actual scripted turn, since a stored row alone
//! cannot establish the account boundary. One test needs Postgres; the adapter test does not.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use opengrok_box::{BoxError, BoxResult, CommandOutput, Computer, StartedCommand};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::AccountId;
use opengrok_harness::{DeltaStream, ModelDelta, ModelDoor, ModelError, ModelRequest};
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

/// A box that is always up and never asked to do anything: the listing only looks at it.
struct IdleBox;

#[async_trait::async_trait]
impl Computer for IdleBox {
    async fn create(&self, _ttl: Option<u64>) -> BoxResult<String> {
        Ok(format!("bx_idle_{}", uuid::Uuid::now_v7().simple()))
    }
    async fn run(&self, _b: &str, _c: &str, _t: u32) -> BoxResult<CommandOutput> {
        Err(BoxError::NoSuchBox)
    }
    async fn start(&self, _b: &str, _c: &str) -> BoxResult<StartedCommand> {
        Err(BoxError::NoSuchBox)
    }
    async fn watch(&self, _b: &str, _p: &str) -> BoxResult<StartedCommand> {
        Err(BoxError::NoSuchBox)
    }
    async fn read_file(&self, _b: &str, _p: &str) -> BoxResult<String> {
        Err(BoxError::NoSuchBox)
    }
    async fn write_file(&self, _b: &str, _p: &str, _c: &str) -> BoxResult<()> {
        Err(BoxError::NoSuchBox)
    }
    async fn expose_port(&self, _b: &str, _p: u16, _t: &str) -> BoxResult<String> {
        Err(BoxError::NoSuchBox)
    }
    async fn stop(&self, _b: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn resume(&self, _b: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn destroy(&self, _b: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn state(&self, _b: &str) -> BoxResult<String> {
        Ok("running".to_string())
    }
}

struct ReadPluginSkill;
#[async_trait::async_trait]
impl ModelDoor for ReadPluginSkill {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
        let deltas = if request.messages.iter().any(|m| m.role == "tool") {
            vec![ModelDelta::Text("done".into())]
        } else {
            vec![
                ModelDelta::ToolCallStart {
                    id: "read-plugin".into(),
                    name: "use_skill".into(),
                },
                ModelDelta::ToolCallArgs {
                    id: "read-plugin".into(),
                    delta: r#"{"name":"demo.triage"}"#.into(),
                },
                ModelDelta::ToolCallEnd {
                    id: "read-plugin".into(),
                },
            ]
        };
        Ok(Box::pin(futures::stream::iter(deltas.into_iter().map(Ok))))
    }
}

struct Harness {
    base: String,
    fixture: String,
    agui: AgUiState,
    store: PgStore,
    client: reqwest::Client,
}

async fn harness(
    database_url: &str,
    registry: opengrok_integrations::registry::Registry,
) -> Harness {
    harness_with_computer(database_url, registry, None).await
}

async fn harness_with_computer(
    database_url: &str,
    registry: opengrok_integrations::registry::Registry,
    computer: Option<Arc<dyn Computer>>,
) -> Harness {
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
        door: Arc::new(ReadPluginSkill),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer,
        vault: Some(Arc::new(
            opengrok_store::Vault::from_base64_keys(
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
                &[],
            )
            .expect("vault"),
        )),
        connectors: Connectors {
            providers: Arc::new(BTreeMap::new()),
            redirect_uri: "http://127.0.0.1/callback".to_string(),
        },
        plugins: Arc::new(BTreeMap::new()),
        host_settings: None,
    };
    let gateway = HostState::new(agui.clone(), None);
    let app = opengrok_server::router(agui.clone(), gateway);
    // The fixture registry is served at the real paths on a listener of its own, recorded as the
    // server's routes are, so the wire corpus has `/plugins/*` as NativeChat calls them. A test
    // names it `/fixture/...`.
    let fixture = opengrok_server::recorded(
        opengrok_server::plugin_registry::router_with_registry(agui.clone(), Some(registry)),
    );
    let serve = |app: axum::Router| async move {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        format!("http://127.0.0.1:{}", addr.port())
    };
    Harness {
        base: serve(app).await,
        fixture: serve(fixture).await,
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
        let url = match path.strip_prefix("/fixture") {
            Some(path) => format!("{}{path}", self.fixture),
            None => format!("{}{path}", self.base),
        };
        let request = match method {
            "GET" => self.client.get(url),
            "POST" => self.client.post(url),
            "PUT" => self.client.put(url),
            "DELETE" => self.client.delete(url),
            "PATCH" => self.client.patch(url),
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

const OLD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const NEW: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const FORK: &str = "cccccccccccccccccccccccccccccccccccccccc";

async fn credential_bot(h: &Harness, account: &AccountId) -> opengrok_core::id::CoworkerId {
    let token = h
        .agui
        .auth
        .minter
        .mint_access(
            account.as_str(),
            "sess-test",
            "fixture@og.local",
            "ultra",
            chrono::Utc::now().timestamp(),
            3600,
        )
        .unwrap();
    let (status, hired, text) = h
        .call(
            &token,
            "POST",
            "/coworkers",
            Some(json!({"name":"Credential bot"})),
        )
        .await;
    assert_eq!(status, 201, "{text}");
    opengrok_core::id::CoworkerId::from_stored(hired["id"].as_str().unwrap())
}

/// The credentials a turn reading the current install would get.
async fn current_values(h: &Harness, account: &AccountId, name: &str) -> BTreeMap<String, String> {
    let installs = opengrok_integrations::installed::list(&h.store, account)
        .await
        .unwrap();
    let Some(installation) = installs.into_iter().find(|i| i.name == name) else {
        return BTreeMap::new();
    };
    let vault = h.agui.vault.as_ref().unwrap();
    opengrok_integrations::installed::values_for_installation(
        &h.store,
        vault,
        account,
        &credential_bot(h, account).await,
        &installation,
    )
    .await
    .unwrap()
    .values
}

async fn registry_fixture() -> (
    opengrok_integrations::registry::Registry,
    Arc<std::sync::atomic::AtomicBool>,
) {
    use axum::{
        Router,
        extract::Path,
        response::{IntoResponse, Response},
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    let advanced = Arc::new(AtomicBool::new(false));
    let flag = advanced.clone();
    let app = Router::new().route("/{*path}", axum::routing::get(move |Path(path): Path<String>| {
        let flag = flag.clone();
        async move {
            let json = |value: Value| -> Response { axum::Json(value).into_response() };
            if path == "repos/fixture/marketplace/commits/HEAD" {
                return json(json!({"sha": if flag.load(Ordering::SeqCst) {NEW} else {OLD}}));
            }
            // OLD is NEW's parent on the default branch; FORK is reachable from neither.
            if let Some(range) = path.strip_prefix("repos/fixture/marketplace/compare/") {
                let status = match range.split_once("...") {
                    Some((base, head)) if base == head => "identical",
                    Some((OLD, NEW)) => "ahead",
                    Some((NEW, OLD)) => "behind",
                    _ => "diverged",
                };
                return json(json!({"status": status}));
            }
            if path.starts_with("repos/fixture/upstream/git/trees/") {
                return json(json!({"tree":[{"path":"bundle/plugin.json","type":"blob","mode":"100644"}], "truncated":false}));
            }
            if path.ends_with("/bundle/plugin.json") { return json(json!({"name":"external"})); }
            if path.starts_with("repos/fixture/marketplace/git/trees/") {
                return json(json!({"truncated":false,"tree": [
                    {"path":"plugins/demo/.grok-plugin/plugin.json","type":"blob","mode":"100644"},
                    {"path":"plugins/demo/.mcp.json","type":"blob","mode":"100644"},
                    {"path":"plugins/demo/skills/triage/SKILL.md","type":"blob","mode":"100644"},
                    {"path":"plugins/demo/skills/triage/reference.txt","type":"blob","mode":"100644"},
                    {"path":"plugins/demo/commands/deploy.md","type":"blob","mode":"100644"}
                ]}));
            }
            if path.ends_with("/.grok-plugin/marketplace.json") {
                return json(json!({"name":"fixture", "plugins":[{"name":"demo","description":"Demo","category":"development","homepage":"https://demo.example","source":{"type":"local","path":"./plugins/demo"}}, {"name":"external","source":{"source":"url","url":"https://github.com/fixture/upstream.git","sha":OLD,"path":"bundle"}}]}));
            }
            if path.ends_with("plugins/demo/.grok-plugin/plugin.json") {
                return json(json!({"name":"demo", "description":if path.contains(OLD) {"old bundle"} else {"new bundle"}, "hooks":{}}));
            }
            if path.ends_with("plugins/demo/.mcp.json") {
                return json(json!({"mcpServers": {
                    "hosted":{"type":"http","url":"https://example.com/mcp","headers":{"Authorization":"Bearer ${DEMO_TOKEN}"}},
                    "local":{"command":"never-launch-me"}
                }}));
            }
            if path.ends_with("/skills/triage/SKILL.md") { return "---\nname: triage\ndescription: Triage safely\n---\nRead the reference first.".into_response(); }
            if path.ends_with("/skills/triage/reference.txt") { return "pinned reference".into_response(); }
            axum::http::StatusCode::NOT_FOUND.into_response()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (
        opengrok_integrations::registry::Registry::new(
            base.clone(),
            base,
            "fixture/marketplace".into(),
        )
        .unwrap()
        .with_head_ttl(std::time::Duration::ZERO),
        advanced,
    )
}

#[tokio::test]
async fn pinned_installations_and_credentials_belong_only_to_the_driving_account() {
    let url = database_or_skip!();
    let (registry, advanced) = registry_fixture().await;
    let h = harness(&url, registry).await;
    let org = format!("org_registry_{}", uuid::Uuid::now_v7());
    let a = h.person(Some(&org)).await;
    let b = h.person(Some(&org)).await;
    let account =
        |token: &str| AccountId::from_stored(h.agui.auth.minter.verify_access(token).unwrap().sub);
    let aid = account(&a);
    let bid = account(&b);
    let catalog_path = "/fixture/plugins/catalog";
    assert_eq!(h.call("", "GET", catalog_path, None).await.0, 401);
    let (status, catalog, _) = h.call(&a, "GET", catalog_path, None).await;
    assert_eq!(status, 200);
    assert_eq!(catalog["revision"], OLD);
    let demo = catalog["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == "demo")
        .unwrap();
    assert_eq!(demo["category"], "development");
    assert_eq!(demo["homepage"], "https://demo.example");
    let (status, detail, _) = h
        .call(
            &a,
            "GET",
            &format!("{catalog_path}/demo?revision={OLD}"),
            None,
        )
        .await;
    assert_eq!(status, 200, "{detail}");
    assert!(detail["manifest"]["name"].is_string(), "{detail}");
    assert!(
        detail["parts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "local" && p["supported"] == false && p["reason"].is_string())
    );
    assert!(
        detail["parts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["kind"] == "hooks" && p["supported"] == false)
    );
    let install_path = "/fixture/plugins/installations";
    let install = json!({"name":"demo","registryRevision":OLD});
    assert_eq!(
        h.call(
            &a,
            "POST",
            install_path,
            Some(json!({"name":"demo","registryRevision":"main"}))
        )
        .await
        .0,
        422
    );
    let (status, row, text) = h
        .call(&a, "POST", install_path, Some(install.clone()))
        .await;
    assert_eq!(status, 201, "{text}");
    assert_eq!(row["revision"], OLD);
    assert_eq!(h.call(&a, "POST", install_path, Some(install)).await.0, 409);
    assert!(
        h.call(&b, "GET", install_path, None)
            .await
            .1
            .as_array()
            .unwrap()
            .is_empty()
    );
    let credential_path = "/fixture/plugins/installations/demo/credentials/demo";
    assert_eq!(
        h.call(
            &b,
            "PUT",
            credential_path,
            Some(json!({"token":"other-secret"}))
        )
        .await
        .0,
        404
    );
    let token = "private-plugin-test-token";
    assert_eq!(
        h.call(&a, "PUT", credential_path, Some(json!({"token":token})))
            .await
            .0,
        204
    );
    let values = current_values(&h, &aid, "demo").await;
    assert_eq!(values["DEMO_TOKEN"], token);
    // A different installation with the same connector cannot supply this plugin's token.
    sqlx::query("insert into plugin_installation(account_id,name,registry,registry_revision,repository,revision,bundle,installed_at_ms) select account_id,'another-plugin',registry,registry_revision,repository,revision,bundle,installed_at_ms from plugin_installation where account_id = $1 and name = 'demo'")
        .bind(aid.as_str()).execute(h.store.pool()).await.unwrap();
    sqlx::query("update plugin_credential set plugin_name = 'another-plugin' where account_id = $1 and plugin_name = 'demo'")
        .bind(aid.as_str()).execute(h.store.pool()).await.unwrap();
    assert!(current_values(&h, &aid, "demo").await.is_empty());
    sqlx::query("update plugin_credential set plugin_name = 'demo' where account_id = $1 and plugin_name = 'another-plugin'")
        .bind(aid.as_str()).execute(h.store.pool()).await.unwrap();
    sqlx::query(
        "delete from plugin_installation where account_id = $1 and name = 'another-plugin'",
    )
    .bind(aid.as_str())
    .execute(h.store.pool())
    .await
    .unwrap();
    assert_eq!(current_values(&h, &aid, "demo").await["DEMO_TOKEN"], token);
    assert!(current_values(&h, &bid, "demo").await.is_empty());
    let cipher: Vec<u8> = sqlx::query_scalar("select ciphertext from secret_store where id = $1")
        .bind(format!("plugin/{aid}/demo/demo"))
        .fetch_one(h.store.pool())
        .await
        .unwrap();
    assert!(
        !cipher
            .windows(token.len())
            .any(|bytes| bytes == token.as_bytes())
    );
    assert!(
        !h.call(&a, "GET", install_path, None)
            .await
            .2
            .contains(token)
    );
    assert!(
        h.call(&a, "GET", "/connectors", None)
            .await
            .1
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["plugin"] == "demo")
    );
    assert!(
        h.call(&b, "GET", "/connectors", None)
            .await
            .1
            .as_array()
            .unwrap()
            .is_empty()
    );
    let (status, hired, text) = h
        .call(
            &a,
            "POST",
            "/coworkers",
            Some(json!({"name":"Registry bot"})),
        )
        .await;
    assert_eq!(status, 201, "{text}");
    let bot = hired["id"].as_str().unwrap();
    let bot_id = opengrok_core::id::CoworkerId::from_stored(bot);
    // "ALL TOOLS" NEVER SWITCHES AN INSTALL ON: it admits every plugin without naming one, and an
    // account's install is on only where it is named. Its row says off, and no skill is offered.
    use opengrok_policy::ToolSet;
    let at = chrono::Utc::now().timestamp_millis();
    h.store
        .grant_access(
            &aid,
            &bot_id,
            &ToolSet::All,
            &ToolSet::All,
            &ToolSet::None,
            at,
        )
        .await
        .unwrap();
    let (_, everything, _) = h
        .call(&a, "GET", &format!("/coworkers/{bot}/ceiling"), None)
        .await;
    let demo = everything["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "demo");
    assert_eq!(demo.unwrap()["enabled"], false, "{everything}");
    assert!(
        opengrok_integrations::turn::skill_offers(&h.store, &aid, &bot_id)
            .await
            .is_empty()
    );
    assert_eq!(
        opengrok_integrations::installed::for_turn(&h.store, &aid, &bot_id)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        opengrok_integrations::installed::for_turn(&h.store, &bid, &bot_id)
            .await
            .unwrap()
            .is_empty()
    );
    let ceiling_path = format!("/coworkers/{bot}/ceiling");
    let (status, ceiling, _) = h.call(&a, "GET", &ceiling_path, None).await;
    assert_eq!(status, 200);
    assert!(
        ceiling["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["name"] == "demo" && r["kind"] == "plugin")
    );
    assert_eq!(
        h.call(
            &a,
            "PUT",
            &ceiling_path,
            Some(json!({"enabled":["demo"],"version":ceiling["version"]}))
        )
        .await
        .0,
        200
    );
    let (_, current, _) = h.call(&a, "GET", &ceiling_path, None).await;
    assert_eq!(
        h.call(
            &a,
            "PUT",
            &ceiling_path,
            Some(json!({"enabled":[],"version":current["version"]}))
        )
        .await
        .0,
        200
    );
    // SWITCHED OFF, THE PLUGIN'S SKILLS ARE NOT OFFERED, and a model naming one anyway reads
    // nothing: before, the ceiling gated its MCP tools only and `use_skill` handed over the text.
    let (status, tools, _) = h
        .call(&a, "GET", &format!("/coworkers/{bot}/tools"), None)
        .await;
    assert_eq!(status, 200, "{tools}");
    assert!(!tools.to_string().contains("use_skill"), "{tools}");
    let turn = || async {
        let (status, _, sse) = h.call(&a, "POST", "/ag-ui", Some(json!({
            "threadId": format!("thr_{}",uuid::Uuid::now_v7()), "runId": uuid::Uuid::now_v7().to_string(),
            "messages":[{"id":"m1","role":"user","content":"triage"}], "forwardedProps":{"coworkerId":bot}
        }))).await;
        assert_eq!(status, 200);
        assert!(sse.contains("RUN_FINISHED"), "{sse}");
        sse
    };
    let off = turn().await;
    assert!(!off.contains("Read the reference first."), "{off}");
    assert!(!off.contains("THIRD-PARTY PLUGIN"), "{off}");
    let (_, current, _) = h.call(&a, "GET", &ceiling_path, None).await;
    let on = json!({"enabled":["demo"],"version":current["version"]});
    assert_eq!(h.call(&a, "PUT", &ceiling_path, Some(on)).await.0, 200);
    // Stop the model path before any paid inference: tools listing uses the same SkillSource.
    let (status, tools, _) = h
        .call(&a, "GET", &format!("/coworkers/{bot}/tools"), None)
        .await;
    assert_eq!(status, 200, "{tools}");
    assert!(tools.to_string().contains("use_skill"));
    let sse = turn().await;
    assert!(sse.contains("Read the reference first."), "{sse}");
    assert!(sse.contains("THIRD-PARTY PLUGIN"));
    assert!(
        sse.contains("files are not on your computer")
            || sse.contains("this coworker has no computer")
    );
    // ONE SKILL OFF FOR THIS BOT, the plugin still on: it is no longer offered, and a ceiling
    // save of the plugin's switch keeps that choice.
    let skills_path = format!("/coworkers/{bot}/plugin-skills");
    let (status, listed, _) = h.call(&a, "GET", &skills_path, None).await;
    assert_eq!(status, 200, "{listed}");
    assert_eq!(listed["skills"][0]["skill"], "triage", "{listed}");
    assert_eq!(listed["skills"][0]["on"], true, "{listed}");
    let (status, page, _) = h
        .call(&a, "GET", &format!("{skills_path}/demo/triage"), None)
        .await;
    assert_eq!(status, 200, "{page}");
    assert!(
        page["body"]
            .as_str()
            .unwrap()
            .contains("Read the reference first.")
    );
    let off = json!({"plugin":"demo","skill":"triage","on":false});
    assert_eq!(h.call(&a, "PUT", &skills_path, Some(off)).await.0, 200);
    let (_, current, _) = h.call(&a, "GET", &ceiling_path, None).await;
    let again = json!({"enabled":["demo"],"version":current["version"]});
    assert_eq!(h.call(&a, "PUT", &ceiling_path, Some(again)).await.0, 200);
    let (_, listed, _) = h.call(&a, "GET", &skills_path, None).await;
    assert_eq!(listed["skills"][0]["on"], false, "{listed}");
    let (_, tools, _) = h
        .call(&a, "GET", &format!("/coworkers/{bot}/tools"), None)
        .await;
    assert!(!tools.to_string().contains("use_skill"), "{tools}");
    let on = json!({"plugin":"demo","skill":"triage","on":true});
    assert_eq!(h.call(&a, "PUT", &skills_path, Some(on)).await.0, 200);
    let (_, tools, _) = h
        .call(&a, "GET", &format!("/coworkers/{bot}/tools"), None)
        .await;
    assert!(tools.to_string().contains("use_skill"), "{tools}");
    let unknown = json!({"plugin":"demo","skill":"nope","on":false});
    assert_eq!(h.call(&a, "PUT", &skills_path, Some(unknown)).await.0, 422);
    assert_eq!(
        h.call(
            &a,
            "PATCH",
            &format!("/coworkers/{bot}"),
            Some(json!({"visibility":"org"}))
        )
        .await
        .0,
        200
    );
    // Even after the member installs the same plugin, the shared bot is not their bot.
    assert_eq!(
        h.call(
            &b,
            "POST",
            install_path,
            Some(json!({"name":"demo","registryRevision":OLD}))
        )
        .await
        .0,
        201
    );
    assert_eq!(
        h.call(
            &b,
            "PUT",
            credential_path,
            Some(json!({"token":"member-token"}))
        )
        .await
        .0,
        204
    );
    let (status, member_tools, _) = h
        .call(&b, "GET", &format!("/coworkers/{bot}/tools"), None)
        .await;
    assert_eq!(
        status, 404,
        "the tools configuration belongs to the owner: {member_tools}"
    );
    assert!(!member_tools.to_string().contains("use_skill"));
    let (status, _, member_turn) = h.call(&b, "POST", "/ag-ui", Some(json!({
        "threadId": format!("thr_{}",uuid::Uuid::now_v7()), "runId": uuid::Uuid::now_v7().to_string(),
        "messages":[{"id":"m1","role":"user","content":"triage"}], "forwardedProps":{"coworkerId":bot}
    }))).await;
    assert_eq!(status, 200, "{member_turn}");
    assert!(member_turn.contains("RUN_FINISHED"), "{member_turn}");
    assert!(!member_turn.contains("Read the reference first."));
    assert!(!member_turn.contains("THIRD-PARTY PLUGIN"));

    assert!(
        opengrok_integrations::installed::for_turn(&h.store, &bid, &bot_id)
            .await
            .unwrap()
            .is_empty()
    );
    advanced.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        h.call(&a, "GET", catalog_path, None).await.1["revision"],
        NEW
    );
    let kept = h.call(&a, "GET", install_path, None).await.1;
    assert_eq!(kept[0]["revision"], OLD);
    assert_eq!(kept[0]["bundle"]["manifest"]["description"], "old bundle");
    // A member can remove their own snapshot, never the owner's. The owner's secret survives.
    assert_eq!(
        h.call(&b, "DELETE", "/fixture/plugins/installations/demo", None)
            .await
            .0,
        204
    );
    assert_eq!(current_values(&h, &aid, "demo").await["DEMO_TOKEN"], token);
    assert_eq!(
        h.call(&a, "DELETE", "/fixture/plugins/installations/demo", None)
            .await
            .0,
        204
    );
    // Uninstall takes the plugin out of every Bot's ceiling: nothing is left to switch a
    // replacement install on without its owner choosing it again.
    let (_, after, _) = h.call(&a, "GET", &ceiling_path, None).await;
    assert!(
        !after["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["name"] == "demo"),
        "{after}"
    );
    let secrets: i64 = sqlx::query_scalar("select count(*) from secret_store where id = $1")
        .bind(format!("plugin/{aid}/demo/demo"))
        .fetch_one(h.store.pool())
        .await
        .unwrap();
    assert_eq!(secrets, 0);
    assert!(
        opengrok_integrations::installed::for_turn(&h.store, &aid, &bot_id)
            .await
            .unwrap()
            .is_empty()
    );
    // Reinstall is the explicit update: only this request selects the new revision.
    assert_eq!(
        h.call(
            &a,
            "POST",
            install_path,
            Some(json!({"name":"demo","registryRevision":NEW}))
        )
        .await
        .0,
        201
    );
    let updated = h.call(&a, "GET", install_path, None).await.1;
    assert_eq!(updated[0]["revision"], NEW);
    let (_, reinstalled, _) = h.call(&a, "GET", &ceiling_path, None).await;
    assert!(
        reinstalled["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["name"] == "demo" && r["enabled"] == false),
        "{reinstalled}"
    );
    assert_eq!(
        updated[0]["bundle"]["manifest"]["description"],
        "new bundle"
    );
    assert!(current_values(&h, &aid, "demo").await.is_empty());
    let (save, removed) = tokio::join!(
        opengrok_integrations::installed::credential(
            &h.store,
            h.agui.vault.as_ref().unwrap(),
            &aid,
            "demo",
            "demo",
            "raced-token",
            1
        ),
        opengrok_integrations::installed::uninstall(&h.store, &aid, "demo", 1)
    );
    assert!(save.is_ok());
    assert!(removed.unwrap());
    let orphan_count: i64 = sqlx::query_scalar("select count(*) from secret_store where id = $1")
        .bind(format!("plugin/{aid}/demo/demo"))
        .fetch_one(h.store.pool())
        .await
        .unwrap();
    assert_eq!(
        orphan_count, 0,
        "save/uninstall ordering must never leave an orphan secret"
    );
}

#[tokio::test]
async fn registry_resolves_local_and_external_commits_without_a_database() {
    let (registry, flag) = registry_fixture().await;
    let old = registry.catalog(None).await.unwrap();
    assert_eq!(old.revision, OLD);
    let external = old.plugins.iter().find(|p| p.name == "external").unwrap();
    assert_eq!(external.repository, "fixture/upstream");
    assert_eq!(external.revision, OLD);
    assert_eq!(
        registry.bundle(external).await.unwrap().manifest.name,
        "external"
    );
    flag.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(registry.catalog(None).await.unwrap().revision, NEW);
    let kept = registry.catalog(Some(OLD)).await.unwrap();
    assert_eq!(kept.revision, OLD);
    assert_eq!(
        registry
            .bundle(kept.plugins.iter().find(|p| p.name == "demo").unwrap())
            .await
            .unwrap()
            .manifest
            .description
            .as_deref(),
        Some("old bundle")
    );
    assert!(registry.catalog(Some("main")).await.is_err());
    // GitHub can place it, but not on the default branch: a fork's commit under the parent path.
    assert!(matches!(
        registry.catalog(Some(FORK)).await,
        Err(opengrok_integrations::registry::Error::Refused(_))
    ));
    // A replica whose HEAD is cached for five minutes still takes a pin newer than it.
    let (cached, moved) = registry_fixture().await;
    let cached = cached.with_head_ttl(std::time::Duration::from_secs(300));
    assert_eq!(cached.catalog(None).await.unwrap().revision, OLD);
    moved.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(cached.catalog(Some(NEW)).await.unwrap().revision, NEW);
    let mut unsafe_entry = external.clone();
    unsafe_entry.path = "../escape".into();
    assert!(registry.bundle(&unsafe_entry).await.is_err());
    unsafe_entry.path = String::new();
    unsafe_entry.repository = "../evil".into();
    assert!(registry.bundle(&unsafe_entry).await.is_err());
}

/// Explicitly opt-in: public service availability and rate limits cannot decide CI.
#[tokio::test]
#[ignore = "live GitHub catalog and Exa MCP discovery; no search or personal token"]
async fn live_pinned_registry_bundle_reports_oauth_required() {
    let url = database_or_skip!();
    let registry = opengrok_integrations::registry::Registry::github(
        "hexuria/plugin-marketplace".into(),
        std::env::var("OG_PLUGIN_REGISTRY_TOKEN").ok(),
    )
    .unwrap();
    let h = harness_with_computer(&url, registry, Some(Arc::new(IdleBox))).await;
    let owner = h.person(None).await;
    let stranger = h.person(None).await;
    let (status, catalog, text) = h
        .call(&owner, "GET", "/fixture/plugins/catalog", None)
        .await;
    assert_eq!(status, 200, "live catalog: {text}");
    let revision = catalog["revision"].as_str().unwrap();
    let entries = catalog["plugins"].as_array().unwrap();
    let exa = entries
        .iter()
        .find(|p| p["name"] == "exa")
        .expect("registry lists Exa");
    let (status, detail, text) = h
        .call(
            &owner,
            "GET",
            &format!("/fixture/plugins/catalog/exa?revision={revision}"),
            None,
        )
        .await;
    assert_eq!(status, 200, "real bundle detail: {text}");
    assert!(
        detail["parts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["kind"] == "mcp" && p["supported"] == true)
    );
    let (status, installed, text) = h
        .call(
            &owner,
            "POST",
            "/fixture/plugins/installations",
            Some(json!({"name":"exa","registryRevision":revision})),
        )
        .await;
    assert_eq!(status, 201, "real bundle install: {text}");
    let (status, hired, text) = h
        .call(
            &owner,
            "POST",
            "/coworkers",
            Some(json!({"name":"Live catalog check"})),
        )
        .await;
    assert_eq!(status, 201, "hire: {text}");
    let bot = hired["id"].as_str().unwrap();
    let path = format!("/coworkers/{bot}/ceiling");
    let (status, ceiling, text) = h.call(&owner, "GET", &path, None).await;
    assert_eq!(status, 200, "ceiling: {text}");
    assert_eq!(
        h.call(
            &owner,
            "PUT",
            &path,
            Some(json!({"enabled":["exa"],"version":ceiling["version"]}))
        )
        .await
        .0,
        200
    );
    let aid = AccountId::from_stored(h.agui.auth.minter.verify_access(&owner).unwrap().sub);
    let bid = opengrok_core::id::CoworkerId::from_stored(bot);
    let (coworker, _) = h.store.load_coworker(&bid).await.unwrap();
    assert!(
        coworker.computer().is_some(),
        "MCP toolbox path requires a provisioned computer"
    );
    let policy = h.store.policy_to_use(&aid, &bid).await.unwrap();
    assert!(opengrok_policy::may_run_any_under(
        &aid, &bid, "exa.exa.", &policy
    ));
    let installs = opengrok_integrations::installed::for_turn(&h.store, &aid, &bid)
        .await
        .unwrap();
    let (endpoints, problems) =
        opengrok_tools::mcp::endpoints_for(&installs[0].bundle.plugin(), &BTreeMap::new());
    assert!(problems.is_empty());
    assert_eq!(endpoints.len(), 1);
    assert_eq!(endpoints[0].url, "https://mcp.exa.ai/mcp/oauth");
    let dialled = opengrok_tools::mcp::Pool::global()
        .dial("live-oauth-boundary", endpoints, |_| true)
        .await;
    assert!(
        dialled.tools.is_empty(),
        "this pinned endpoint requires OAuth"
    );
    assert!(
        dialled
            .unavailable
            .get("exa.exa")
            .is_some_and(|e| e.contains("Auth required")),
        "{:?}",
        dialled.unavailable
    );
    let (status, tools, text) = h
        .call(&owner, "GET", &format!("/coworkers/{bot}/tools"), None)
        .await;
    assert_eq!(status, 200, "tools: {text}");
    let remote: Vec<_> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["kind"] == "plugin")
        .map(|t| t["name"].clone())
        .collect();
    assert!(
        remote.is_empty(),
        "OAuth-protected tools must not be claimed reachable: {tools}"
    );
    assert!(
        h.call(&stranger, "GET", "/fixture/plugins/installations", None)
            .await
            .1
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        h.call(&owner, "DELETE", "/fixture/plugins/installations/exa", None)
            .await
            .0,
        204
    );
    assert!(
        h.call(&owner, "GET", "/fixture/plugins/installations", None)
            .await
            .1
            .as_array()
            .unwrap()
            .is_empty()
    );
    println!(
        "LIVE_REGISTRY_EVIDENCE {}",
        json!({"registry":catalog["registry"],"registryRevision":revision,
        "catalogEntries":entries.len(),"plugin":"exa","repository":exa["repository"],"sourceRevision":installed["revision"],
        "parts":detail["parts"],"remoteTools":remote,"crossAccountVisible":false,"uninstalled":true,
        "oauthUsed":false,"toolCalls":0,"connectionOutcome":"blocked: Auth required"})
    );
}

#[tokio::test]
#[ignore = "live anonymous Exa MCP transport baseline; no search is executed"]
async fn live_anonymous_mcp_transport_lists_tools() {
    // Exa's documented public endpoint differs from the registry's pinned OAuth endpoint.
    let endpoint = opengrok_tools::mcp::Endpoint {
        plugin: "exa".into(),
        server: "public".into(),
        url: "https://mcp.exa.ai/mcp".into(),
        headers: BTreeMap::new(),
        harden: None,
    };
    let connected = opengrok_tools::mcp::Pool::global()
        .dial("live-anonymous-baseline", vec![endpoint], |_| true)
        .await;
    assert!(
        connected.unavailable.is_empty(),
        "{:?}",
        connected.unavailable
    );
    assert!(!connected.tools.is_empty());
    let names: Vec<_> = connected
        .tools
        .iter()
        .map(|t| t.remote_name.clone())
        .collect();
    println!(
        "LIVE_MCP_BASELINE {}",
        json!({"endpoint":"https://mcp.exa.ai/mcp","remoteTools":names,"oauthUsed":false,"toolCalls":0})
    );
}

#[tokio::test]
async fn an_old_installation_snapshot_cannot_receive_a_replacement_token() {
    let url = database_or_skip!();
    let (registry, advanced) = registry_fixture().await;
    let h = harness(&url, registry).await;
    let owner = h.person(None).await;
    let account = AccountId::from_stored(h.agui.auth.minter.verify_access(&owner).unwrap().sub);
    let route = "/fixture/plugins/installations";
    assert_eq!(
        h.call(
            &owner,
            "POST",
            route,
            Some(json!({"name":"demo","registryRevision":OLD}))
        )
        .await
        .0,
        201
    );
    let snapshot = opengrok_integrations::installed::list(&h.store, &account)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(
        h.call(
            &owner,
            "DELETE",
            "/fixture/plugins/installations/demo",
            None
        )
        .await
        .0,
        204
    );
    advanced.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        h.call(
            &owner,
            "POST",
            route,
            Some(json!({"name":"demo","registryRevision":NEW}))
        )
        .await
        .0,
        201
    );
    assert_eq!(
        h.call(
            &owner,
            "PUT",
            "/fixture/plugins/installations/demo/credentials/demo",
            Some(json!({"token":"replacement-secret"}))
        )
        .await
        .0,
        204
    );
    let bot = credential_bot(&h, &account).await;
    let old_values = opengrok_integrations::installed::values_for_installation(
        &h.store,
        h.agui.vault.as_ref().unwrap(),
        &account,
        &bot,
        &snapshot,
    )
    .await
    .unwrap();
    assert!(
        old_values.values.is_empty(),
        "a snapshot captured before uninstall must not receive the replacement credential"
    );
    let current = opengrok_integrations::installed::list(&h.store, &account)
        .await
        .unwrap()
        .remove(0);
    let current_values = opengrok_integrations::installed::values_for_installation(
        &h.store,
        h.agui.vault.as_ref().unwrap(),
        &account,
        &bot,
        &current,
    )
    .await
    .unwrap();
    assert_eq!(current_values.values["DEMO_TOKEN"], "replacement-secret");
}

#[tokio::test]
async fn a_second_pasted_account_is_added_beside_the_first_and_each_bot_uses_its_own() {
    let url = database_or_skip!();
    let (registry, _) = registry_fixture().await;
    let h = harness(&url, registry).await;
    let owner = h.person(None).await;
    let account = AccountId::from_stored(h.agui.auth.minter.verify_access(&owner).unwrap().sub);
    let install = Some(json!({"name":"demo","registryRevision":OLD}));
    let installed = h.call(&owner, "POST", "/fixture/plugins/installations", install);
    assert_eq!(installed.await.0, 201);
    let path = "/fixture/plugins/installations/demo/credentials/demo";

    // Adding always adds: two accounts, numbered, each with its own token.
    let (status, first, text) = h
        .call(&owner, "POST", path, Some(json!({"token":"first-secret"})))
        .await;
    assert_eq!(status, 201, "{text}");
    let (status, second, text) = h
        .call(&owner, "POST", path, Some(json!({"token":"second-secret"})))
        .await;
    assert_eq!(status, 201, "{text}");
    let (first, second) = (
        first["connectionId"].as_str().unwrap(),
        second["connectionId"].as_str().unwrap(),
    );
    assert_ne!(first, second);
    let (_, listed, _) = h.call(&owner, "GET", "/connections", None).await;
    let label = |id: &str| {
        listed
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == id)
            .unwrap()["label"]
            .clone()
    };
    assert_eq!(label(first), "Demo");
    assert_eq!(label(second), "Demo 2");
    assert_eq!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["kind"] == "token")
            .count(),
        2
    );

    // Replacing without naming one cannot tell which is meant; naming one replaces only it.
    let unnamed = Some(json!({"token":"which-one"}));
    assert_eq!(h.call(&owner, "PUT", path, unnamed).await.0, 409);
    let stranger = Some(json!({"token":"x","connectionId":"conn_somebody_else"}));
    assert_eq!(h.call(&owner, "PUT", path, stranger).await.0, 404);
    let named = Some(json!({"token":"second-renewed","connectionId":second}));
    assert_eq!(h.call(&owner, "PUT", path, named).await.0, 204);

    // The installs list says which pasted accounts are this install's, and for which service.
    let (status, installs, _) = h
        .call(&owner, "GET", "/fixture/plugins/installations", None)
        .await;
    assert_eq!(status, 200);
    assert_eq!(installs[0]["connectors"], json!(["demo"]));
    assert_eq!(
        installs[0]["accounts"],
        json!([{"connector":"demo","connectionId":first}, {"connector":"demo","connectionId":second}])
    );

    // With two and no pin, the Bot's turn gets neither and the person is asked (#360); pinned,
    // it gets that account's token and only that one.
    let bot = credential_bot(&h, &account).await;
    let install = opengrok_integrations::installed::list(&h.store, &account)
        .await
        .unwrap()
        .remove(0);
    let vault = h.agui.vault.as_ref().unwrap();
    let turn = opengrok_integrations::installed::values_for_installation(
        &h.store, vault, &account, &bot, &install,
    )
    .await
    .unwrap();
    assert!(turn.values.is_empty(), "{:?}", turn.values);
    assert_eq!(turn.needs_choice, vec!["demo".to_string()]);
    let pin = format!("/coworkers/{bot}/pins/demo");
    let (status, _, text) = h
        .call(&owner, "PUT", &pin, Some(json!({"connectionId":second})))
        .await;
    assert_eq!(status, 200, "{text}");
    let turn = opengrok_integrations::installed::values_for_installation(
        &h.store, vault, &account, &bot, &install,
    )
    .await
    .unwrap();
    assert_eq!(turn.values["DEMO_TOKEN"], "second-renewed");
    let (status, _, text) = h
        .call(&owner, "PUT", &pin, Some(json!({"connectionId":first})))
        .await;
    assert_eq!(status, 200, "{text}");
    let turn = opengrok_integrations::installed::values_for_installation(
        &h.store, vault, &account, &bot, &install,
    )
    .await
    .unwrap();
    assert_eq!(turn.values["DEMO_TOKEN"], "first-secret");

    // Uninstalling takes every pasted account with it, and their secrets out of the vault: the
    // dialog says the keys go, so the ciphertext must not outlive the accounts.
    let sealed: Vec<String> = sqlx::query_scalar(
        "select c.secret_id from plugin_credential c join connection_view v on v.id = c.connection_id
          where c.account_id = $1 and c.plugin_name = 'demo'",
    )
    .bind(account.as_str())
    .fetch_all(h.store.pool())
    .await
    .unwrap();
    assert_eq!(sealed.len(), 2, "{sealed:?}");
    let gone = h.call(
        &owner,
        "DELETE",
        "/fixture/plugins/installations/demo",
        None,
    );
    assert_eq!(gone.await.0, 204);
    let (_, listed, _) = h.call(&owner, "GET", "/connections", None).await;
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["kind"] != "token"),
        "{listed}"
    );
    let kept: i64 = sqlx::query_scalar("select count(*) from secret_store where id = any($1)")
        .bind(&sealed)
        .fetch_one(h.store.pool())
        .await
        .unwrap();
    assert_eq!(kept, 0, "an uninstalled plugin's keys stayed in the vault");
}

/// `@demo` in a message is the owner switching demo on for that turn (#359): its server is dialled
/// and its tools pass the gate although the Bot's switch is off, nothing is stored, and nobody but
/// the Bot's owner can widen it that way, not even with an install of their own by that name.
#[tokio::test]
async fn a_tagged_plugin_is_that_turns_alone_and_only_its_owners_tag_counts() {
    use opengrok_integrations::turn;
    let url = database_or_skip!();
    let (registry, _) = registry_fixture().await;
    let h = harness(&url, registry).await;
    let owner = h.person(None).await;
    let account = AccountId::from_stored(h.agui.auth.minter.verify_access(&owner).unwrap().sub);
    let install = Some(json!({"name":"demo","registryRevision":OLD}));
    let installed = h.call(&owner, "POST", "/fixture/plugins/installations", install);
    assert_eq!(installed.await.0, 201);
    let path = "/fixture/plugins/installations/demo/credentials/demo";
    let pasted = h.call(&owner, "POST", path, Some(json!({"token":"demo-secret"})));
    assert_eq!(pasted.await.0, 201);
    let bot = credential_bot(&h, &account).await;
    let (store, vault) = (&h.store, h.agui.vault.as_deref());
    let policy = store.policy_to_use(&account, &bot).await.unwrap();
    let no_operator = |_: &str| false;
    let tool = "demo.hosted.list_zones";
    let runs = |policy: &opengrok_policy::Context| {
        let action = opengrok_policy::Action::RunTool(tool);
        opengrok_policy::decide(&account, &bot, action, policy).is_allowed()
    };

    // Switched off: no server, no tool.
    assert!(!turn::switched_on(&account, &bot, "demo", &policy));
    let plain = turn::TurnPlugins::default();
    let off = turn::endpoints(store, vault, &account, &bot, &policy, &plain, no_operator).await;
    assert!(
        off.is_empty(),
        "{:?}",
        off.iter().map(|e| e.key()).collect::<Vec<_>>()
    );
    assert!(!runs(&policy));

    // The turn is told it is installed and off, so it asks for a tag rather than for a key; a
    // tagged turn is not told so.
    assert_eq!(
        turn::switched_off(store, &account, &bot, &[]).await,
        ["demo"]
    );
    let tagged_demo = ["demo".to_string()];
    let off = turn::switched_off(store, &account, &bot, &tagged_demo).await;
    assert!(off.is_empty(), "{off:?}");

    // Tagged by its owner: what is installed is kept, a name nobody installed is not.
    let asked = ["demo".to_string(), "nope".to_string()];
    let tagged = turn::mentioned(store, &account, &bot, &asked).await;
    assert_eq!(tagged, ["demo"]);
    let this_turn = opengrok_policy::with_mentioned(policy.clone(), &tagged);
    let tags = turn::TurnPlugins {
        mentioned: tagged.clone(),
        ..Default::default()
    };
    let on = turn::endpoints(store, vault, &account, &bot, &this_turn, &tags, no_operator).await;
    assert_eq!(
        on.iter().map(|e| e.key()).collect::<Vec<_>>(),
        ["demo.hosted"]
    );
    assert!(runs(&this_turn));

    // Nothing stored: the next turn reads the switch as it was.
    let next = store.policy_to_use(&account, &bot).await.unwrap();
    assert!(!turn::switched_on(&account, &bot, "demo", &next));
    assert!(!runs(&next));

    // On a Bot whose tools are "all", which never switches an install on, the tag still does.
    use opengrok_policy::ToolSet;
    let at = chrono::Utc::now().timestamp_millis();
    let (all, none) = (&ToolSet::All, &ToolSet::None);
    store
        .grant_access(&account, &bot, all, all, none, at)
        .await
        .unwrap();
    let everything = store.policy_to_use(&account, &bot).await.unwrap();
    let off = turn::endpoints(
        store,
        vault,
        &account,
        &bot,
        &everything,
        &plain,
        no_operator,
    )
    .await;
    assert!(off.is_empty());
    let this_turn = opengrok_policy::with_mentioned(everything, &tagged);
    let on = turn::endpoints(store, vault, &account, &bot, &this_turn, &tags, no_operator).await;
    assert_eq!(on.len(), 1);

    // Somebody else's tag on this Bot widens nothing, though they installed a demo of their own.
    let stranger = h.person(None).await;
    let theirs = AccountId::from_stored(h.agui.auth.minter.verify_access(&stranger).unwrap().sub);
    let install = Some(json!({"name":"demo","registryRevision":OLD}));
    let installed = h.call(&stranger, "POST", "/fixture/plugins/installations", install);
    assert_eq!(installed.await.0, 201);
    assert!(
        turn::mentioned(store, &theirs, &bot, &asked)
            .await
            .is_empty()
    );
    // Nor is anyone else's turn told of the owner's installs.
    let told = turn::switched_off(store, &theirs, &bot, &[]).await;
    assert!(told.is_empty(), "{told:?}");
}

/// A tagged plugin the Bot cannot use yet is asked about before any model call (#360): two
/// accounts and no pin answer with a "Which account?" card listing both, a name nobody installed
/// with an install card, and no model is asked. The card's pick, sent with the same message, is
/// used for that turn only; a stranger's tag on the Bot asks nothing.
#[tokio::test]
async fn a_tagged_plugin_with_two_accounts_asks_which_before_the_model_is_asked() {
    use opengrok_integrations::{installed, turn};
    let url = database_or_skip!();
    let (registry, _) = registry_fixture().await;
    let h = harness(&url, registry).await;
    let owner = h.person(None).await;
    let account = AccountId::from_stored(h.agui.auth.minter.verify_access(&owner).unwrap().sub);
    let install = Some(json!({"name":"demo","registryRevision":OLD}));
    let installed = h.call(&owner, "POST", "/fixture/plugins/installations", install);
    assert_eq!(installed.await.0, 201);
    let path = "/fixture/plugins/installations/demo/credentials/demo";
    let mut ids = Vec::new();
    for token in ["first-secret", "second-secret"] {
        let (status, added, text) = h
            .call(&owner, "POST", path, Some(json!({"token":token})))
            .await;
        assert_eq!(status, 201, "{text}");
        ids.push(added["connectionId"].as_str().unwrap().to_string());
    }
    let bot = credential_bot(&h, &account).await;
    let send = |token: String, props: Value| {
        let h = &h;
        async move {
            let body = json!({
                "threadId": format!("thr_{}", uuid::Uuid::now_v7()),
                "runId": uuid::Uuid::now_v7().to_string(),
                "messages": [{"id":"m1","role":"user","content":"list my zones"}],
                "forwardedProps": props,
            });
            let (status, _, sse) = h.call(&token, "POST", "/ag-ui", Some(body)).await;
            assert_eq!(status, 200, "{sse}");
            sse
        }
    };

    let tagged = json!({"coworkerId": bot.as_str(), "mentionedPlugins": ["demo", "nope"]});
    let sse = send(owner.clone(), tagged).await;
    assert!(sse.contains("RUN_FINISHED"), "{sse}");
    // No model: the stand-in door's first move is a tool call.
    assert!(!sse.contains("TOOL_CALL_START"), "{sse}");
    let frame = sse
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .find(|event| event["name"] == turn::PLUGIN_NEEDS)
        .unwrap_or_else(|| panic!("no needs card: {sse}"));
    let needs = frame["value"]["needs"].as_array().unwrap();
    assert_eq!(needs.len(), 2, "{needs:?}");
    assert_eq!(needs[0], json!({"plugin":"nope","need":"install"}));
    assert_eq!(needs[1]["need"], "choose");
    assert_eq!(
        (needs[1]["plugin"].as_str(), needs[1]["connector"].as_str()),
        (Some("demo"), Some("demo"))
    );
    let listed: Vec<&str> = needs[1]["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(listed, ids.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(!frame.to_string().contains("secret"), "{frame}");

    // The pick, for this turn: that account and only that one, and nothing more to ask.
    let picked = turn::TurnPlugins {
        mentioned: vec!["demo".into()],
        chosen: [(
            "demo".to_string(),
            [("demo".to_string(), ids[1].clone())].into(),
        )]
        .into(),
    };
    let (store, vault) = (&h.store, h.agui.vault.as_deref());
    let asked = ["demo".to_string()];
    assert!(
        turn::needs(store, vault, &account, &bot, &asked, &picked)
            .await
            .is_empty()
    );
    let install = installed::list(store, &account).await.unwrap().remove(0);
    let chosen = picked.chosen["demo"].clone();
    let values =
        installed::values_choosing(store, vault.unwrap(), &account, &bot, &install, &chosen);
    assert_eq!(values.await.unwrap().values["DEMO_TOKEN"], "second-secret");
    // Not stored: without the pick the Bot is still asked.
    let unpicked = turn::TurnPlugins {
        mentioned: vec!["demo".into()],
        ..Default::default()
    };
    assert_eq!(
        turn::needs(store, vault, &account, &bot, &asked, &unpicked)
            .await
            .len(),
        1
    );
    // A pick that is not one of the person's accounts picks nothing.
    let forged: BTreeMap<String, String> = [("demo".to_string(), "conn_nobody".to_string())].into();
    let values =
        installed::values_choosing(store, vault.unwrap(), &account, &bot, &install, &forged);
    assert!(values.await.unwrap().values.is_empty());

    // Somebody else tagging it on this Bot is asked nothing and given nothing.
    let stranger = h.person(None).await;
    let theirs = AccountId::from_stored(h.agui.auth.minter.verify_access(&stranger).unwrap().sub);
    assert!(
        turn::needs(store, vault, &theirs, &bot, &asked, &unpicked)
            .await
            .is_empty()
    );
}

/// A Bot manages its person's plugins in chat (#359) through the Plugins routes' own functions:
/// it installs (on for no Bot), lists, adds an account by card, renames, switches the plugin on for
/// itself, and pins. Uninstalling, removing an account, and anything aimed at another of the
/// person's Bots asks first, naming what it acts on as stored; a Bot that is not the person's is
/// refused like one that does not exist.
#[tokio::test]
async fn a_bot_manages_its_persons_plugins_and_asks_first_where_it_cannot_be_undone() {
    use opengrok_tools::plugin_desk::{Ask, Filter, PluginDesk};
    let url = database_or_skip!();
    let (registry, _) = registry_fixture().await;
    let h = harness(&url, registry.clone()).await;
    let owner = h.person(None).await;
    let account = AccountId::from_stored(h.agui.auth.minter.verify_access(&owner).unwrap().sub);
    let bot = credential_bot(&h, &account).await;
    let other = credential_bot(&h, &account).await;
    let desk = opengrok_server::plugin_desk::configured(&h.agui, Some(registry));
    let context = opengrok_tools::ToolContext {
        account_id: account.clone(),
        coworker_id: bot.clone(),
        box_id: None,
        group_box: None,
        screen_hold: false,
        screen_held_in: None,
        screen: Default::default(),
        thread_id: None,
        run_id: None,
    };
    let (desk, context) = (&desk, &context);
    let answer = move |ask: Ask| desk.answer(context, ask);
    let asks = move |ask: Ask| async move { desk.ask_first(context, &ask).await };
    let demo = || "demo".to_string();

    // Installed, on for nobody, and listed so.
    let installed = answer(Ask::Install { plugin: demo() }).await.unwrap();
    assert_eq!(installed["installed"], "demo");
    let again = answer(Ask::Install { plugin: demo() }).await;
    assert_eq!(again.unwrap_err(), "demo is already installed.");
    let list = Ask::List {
        filter: Filter::Installed,
        query: None,
        category: None,
    };
    let listed = answer(list).await.unwrap();
    assert_eq!(listed["plugins"][0]["name"], "demo");
    assert_eq!(listed["plugins"][0]["onForThisBot"], false);

    // Adding an account is a card for the person; the model is handed no secret and no field.
    let add = Ask::AddAccount {
        plugin: demo(),
        connector: None,
    };
    assert_eq!(asks(add.clone()).await.unwrap(), None);
    let card = answer(add).await.unwrap();
    assert_eq!(
        (card["plugin"].as_str(), card["connector"].as_str()),
        (Some("demo"), Some("demo"))
    );
    let path = "/fixture/plugins/installations/demo/credentials/demo";
    let (_, pasted, _) = h
        .call(&owner, "POST", path, Some(json!({"token":"demo-secret"})))
        .await;
    let id = pasted["connectionId"].as_str().unwrap().to_string();
    let rename = Ask::Rename {
        account: id.clone(),
        label: "Work".into(),
    };
    assert_eq!(answer(rename).await.unwrap()["label"], "Work");
    let rows = answer(Ask::Accounts { plugin: None }).await.unwrap();
    assert_eq!(rows["accounts"][0]["label"], "Work");
    assert!(!rows.to_string().contains("demo-secret"), "{rows}");

    // On for itself without asking; for another Bot only after asking, by its stored name.
    let mine = Ask::SetForBot {
        plugin: demo(),
        on: true,
        bot: None,
    };
    assert_eq!(asks(mine.clone()).await.unwrap(), None);
    assert_eq!(answer(mine).await.unwrap()["changed"], true);
    let policy = h.store.policy_to_use(&account, &bot).await.unwrap();
    assert!(opengrok_integrations::turn::switched_on(
        &account, &bot, "demo", &policy
    ));
    let theirs = Ask::SetForBot {
        plugin: demo(),
        on: true,
        bot: Some(other.to_string()),
    };
    let card = asks(theirs).await.unwrap().unwrap();
    assert_eq!(card, "Turn demo on for Credential bot?");
    let pick = Ask::Pick {
        plugin: demo(),
        connector: None,
        account: Some(id.clone()),
        bot: Some(other.to_string()),
    };
    let card = asks(pick).await.unwrap().unwrap();
    assert_eq!(card, "Make \"Work\" the demo account Credential bot uses?");
    let pick = Ask::Pick {
        plugin: demo(),
        connector: None,
        account: Some(id.clone()),
        bot: None,
    };
    assert_eq!(asks(pick.clone()).await.unwrap(), None);
    assert_eq!(answer(pick).await.unwrap()["account"], id.as_str());

    // A Bot that is not the person's is the same refusal as one that does not exist.
    let stranger = h.person(None).await;
    let theirs = AccountId::from_stored(h.agui.auth.minter.verify_access(&stranger).unwrap().sub);
    let foreign = credential_bot(&h, &theirs).await;
    let aimed = Ask::SetForBot {
        plugin: demo(),
        on: true,
        bot: Some(foreign.to_string()),
    };
    let refused = asks(aimed).await.unwrap_err();
    assert_eq!(
        refused,
        format!("no Bot called {foreign} is your person's.")
    );
    let nobody = Ask::SetForBot {
        plugin: demo(),
        on: true,
        bot: Some("cw_nobody".into()),
    };
    assert_eq!(
        asks(nobody).await.unwrap_err(),
        "no Bot called cw_nobody is your person's."
    );

    // What cannot be undone asks first, naming it as stored; then it happens.
    let remove = Ask::Remove {
        account: id.clone(),
    };
    let card = asks(remove.clone()).await.unwrap().unwrap();
    assert_eq!(
        card,
        "Remove the demo account \"Work\"? Credential bot will stop using it."
    );
    assert_eq!(
        answer(remove).await.unwrap()["botsThatLostIt"],
        json!(["Credential bot"])
    );
    let rows = answer(Ask::Accounts { plugin: None }).await.unwrap();
    assert_eq!(rows["accounts"], json!([]));
    let uninstall = Ask::Uninstall { plugin: demo() };
    assert_eq!(
        asks(uninstall.clone()).await.unwrap().unwrap(),
        "Uninstall demo?"
    );
    assert_eq!(answer(uninstall).await.unwrap()["uninstalled"], "demo");
    let gone = Ask::Uninstall { plugin: demo() };
    assert_eq!(
        asks(gone).await.unwrap_err(),
        "demo is not installed; call list_plugins."
    );
}

/// A stand-in MCP server and its authorization server, as the MCP authorization spec has them:
/// the MCP endpoint refuses with 401 naming its resource metadata, which names the authorization
/// server, whose metadata names where to register, consent and trade codes. The token endpoint
/// checks PKCE against the challenge the consent page carried, and the resource.
async fn stand_in_mcp_provider() -> (String, Arc<std::sync::Mutex<BTreeMap<String, String>>>) {
    use axum::{
        Form, Json, Router,
        http::StatusCode,
        response::IntoResponse,
        routing::{get, post},
    };
    use base64::Engine as _;
    use sha2::Digest as _;
    let seen: Arc<std::sync::Mutex<BTreeMap<String, String>>> = Default::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let (b1, b2, b3) = (base.clone(), base.clone(), base.clone());
    let (s1, s2) = (seen.clone(), seen.clone());
    let app = Router::new()
        .route("/mcp", post(move || {
            let b = b1.clone();
            async move {
                let hint = format!(r#"Bearer resource_metadata="{b}/.well-known/oauth-protected-resource/mcp""#);
                (StatusCode::UNAUTHORIZED, [("www-authenticate", hint)], "").into_response()
            }
        }))
        .route("/.well-known/oauth-protected-resource/mcp", get(move || {
            let b = b2.clone();
            async move { Json(json!({"resource": format!("{b}/mcp"), "authorization_servers": [b], "scopes_supported": ["mcp"]})) }
        }))
        .route("/.well-known/oauth-authorization-server", get(move || {
            let b = b3.clone();
            async move { Json(json!({"issuer": b, "authorization_endpoint": format!("{b}/authorize"),
                "token_endpoint": format!("{b}/token"), "registration_endpoint": format!("{b}/register"),
                "code_challenge_methods_supported": ["S256"]})) }
        }))
        .route("/register", post(move |Json(body): Json<Value>| {
            let seen = s1.clone();
            async move {
                let mut seen = seen.lock().unwrap();
                let n = seen.get("registrations").map_or(0, |n| n.parse::<u32>().unwrap()) + 1;
                seen.insert("registrations".into(), n.to_string());
                assert_eq!(body["token_endpoint_auth_method"], "none");
                Json(json!({"client_id": "client-1"}))
            }
        }))
        .route("/token", post(move |Form(form): Form<BTreeMap<String, String>>| {
            let seen = s2.clone();
            async move {
                let seen = seen.lock().unwrap();
                match form["grant_type"].as_str() {
                    "authorization_code" => {
                        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
                            .encode(sha2::Sha256::digest(form["code_verifier"].as_bytes()));
                        if Some(&challenge) != seen.get("challenge") || form["code"] != "code-1"
                            || form["client_id"] != "client-1" || !form["resource"].ends_with("/mcp")
                        {
                            return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant"}))).into_response();
                        }
                        Json(json!({"access_token": "at-1", "refresh_token": "rt-1", "expires_in": 1})).into_response()
                    }
                    "refresh_token" if form["refresh_token"] == "rt-1" => {
                        Json(json!({"access_token": "at-2", "expires_in": 3600})).into_response()
                    }
                    _ => (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant"}))).into_response(),
                }
            }
        }));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, seen)
}

#[tokio::test]
async fn a_plugin_account_is_signed_in_at_its_own_server_refreshed_and_used_by_its_bot() {
    use opengrok_integrations::{attempts, installed, mcp_oauth};
    let url = database_or_skip!();
    let (registry, _) = registry_fixture().await;
    let h = harness(&url, registry).await;
    let owner = h.person(None).await;
    let account = AccountId::from_stored(h.agui.auth.minter.verify_access(&owner).unwrap().sub);
    let install = Some(json!({"name":"demo","registryRevision":OLD}));
    assert_eq!(
        h.call(&owner, "POST", "/fixture/plugins/installations", install)
            .await
            .0,
        201
    );
    let (base, seen) = stand_in_mcp_provider().await;
    // Loopback is the stand-in's; deployments allow public HTTPS only (`mcp_oauth::PUBLIC`).
    let guard: mcp_oauth::Guard = |url| url.starts_with("http://127.0.0.1:");
    let http = reqwest::Client::new();
    let (store, vault) = (&h.store, h.agui.vault.as_deref().unwrap());
    let redirect = "https://og.example/connections/callback";

    // Discovery follows the server's own 401 to its authorization server.
    let metadata = mcp_oauth::discover(&http, guard, &format!("{base}/mcp"))
        .await
        .unwrap();
    assert_eq!(metadata.issuer, base);
    assert_eq!(metadata.token_endpoint, format!("{base}/token"));
    assert_eq!(metadata.scopes, ["mcp"]);
    // A public-only rule refuses a loopback server outright.
    assert!(matches!(
        mcp_oauth::discover(&http, mcp_oauth::PUBLIC, &format!("{base}/mcp")).await,
        Err(mcp_oauth::McpAuthError::NoSignIn(_))
    ));

    // Registered once, then the same client for every sign-in after.
    let client = mcp_oauth::client(&http, guard, store, vault, &metadata, redirect, 1)
        .await
        .unwrap();
    assert_eq!(client.client_id, "client-1");
    let again = mcp_oauth::client(&http, guard, store, vault, &metadata, redirect, 2)
        .await
        .unwrap();
    assert_eq!(again.client_id, "client-1");
    assert_eq!(seen.lock().unwrap()["registrations"], "1");

    // The consent page carries the challenge; the provider holds the code to it.
    let pkce = mcp_oauth::pkce();
    let waiting = attempts::start_mcp(
        store,
        &account,
        "demo",
        "demo",
        None,
        &pkce.verifier,
        &metadata,
        &client.client_id,
        3,
    )
    .await
    .unwrap();
    assert_eq!(waiting.plugin.as_deref(), Some("demo"));
    assert_eq!(waiting.status, "pending");
    let page = mcp_oauth::authorize_url(&metadata, &client, redirect, "state", &pkce.challenge);
    assert!(page.starts_with(&format!("{base}/authorize?")));
    seen.lock()
        .unwrap()
        .insert("challenge".into(), pkce.challenge.clone());
    let pending = attempts::mcp_pending(store, &account, &waiting.id, redirect)
        .await
        .unwrap()
        .unwrap();
    // A wrong code is the provider's refusal, never an account.
    assert!(
        mcp_oauth::exchange(&http, guard, store, vault, &pending, "wrong")
            .await
            .is_err()
    );
    let token = mcp_oauth::exchange(&http, guard, store, vault, &pending, "code-1")
        .await
        .unwrap();
    assert_eq!(token.access_token, "at-1");
    let id = mcp_oauth::connect(store, vault, &account, &pending, &token, 4)
        .await
        .unwrap()
        .unwrap();

    // An `mcp` account of the person's, under the label the sign-in had, never lent.
    let (_, listed, _) = h.call(&owner, "GET", "/connections", None).await;
    let row = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == id.as_str())
        .unwrap()
        .clone();
    assert_eq!(row["kind"], "mcp");
    assert_eq!(row["label"], "Demo");
    let bot = credential_bot(&h, &account).await;
    let lend = h
        .call(
            &owner,
            "POST",
            &format!("/connections/{id}/lend"),
            Some(json!({"coworker_id": bot.as_str()})),
        )
        .await;
    assert_ne!(lend.0, 200, "{}", lend.2);

    // The Bot's turn gets its token through the install's binding, as a pasted one's.
    let installation = installed::list(store, &account).await.unwrap().remove(0);
    let turn = installed::values_for_installation(store, vault, &account, &bot, &installation)
        .await
        .unwrap();
    assert_eq!(turn.values["DEMO_TOKEN"], "at-1");

    // Lapsing within the minute, it is refreshed before the turn reads it.
    mcp_oauth::refresh_due(&http, guard, store, vault, &account, "demo", 5).await;
    let turn = installed::values_for_installation(store, vault, &account, &bot, &installation)
        .await
        .unwrap();
    assert_eq!(turn.values["DEMO_TOKEN"], "at-2");

    // A reconnect refreshes the same account: still one, under the same id.
    let reconnect = mcp_oauth::Pending {
        target: Some(id.clone()),
        ..pending.clone()
    };
    let again = mcp_oauth::connect(store, vault, &account, &reconnect, &token, 6)
        .await
        .unwrap();
    assert_eq!(again.as_deref(), Some(id.as_str()));
    let (_, listed, _) = h.call(&owner, "GET", "/connections", None).await;
    assert_eq!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["kind"] == "mcp")
            .count(),
        1
    );

    // How an account is added: a stranger's or an unknown install is the same 404.
    let path = "/plugins/installations/nope/connectors/demo/sign-in";
    assert_eq!(h.call(&owner, "GET", path, None).await.0, 404);
    let path = "/plugins/installations/demo/connectors/demo/authorize?format=json&connection_id=conn_nobody";
    assert_eq!(h.call(&owner, "GET", path, None).await.0, 404);
}

/// A registry whose entries and bundles each break one rule `docs/plugin-registry.md` states.
async fn hostile_registry() -> opengrok_integrations::registry::Registry {
    use axum::{
        extract::Path,
        response::{IntoResponse, Response},
    };
    let app = axum::Router::new().route("/{*path}", axum::routing::get(|Path(path): Path<String>| async move {
        let json = |value: Value| -> Response { axum::Json(value).into_response() };
        let blob = |path: &str| json!({"path": path, "type": "blob", "mode": "100644"});
        match path.as_str() {
            "repos/fixture/hostile/commits/HEAD" => return json(json!({"sha": OLD})),
            // GitHub failing is an outage, never a verdict that the pin is a fork's.
            "repos/fixture/hostile/compare/dddddddddddddddddddddddddddddddddddddddd...aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" => {
                return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            "repos/fixture/hostile/git/trees/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" => {
                let mut tree = vec![
                    blob("plugins/good/plugin.json"),
                    blob("plugins/good/.mcp.json"),
                    blob("plugins/good/skills/ok/SKILL.md"),
                    blob("plugins/good/skills/ok/logo.png"),
                    blob("plugins/good/skills/ok/API Guide.md"),
                    json!({"path": "plugins/good/skills/ok/link", "type": "blob", "mode": "120000"}),
                    blob("plugins/liar/plugin.json"),
                    blob("plugins/huge/plugin.json"),
                    blob("plugins/huge/skills/x/big.txt"),
                    blob("plugins/many/plugin.json"),
                ];
                // The cap is on the files a plugin needs (a skill's reference files are best effort).
                tree.extend((0..129).map(|i| blob(&format!("plugins/many/skills/s{i}/SKILL.md"))));
                return json(json!({"truncated": false, "tree": tree}));
            }
            "repos/fixture/truncated/git/trees/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" => {
                return json(json!({"truncated": true, "tree": [blob("plugin.json")]}));
            }
            "repos/fixture/upstream/git/trees/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" => {
                return json(json!({"truncated": false, "tree": [blob("bundle/plugin.json")]}));
            }
            _ => {}
        }
        if path.ends_with("/.grok-plugin/marketplace.json") {
            return json(json!({"plugins": [
                {"name": "good", "source": "./plugins/good/"},
                {"name": "Bad.Name", "source": "./plugins/good"},
                {"name": "dup", "source": "./plugins/good"},
                {"name": "dup", "source": "./plugins/liar"},
                {"name": "escape", "source": "../outside"},
                {"name": "ext", "source": {"source": "url", "url": "https://github.com/fixture/upstream.git", "sha": OLD, "path": "./bundle"}},
                {"name": "trunc", "source": {"source": "url", "url": "https://github.com/fixture/truncated", "sha": OLD}},
                {"name": "liar", "source": "./plugins/liar"},
                {"name": "huge", "source": "./plugins/huge"},
                {"name": "many", "source": "./plugins/many"},
                {"name": "a-name-that-is-entirely-valid-in-its-characters-but-runs-past-sixty-four", "source": "./plugins/good"},
                {"description": "a row with no name is left out"}
            ]}));
        }
        if path.ends_with("plugins/good/plugin.json") { return json(json!({"name": "good"})); }
        if path.ends_with("bundle/plugin.json") { return json(json!({"name": "ext"})); }
        if path.ends_with("plugins/liar/plugin.json") { return json(json!({"name": "someone-else"})); }
        if path.ends_with("plugins/huge/plugin.json") { return json(json!({"name": "huge"})); }
        if path.ends_with("plugins/many/plugin.json") { return json(json!({"name": "many"})); }
        if path.ends_with("plugins/good/.mcp.json") {
            return json(json!({"mcpServers": {
                "fine": {"type": "http", "url": "https://mcp.example.com/mcp"},
                "insecure": {"type": "http", "url": "http://mcp.example.com/mcp"},
                "private": {"type": "http", "url": "https://10.0.0.1/mcp"},
                "loopback": {"type": "http", "url": "https://127.1/mcp"},
                "off": {"type": "http", "url": "https://mcp.example.com/off", "disabled": true},
                "needs-oauth": {"type": "http", "url": "https://mcp.example.com/o", "oauth": {}},
                "extra": {"type": "http", "url": "https://mcp.example.com/e", "timeout": 5}
            }}));
        }
        if path.ends_with("skills/ok/SKILL.md") { return "---\nname: ok\n---\nFine.".into_response(); }
        if path.ends_with("skills/ok/logo.png") { return vec![0x89u8, 0x50, 0xff, 0xfe].into_response(); }
        if path.ends_with("skills/x/big.txt") { return "x".repeat(2 * 1024 * 1024 + 1).into_response(); }
        axum::http::StatusCode::NOT_FOUND.into_response()
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    opengrok_integrations::registry::Registry::new(base.clone(), base, "fixture/hostile".into())
        .unwrap()
}

#[tokio::test]
async fn one_bad_entry_is_one_unavailable_entry_and_every_stated_refusal_holds() {
    use opengrok_integrations::registry::Error;
    let registry = hostile_registry().await;
    let catalog = registry.catalog(None).await.unwrap();
    let entry = |name: &str| {
        catalog
            .plugins
            .iter()
            .find(|e| e.name == name)
            .unwrap()
            .clone()
    };
    assert_eq!(
        catalog.plugins.len(),
        10,
        "the nameless row is left out, the rest kept"
    );
    // Judged whole, not cut to 64 first and then read as valid.
    let long = "a-name-that-is-entirely-valid-in-its-characters-but-runs-past-sixty-four";
    for (name, why) in [
        ("Bad.Name", "plugin name is not a valid tool prefix"),
        (long, "plugin name is not a valid tool prefix"),
        ("dup", "two registry entries share this name"),
        ("escape", "registry bundle path is unsafe"),
    ] {
        assert_eq!(
            entry(name).unavailable_reason.as_deref(),
            Some(why),
            "{name}"
        );
        assert!(matches!(
            registry.bundle(&entry(name)).await,
            Err(Error::Refused(_))
        ));
    }
    assert_eq!(entry("good").path, "plugins/good");
    assert_eq!(entry("ext").path, "bundle");
    assert_eq!(
        registry.bundle(&entry("ext")).await.unwrap().manifest.name,
        "ext"
    );

    let good = registry.bundle(&entry("good")).await.unwrap();
    // The same source, unavailable at another registry commit: the cached bundle does not answer.
    let mut pulled = entry("good");
    pulled.unavailable_reason = Some("two registry entries share this name".into());
    assert!(matches!(
        registry.bundle(&pulled).await,
        Err(Error::Refused(_))
    ));
    let reason = |kind: &str, name: &str| {
        let part = good.parts.iter().find(|p| p.kind == kind && p.name == name);
        part.and_then(|p| p.reason.clone()).unwrap_or_default()
    };
    assert_eq!(
        good.mcp.servers.keys().collect::<Vec<_>>(),
        ["fine", "needs-oauth"]
    );
    assert_eq!(reason("mcp", "insecure"), "remote MCP requires HTTPS");
    assert_eq!(
        reason("mcp", "private"),
        "remote MCP must name a public host"
    );
    assert_eq!(
        reason("mcp", "loopback"),
        "remote MCP must name a public host"
    );
    assert_eq!(reason("mcp", "off"), "its author switched this server off");
    // A server that signs its people in is kept: this server signs them in itself (#364).
    assert_eq!(reason("mcp", "needs-oauth"), "");
    assert!(good.mcp.servers.contains_key("needs-oauth"));
    assert_eq!(
        reason("mcp", "extra"),
        "MCP field `timeout` is not supported"
    );
    // One PNG, one odd name and one symlink are three skipped files, not a failed install.
    assert_eq!(good.skills.len(), 1);
    assert_eq!(
        reason("file", "skills/ok/logo.png"),
        "not a UTF-8 text file"
    );
    assert_eq!(
        reason("file", "skills/ok/API Guide.md"),
        "file name is not a plain path"
    );
    assert_eq!(
        reason("file", "skills/ok/link"),
        "symlinks are not followed"
    );

    for (name, why) in [
        ("trunc", "repository tree is truncated"),
        ("liar", "bundle name disagrees with registry"),
        ("huge", "registry file exceeds 2 MiB"),
        ("many", "bundle exceeds 128 files"),
    ] {
        match registry.bundle(&entry(name)).await {
            Err(Error::Refused(said)) => assert_eq!(said, why, "{name}"),
            other => panic!("{name}: {:?}", other.map(|b| b.parts)),
        }
    }
    // A malformed pin is the request's fault; one off the default branch is refused, not served.
    assert!(matches!(
        registry.catalog(Some("main")).await,
        Err(Error::Request(_))
    ));
    assert!(matches!(
        registry.catalog(Some(FORK)).await,
        Err(Error::Refused(_))
    ));
    let outage = "dddddddddddddddddddddddddddddddddddddddd";
    assert!(matches!(
        registry.catalog(Some(outage)).await,
        Err(Error::Upstream(_))
    ));
}

/// Installs and uninstalls by one account at once never deadlock. Each takes the plugin out of the
/// ceiling and grants of EVERY Bot the account owns, locking all of those rows; taken in whatever
/// order a scan met them, two such transactions could each hold a row the other waited on, and
/// Postgres killed one, which the person saw as a 503. Every Bot is switched back on between
/// rounds, so each pass really rewrites rows and moves them in the heap, as real traffic does.
///
/// A RACE, SO IT CATCHES THE BUG OFTEN, NOT ALWAYS: with the `order by` taken out of
/// `installed::switch_off` it hit "deadlock detected" in about half its runs here, and with it in
/// place it never did. A green run alone does not prove the order; a red one always means a cycle.
#[tokio::test]
async fn installs_by_one_account_at_once_never_deadlock() {
    let url = database_or_skip!();
    let (registry, _) = registry_fixture().await;
    let h = harness(&url, registry).await;
    let owner = h.person(None).await;
    let account = AccountId::from_stored(h.agui.auth.minter.verify_access(&owner).unwrap().sub);
    let mut bots = Vec::new();
    for i in 0..8 {
        let hire = json!({"name": format!("Busy bot {i}")});
        let (status, hired, text) = h.call(&owner, "POST", "/coworkers", Some(hire)).await;
        assert_eq!(status, 201, "{text}");
        bots.push(opengrok_core::id::CoworkerId::from_stored(
            hired["id"].as_str().unwrap(),
        ));
    }
    let plugins = [
        "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel",
    ];
    let rounds = 12;
    let work = plugins.map(|plugin| {
        let (store, account, bots) = (h.store.clone(), account.clone(), bots.clone());
        tokio::spawn(async move {
            let files: BTreeMap<String, String> = [(
                "plugin.json".to_string(),
                json!({ "name": plugin }).to_string(),
            )]
            .into();
            let bundle = opengrok_plugins::bundle::Bundle::from_files(&files).unwrap();
            let catalog = opengrok_integrations::registry::Catalog {
                registry: "fixture/marketplace".into(),
                revision: OLD.into(),
                plugins: Vec::new(),
            };
            let entry = opengrok_integrations::registry::Entry {
                name: plugin.into(),
                description: String::new(),
                category: None,
                homepage: None,
                repository: "fixture/marketplace".into(),
                revision: OLD.into(),
                path: String::new(),
                unavailable_reason: None,
            };
            let on = opengrok_policy::ToolSet::only([format!("{plugin}.*"), "shell".into()]);
            for round in 0..rounds {
                let at = round + 1;
                // Switched on everywhere, one Bot at a time, as a ceiling screen saves it.
                for bot in &bots {
                    store
                        .set_ceiling(&account, bot, &on, None, at)
                        .await
                        .unwrap();
                }
                let installed = opengrok_integrations::installed::install(
                    &store, &account, &catalog, &entry, &bundle, at,
                )
                .await;
                assert!(installed.is_ok(), "{plugin} round {round}: {installed:?}");
                let removed =
                    opengrok_integrations::installed::uninstall(&store, &account, plugin, at).await;
                assert!(
                    matches!(removed, Ok(true)),
                    "{plugin} round {round}: {removed:?}"
                );
            }
        })
    });
    for task in work {
        task.await
            .expect("no task panicked: no install or uninstall was killed as a deadlock");
    }
}
