//! The person's standing answer, per computer, to the egress tunnel's card — and the two things
//! it was built on top of.
//!
//! FIRST, the box host-settings looks at. `egressTunnelAvailable` once probed the id frozen on
//! the coworker's row with the boot-time provider, while the Computer pane and a turn's tools
//! resolved the box through the account's sharing scope. After an update or a takeover the two
//! ids differ (four of the dev account's coworkers carried a dead container's id, 21 Sep 2026),
//! and host-settings said the tunnel was off for a box whose guest said it was on — so the app
//! painted the tunnel card without its chrome. Now every door finds the same box.
//!
//! SECOND, the standing choice: `bypass` raises no card, `ask` raises one per run, `never` takes
//! the browser tools off the offer while the tunnel is on and leaves them alone when it is off.
//!
//! THIRD, the bug #160 shipped: a resumed run was marked consented whatever the answer, so a
//! Deny on the tunnel card let the model's next screen action through with no card at all.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream;
use opengrok_box::{
    BoxResult, CommandOutput, Computer, CuaAction, EgressTunnel, Screenshot, StartedCommand,
};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, OrgId};
use opengrok_core::org::{Org, OrgCommand};
use opengrok_core::run::RunStatus;
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

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

const ONE_PIXEL_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

/// A box with a screen whose guest says the tunnel is attached, that records what touched it.
/// When `only_for` names a box, only that box's guest says so: the frozen-id test needs the
/// two ids to answer differently, or it cannot tell which one was asked.
#[derive(Default)]
struct TunnelBox {
    touched: Mutex<Vec<String>>,
    only_for: Mutex<Option<String>>,
}

impl TunnelBox {
    fn touched(&self) -> Vec<String> {
        self.touched.lock().expect("touched").clone()
    }
    fn note(&self, what: &str) {
        self.touched.lock().expect("touched").push(what.to_string());
    }
}

