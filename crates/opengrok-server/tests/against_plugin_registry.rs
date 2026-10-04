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
    let app = opengrok_server::router(agui.clone(), gateway).nest(
        "/fixture",
        opengrok_server::plugin_registry::router_with_registry(agui.clone(), Some(registry)),
    );
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
        &installation,
    )
    .await
    .unwrap()
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
                return json(json!({"name":"fixture", "plugins":[{"name":"demo","description":"Demo","source":{"type":"local","path":"./plugins/demo"}}, {"name":"external","source":{"source":"url","url":"https://github.com/fixture/upstream.git","sha":OLD,"path":"bundle"}}]}));
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
    let (status, detail, _) = h
        .call(
            &a,
            "GET",
            &format!("{catalog_path}/demo?revision={OLD}"),
            None,
        )
        .await;
    assert_eq!(status, 200, "{detail}");
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
    let old_values = opengrok_integrations::installed::values_for_installation(
        &h.store,
        h.agui.vault.as_ref().unwrap(),
        &account,
        &snapshot,
    )
    .await
    .unwrap();
    assert!(
        old_values.is_empty(),
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
        &current,
    )
    .await
    .unwrap();
    assert_eq!(current_values["DEMO_TOKEN"], "replacement-secret");
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
                tree.extend((0..129).map(|i| blob(&format!("plugins/many/skills/s/{i}.md"))));
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
    assert_eq!(good.mcp.servers.keys().collect::<Vec<_>>(), ["fine"]);
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
    assert!(reason("mcp", "needs-oauth").contains("OAuth"));
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