#[async_trait]
impl Computer for TunnelBox {
    async fn create(&self, _ttl_seconds: Option<u64>) -> BoxResult<String> {
        Ok(format!("bx_tunnel_{}", uuid::Uuid::now_v7().simple()))
    }
    async fn run(
        &self,
        _box_id: &str,
        command: &str,
        _timeout_seconds: u32,
    ) -> BoxResult<CommandOutput> {
        self.note(&format!("run {command}"));
        Ok(CommandOutput {
            exit_code: 0,
            stdout: "ran".to_string(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            timed_out: false,
        })
    }
    async fn start(&self, _box_id: &str, command: &str) -> BoxResult<StartedCommand> {
        self.note(&format!("start {command}"));
        Ok(StartedCommand {
            process_id: "p1".to_string(),
            running: false,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
        })
    }
    async fn watch(&self, _box_id: &str, _process_id: &str) -> BoxResult<StartedCommand> {
        Ok(StartedCommand {
            process_id: "p1".to_string(),
            running: false,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
        })
    }
    async fn read_file(&self, _box_id: &str, _path: &str) -> BoxResult<String> {
        Ok(String::new())
    }
    async fn write_file(&self, _box_id: &str, _path: &str, _content: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn expose_port(&self, _box_id: &str, _port: u16, _title: &str) -> BoxResult<String> {
        Ok("http://stub.invalid".to_string())
    }
    async fn stop(&self, _box_id: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn resume(&self, _box_id: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn destroy(&self, _box_id: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn state(&self, _box_id: &str) -> BoxResult<String> {
        Ok("running".to_string())
    }
    async fn offers_a_screen(&self, _box_id: &str) -> bool {
        true
    }
    async fn screenshot(&self, _box_id: &str) -> BoxResult<Screenshot> {
        self.note("screenshot");
        Ok(Screenshot {
            mime: "image/png".to_string(),
            png_base64: ONE_PIXEL_PNG.to_string(),
            width: 1,
            height: 1,
        })
    }
    async fn act(&self, _box_id: &str, _action: &CuaAction) -> BoxResult<()> {
        self.note("act");
        Ok(())
    }
    async fn egress_tunnel(&self, box_id: &str) -> Option<EgressTunnel> {
        let only = self.only_for.lock().expect("only_for").clone();
        if only.as_deref().is_some_and(|live| live != box_id) {
            return None;
        }
        Some(EgressTunnel {
            enabled: true,
            ready: true,
        })
    }
}

/// A model that takes a screenshot every time it is asked, with a FRESH call id each round, and
/// stops after three rounds. The fixed-id mock cannot drive this: an answered call id is carried
/// on the resumed runner as its yes, so a second call with the same id would be let through by
/// that yes rather than by the consent this file is about.
struct LookingModel {
    rounds: AtomicUsize,
}

#[async_trait]
impl ModelDoor for LookingModel {
    async fn stream(&self, _request: ModelRequest) -> Result<DeltaStream, ModelError> {
        let round = self.rounds.fetch_add(1, Ordering::SeqCst) + 1;
        let script = if round > 3 {
            vec![ModelDelta::Text("that is all I needed".to_string())]
        } else {
            let id = format!("look-{round}");
            vec![
                ModelDelta::Text(format!("looking, round {round}")),
                ModelDelta::ToolCallStart {
                    id: id.clone(),
                    name: "computer".to_string(),
                },
                ModelDelta::ToolCallArgs {
                    id: id.clone(),
                    delta: r#"{"action":"screenshot"}"#.to_string(),
                },
                ModelDelta::ToolCallEnd { id },
            ]
        };
        Ok(Box::pin(stream::iter(script.into_iter().map(Ok))))
    }
}

async fn seed_account(store: &PgStore, email: &str) -> AccountId {
    seed_member(store, email, None).await
}

/// An account, in an org when one is named.
async fn seed_member(store: &PgStore, email: &str, org: Option<&OrgId>) -> AccountId {
    let id = AccountId::new();
    let at_ms = now_ms();
    let org_id = org.map(|org| org.to_string());
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.to_string(),
            password_hash: "x".to_string(),
            first_name: "Host".to_string(),
            last_name: String::new(),
            org_id: org_id.clone().unwrap_or_default(),
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
        org_id,
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

/// An org whose admin is `admin`, sharing one computer among its members.
async fn seed_org_sharing_one_box(store: &PgStore, org: &OrgId, admin: &AccountId, email: &str) {
    let at_ms = now_ms();
    let domain = email.rsplit('@').next().expect("domain").to_string();
    let events = Org::default()
        .decide(OrgCommand::Create {
            name: format!("org of {email}"),
            admin: admin.clone(),
            domains: vec![domain],
            at_ms,
        })
        .expect("create org");
    let state = Org::replay(&events);
    store
        .append_org(org, 0, &events, &state, at_ms)
        .await
        .expect("append org");
    store
        .set_sharing_mode("org", org.as_str(), "per-org", at_ms)
        .await
        .expect("per-org");
}

struct Harness {
    base: String,
    agui: AgUiState,
    store: PgStore,
    account: AccountId,
    tunnel_box: Arc<TunnelBox>,
    model: Arc<LookingModel>,
    client: reqwest::Client,
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
    let tunnel_box = Arc::new(TunnelBox::default());
    let model = Arc::new(LookingModel {
        rounds: AtomicUsize::new(0),
    });
    let auth = AuthState::new(
        store.clone(),
        Arc::new(TokenMinter::new(b"egress-policy-secret")),
        email.to_string(),
    );
    let agui = AgUiState {
        auth,
        door: model.clone(),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer: Some(tunnel_box.clone()),
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
        agui,
        store,
        account,
        tunnel_box,
        model,
        client: reqwest::Client::new(),
    }
}

impl Harness {
    fn access_token(&self, email: &str) -> String {
        self.token_for(&self.account, email)
    }

    fn token_for(&self, account: &AccountId, email: &str) -> String {
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

    async fn hire(&self, token: &str, name: &str) -> String {
        let hired: Value = self
            .client
            .post(format!("{}/coworkers", self.base))
            .header("authorization", format!("Bearer {token}"))
            .json(&json!({ "name": name }))
            .send()
            .await
            .expect("hire")
            .json()
            .await
            .expect("hire json");
        hired["id"].as_str().expect("coworker id").to_string()
    }

    async fn get(&self, token: &str, path: &str) -> (u16, Value) {
        let res = self
            .client
            .get(format!("{}{path}", self.base))
            .header("authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("get");
        let status = res.status().as_u16();
        let text = res.text().await.expect("body");
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn put(&self, token: &str, path: &str, body: &Value) -> (u16, Value) {
        let res = self
            .client
            .put(format!("{}{path}", self.base))
            .header("authorization", format!("Bearer {token}"))
            .json(body)
            .send()
            .await
            .expect("put");
        let status = res.status().as_u16();
        let text = res.text().await.expect("body");
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn route_traffic(&self, token: &str, on: bool) {
        let (status, _) = self
            .put(
                token,
                "/ag-ui/host-settings",
                &json!({ "egressTunnelEnabled": on }),
            )
            .await;
        assert_eq!(status, 200, "host-settings patch");
    }

    async fn tool_names(&self, token: &str, agent: &str) -> Vec<String> {
        let (status, body) = self.get(token, &format!("/coworkers/{agent}/tools")).await;
        assert_eq!(status, 200, "{body}");
        body["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .filter_map(|tool| tool["name"].as_str().map(str::to_string))
            .collect()
    }

    async fn turn(&self, token: &str, agent: &str, prompt: &str) -> String {
        let res = self
            .client
            .post(format!("{}/ag-ui", self.base))
            .header("authorization", format!("Bearer {token}"))
            .json(&json!({
                "threadId": format!("thr-{}", uuid::Uuid::now_v7()),
                "runId": uuid::Uuid::now_v7().to_string(),
                "messages": [{ "id": "m1", "role": "user", "content": prompt }],
                "forwardedProps": { "coworkerId": agent },
            }))
            .send()
            .await
            .expect("ag-ui turn");
        assert_eq!(res.status().as_u16(), 200, "ag-ui turn status");
        res.text().await.expect("sse")
    }

    /// The run this person is waiting on and the call its card is for — one whose call id is
    /// not `after`, so a second card on the same run is told apart from the first.
    async fn wait_for_pending(&self, after: Option<&str>) -> (opengrok_core::id::RunId, String) {
        for _ in 0..100 {
            for id in self
                .store
                .awaiting_approval(&self.account)
                .await
                .expect("awaiting")
            {
                if let Ok((run, _)) = self.store.load_run(&id).await
                    && let Some(pending) = run.pending.as_ref()
                    && after != Some(pending.call_id.as_str())
                {
                    return (id, pending.call_id.clone());
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("no run suspended for an approval in 10s");
    }

    async fn answer(&self, token: &str, run_id: &str, call_id: &str, approved: bool) -> Value {
        self.client
            .post(format!("{}/ag-ui/runs/{run_id}/answer", self.base))
            .header("authorization", format!("Bearer {token}"))
            .json(&json!({ "call_id": call_id, "approved": approved }))
            .send()
            .await
            .expect("answer")
            .json()
            .await
            .expect("answer json")
    }

    async fn wait_for_ending(&self, run_id: &opengrok_core::id::RunId) -> opengrok_core::run::Run {
        for _ in 0..100 {
            let (run, _) = self.store.load_run(run_id).await.expect("run");
            if run.status.is_terminal() {
                return run;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("the run never ended");
    }
}

const BROWSER_TOOLS: [&str; 4] = [
    "computer",
    "open_url",
    "request_user_form",
    "credential.request",
];

fn has(names: &[String], tool: &str) -> bool {
    names
        .iter()
        .any(|name| name == tool || name == &opengrok_tools::openai_safe_tool_name(tool))
}

/// Host-settings follows the scope's live box, not the id frozen on the coworker's row.
#[tokio::test]
async fn host_settings_asks_the_scoped_box_not_the_frozen_id() {
    let database_url = database_or_skip!();
    let email = format!("egress-scoped-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Vamos").await;

    // The account's box is replaced under the coworker (an update, a heal, a takeover): the
    // scoped row moves on, the coworker's row keeps the old id.
    let (frozen, _) = h
        .store
        .load_coworker(&opengrok_core::id::CoworkerId::from_stored(agent.clone()))
        .await
        .expect("coworker");
    let frozen_id = frozen
        .computer()
        .expect("hired with a computer")
        .as_str()
        .to_string();
    let live_id = format!("bx_live_{}", uuid::Uuid::now_v7().simple());
    h.store
        .set_scoped_computer(
            "account",
            h.account.as_str(),
            &live_id,
            "local-docker",
            None,
            now_ms(),
        )
        .await
        .expect("move the scoped box");
    assert_ne!(frozen_id, live_id);
    // Only the live box's guest advertises a tunnel; a probe of the frozen id gets nothing.
    *h.tunnel_box.only_for.lock().expect("only_for") = Some(live_id.clone());

    h.route_traffic(&token, true).await;
    let (status, settings) = h
        .get(&token, &format!("/ag-ui/host-settings?coworker={agent}"))
        .await;
    assert_eq!(status, 200, "{settings}");
    assert_eq!(
        settings["egressTunnelAvailable"],
        json!(true),
        "the live box's guest says the tunnel is there; the frozen id must not be consulted: {settings}"
    );
    let (_, computer) = h.get(&token, &format!("/coworkers/{agent}/computer")).await;
    assert_eq!(computer["boxId"], json!(live_id), "{computer}");
    assert_eq!(
        computer["isEgressTunnelAvailable"],
        json!(true),
        "{computer}"
    );
    assert_eq!(
        computer["egressPolicy"],
        json!("ask"),
        "no choice yet reads as ask: {computer}"
    );
    assert_eq!(computer["shareScope"], json!("user"), "{computer}");

    h.route_traffic(&token, false).await;
    let (_, settings) = h
        .get(&token, &format!("/ag-ui/host-settings?coworker={agent}"))
        .await;
    assert_eq!(
        settings["egressTunnelAvailable"],
        json!(false),
        "host intent first: {settings}"
    );
}

/// The choice reads back, is validated, and is this account's alone.
#[tokio::test]
async fn the_policy_is_kept_per_computer_and_refused_for_strangers() {
    let database_url = database_or_skip!();
    let email = format!("egress-policy-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Ada").await;
    let path = format!("/coworkers/{agent}/computer/egress-policy");

    let (status, policy) = h.get(&token, &path).await;
    assert_eq!(status, 200, "{policy}");
    assert_eq!(policy["mode"], json!("ask"), "{policy}");
    assert_eq!(policy["scope"], json!("user"), "{policy}");
    assert_eq!(policy["scopeId"], json!(h.account.as_str()), "{policy}");

    let (status, _) = h.put(&token, &path, &json!({ "mode": "never" })).await;
    assert_eq!(status, 204);
    let (_, policy) = h.get(&token, &path).await;
    assert_eq!(policy["mode"], json!("never"), "{policy}");
    let (_, computer) = h.get(&token, &format!("/coworkers/{agent}/computer")).await;
    assert_eq!(
        computer["egressPolicy"],
        json!("never"),
        "the pane carries it: {computer}"
    );

    // The same box, a second coworker: the choice is the computer's, not the coworker's.
    let sibling = h.hire(&token, "Bob").await;
    let (_, sibling_policy) = h
        .get(
            &token,
            &format!("/coworkers/{sibling}/computer/egress-policy"),
        )
        .await;
    assert_eq!(sibling_policy["mode"], json!("never"), "{sibling_policy}");

    let (status, body) = h.put(&token, &path, &json!({ "mode": "sometimes" })).await;
    assert_eq!(status, 422, "{body}");
    let (_, policy) = h.get(&token, &path).await;
    assert_eq!(
        policy["mode"],
        json!("never"),
        "a refused word changes nothing: {policy}"
    );

    let (status, _) = h
        .get(&token, "/coworkers/cw_nobody/computer/egress-policy")
        .await;
    assert_eq!(status, 404);
    // A different account: its coworker list does not name this one.
    let stranger_email = format!("egress-stranger-{}@og.local", uuid::Uuid::now_v7().simple());
    let other = seed_account(&h.store, &stranger_email).await;
    let other_token = h
        .agui
        .auth
        .minter
        .mint_access(
            other.as_str(),
            "sess-other",
            &stranger_email,
            "ultra",
            chrono::Utc::now().timestamp(),
            3600,
        )
        .expect("mint");
    let (status, _) = h
        .put(&other_token, &path, &json!({ "mode": "bypass" }))
        .await;
    assert_eq!(
        status, 404,
        "another account's coworker reads as no such coworker"
    );
    let (status, _) = h.get(&other_token, &path).await;
    assert_eq!(status, 404);
}

/// `never` takes the browser tools off the offer while the tunnel is on, and only then.
#[tokio::test]
async fn never_withholds_the_browser_tools_only_while_the_tunnel_is_on() {
    let database_url = database_or_skip!();
    let email = format!("egress-never-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Ada").await;
    let path = format!("/coworkers/{agent}/computer/egress-policy");

    let offered = h.tool_names(&token, &agent).await;
    for tool in BROWSER_TOOLS {
        assert!(
            has(&offered, tool),
            "{tool} is offered on a box with a screen: {offered:?}"
        );
    }

    let (status, _) = h.put(&token, &path, &json!({ "mode": "never" })).await;
    assert_eq!(status, 204);
    let offered = h.tool_names(&token, &agent).await;
    for tool in BROWSER_TOOLS {
        assert!(
            has(&offered, tool),
            "the tunnel is off, so never means nothing yet: {offered:?}"
        );
    }

    h.route_traffic(&token, true).await;
    let offered = h.tool_names(&token, &agent).await;
    for tool in BROWSER_TOOLS {
        assert!(!has(&offered, tool), "{tool} must be withheld: {offered:?}");
    }
    assert!(
        has(&offered, "shell"),
        "the box's own shell is not the person's network: {offered:?}"
    );

    // And a turn under never never reaches the box, nor raises a card: the model is not offered
    // the tool, and one it calls anyway is refused in words.
    let sse = h.turn(&token, &agent, "look at the screen").await;
    assert!(
        !sse.contains("run-awaiting-approval"),
        "no card under never: {sse}"
    );
    assert!(
        sse.contains("switched off"),
        "the refusal names the reason: {sse}"
    );
    assert!(
        h.tunnel_box.touched().is_empty(),
        "{:?}",
        h.tunnel_box.touched()
    );

    let (status, _) = h.put(&token, &path, &json!({ "mode": "bypass" })).await;
    assert_eq!(status, 204);
    let offered = h.tool_names(&token, &agent).await;
    for tool in BROWSER_TOOLS {
        assert!(
            has(&offered, tool),
            "{tool} is back under bypass: {offered:?}"
        );
    }
}

/// `bypass` raises no card; `ask` raises one, whose Deny does NOT consent the rest of the run.
#[tokio::test]
async fn bypass_skips_the_card_and_a_deny_does_not_consent_the_run() {
    let database_url = database_or_skip!();
    let email = format!("egress-deny-{}@og.local", uuid::Uuid::now_v7().simple());
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "Ada").await;
    let path = format!("/coworkers/{agent}/computer/egress-policy");
    h.route_traffic(&token, true).await;

    // bypass: the model looks three times and nobody is asked.
    let (status, _) = h.put(&token, &path, &json!({ "mode": "bypass" })).await;
    assert_eq!(status, 204);
    let sse = h.turn(&token, &agent, "look at the screen").await;
    assert!(
        !sse.contains("run-awaiting-approval"),
        "no card under bypass: {sse}"
    );
    assert_eq!(
        h.tunnel_box.touched().len(),
        3,
        "{:?}",
        h.tunnel_box.touched()
    );

    // ask: the first look raises the card. A DENY is carried to the model, which looks again —
    // and that look must raise a card of its own, not slip through on a consent that was
    // never given (the #160 bug).
    h.model.rounds.store(0, Ordering::SeqCst);
    let (status, _) = h.put(&token, &path, &json!({ "mode": "ask" })).await;
    assert_eq!(status, 204);
    let sse = h.turn(&token, &agent, "look at the screen").await;
    assert!(
        sse.contains("run-awaiting-approval"),
        "ask raises the card: {sse}"
    );
    let (run_id, first_call) = h.wait_for_pending(None).await;
    // The queue carries the ask's own sentence, so a card rebuilt from it (the app after a
    // relaunch) is still the tunnel's card and not a judge's.
    let (status, queue) = h.get(&token, "/ag-ui/approvals").await;
    assert_eq!(status, 200, "{queue}");
    let item = queue
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .find(|item| item["callId"].as_str() == Some(first_call.as_str()))
        })
        .cloned()
        .expect("the waiting call is on the queue");
    assert_eq!(item["reason"], json!("auto-review"), "{item}");
    assert!(
        item["why"]
            .as_str()
            .is_some_and(|why| why.contains("egress tunnel")),
        "the queue carries the tunnel's own sentence: {item}"
    );
    let answered = h.answer(&token, run_id.as_str(), &first_call, false).await;
    assert_eq!(answered["approved"], false, "{answered}");
    let (again, second_call) = h.wait_for_pending(Some(&first_call)).await;
    assert_eq!(again, run_id, "the same run asks again");
    assert_ne!(second_call, first_call);
    assert_eq!(
        h.tunnel_box.touched().len(),
        3,
        "a denied look never reached the box"
    );

    // A YES consents the rest of the run: the third look runs without a card, and the run ends.
    let answered = h.answer(&token, run_id.as_str(), &second_call, true).await;
    assert_eq!(answered["approved"], true, "{answered}");
    let run = h.wait_for_ending(&run_id).await;
    assert_eq!(run.status, RunStatus::Finished, "{:?}", run.status);
    assert!(run.pending.is_none(), "{:?}", run.pending);
    assert_eq!(
        h.tunnel_box.touched().len(),
        5,
        "the approved look and the one after it ran: {:?}",
        h.tunnel_box.touched()
    );
}

/// An org-shared computer's standing consent is the org admin's to set: a member may read it
/// and may not write it, because a `bypass` there removes the card for every member's runs.
#[tokio::test]
async fn an_org_shared_box_takes_its_choice_from_the_admin_only() {
    let database_url = database_or_skip!();
    let stamp = uuid::Uuid::now_v7().simple();
    let admin_email = format!("egress-org-admin-{stamp}@og.local");
    let h = harness(&database_url, &format!("egress-org-host-{stamp}@og.local")).await;
    // An admin and a member, both registered into one org that shares one computer.
    let org = OrgId::new();
    let store = h.store.clone();
    let admin = seed_member(&store, &admin_email, Some(&org)).await;
    seed_org_sharing_one_box(&store, &org, &admin, &admin_email).await;
    let member_email = format!("egress-org-member-{stamp}@og.local");
    let member = seed_member(&store, &member_email, Some(&org)).await;

    let admin_token = h.token_for(&admin, &admin_email);
    let member_token = h.token_for(&member, &member_email);
    let admin_bot = h.hire(&admin_token, "Admin's bot").await;
    let member_bot = h.hire(&member_token, "Member's bot").await;
    let admin_path = format!("/coworkers/{admin_bot}/computer/egress-policy");
    let member_path = format!("/coworkers/{member_bot}/computer/egress-policy");

    // One box, the org's: both bots resolve to the same scope.
    let (_, seen_by_admin) = h.get(&admin_token, &admin_path).await;
    let (_, seen_by_member) = h.get(&member_token, &member_path).await;
    assert_eq!(seen_by_admin["scope"], json!("org"), "{seen_by_admin}");
    assert_eq!(
        seen_by_admin["scopeId"], seen_by_member["scopeId"],
        "{seen_by_member}"
    );

    let (status, body) = h
        .put(&member_token, &member_path, &json!({ "mode": "bypass" }))
        .await;
    assert_eq!(
        status, 403,
        "a member may not remove the card for the whole org: {body}"
    );
    let (status, _) = h
        .put(&admin_token, &admin_path, &json!({ "mode": "never" }))
        .await;
    assert_eq!(status, 204);
    let (_, seen_by_member) = h.get(&member_token, &member_path).await;
    assert_eq!(
        seen_by_member["mode"],
        json!("never"),
        "the admin's choice reaches the member: {seen_by_member}"
    );
}
