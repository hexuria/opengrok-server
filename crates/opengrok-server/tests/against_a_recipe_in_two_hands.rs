//! A recipe held by two people — its owner and a colleague it was shared with — over HTTP.
//!
//! `against_a_shared_recipe` proves the rows; this file proves what a person and a bot see through
//! the routes and through a turn's offers, because the bugs here were all in what one person's
//! action left behind for the other: a share taken back that the colleague's bot kept running, and
//! a run whose caller hung up before the box answered.
//!
//! NOT ONE BYTE LEAVES THE MACHINE: the box is a stand-in. Needs Postgres, and skips loudly
//! without OG_DATABASE_URL, the same bargain the other integration tests make.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use opengrok_box::{BoxResult, CommandOutput, Computer, StartedCommand};
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

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// A box that plays every recipe, leaves one screenshot per run where it was told to write
/// them, and — when gated — holds each play until the test lets it finish, which is how a test
/// hangs up on a run that is still going.
#[derive(Default)]
struct StubBox {
    gate: Option<tokio::sync::Semaphore>,
    started: tokio::sync::Notify,
}

#[async_trait]
impl Computer for StubBox {
    async fn create(&self, _ttl_seconds: Option<u64>) -> BoxResult<String> {
        Ok(format!("bx_stub_{}", uuid::Uuid::now_v7().simple()))
    }
    async fn run(
        &self,
        _box_id: &str,
        _command: &str,
        _timeout_seconds: u32,
    ) -> BoxResult<CommandOutput> {
        Ok(CommandOutput {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            timed_out: false,
        })
    }
    async fn start(&self, _box_id: &str, _command: &str) -> BoxResult<StartedCommand> {
        Ok(StartedCommand {
            process_id: "p".to_string(),
            running: false,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
        })
    }
    async fn watch(&self, _box_id: &str, _process_id: &str) -> BoxResult<StartedCommand> {
        self.start("", "").await
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
    async fn run_recipe(&self, _box_id: &str, request: &Value) -> BoxResult<Value> {
        self.started.notify_one();
        if let Some(gate) = &self.gate {
            gate.acquire().await.expect("gate").forget();
        }
        let artifacts = match request["artifact_dir"].as_str() {
            Some(dir) => json!([{
                "path": format!("{dir}/step-0.png"), "kind": "screenshot",
                "mime": "image/png", "step_index": 0
            }]),
            None => json!([]),
        };
        Ok(json!({ "ok": true, "ran": 1, "artifacts": artifacts }))
    }
    async fn read_file_bytes(&self, _box_id: &str, _path: &str) -> BoxResult<Vec<u8>> {
        Ok(vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])
    }
}

async fn seed_account(store: &PgStore, email: &str, org: &str) -> AccountId {
    let id = AccountId::new();
    let at_ms = now_ms();
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.to_string(),
            password_hash: "x".to_string(),
            first_name: "Recipe".to_string(),
            last_name: String::new(),
            org_id: org.to_string(),
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
        first_name: "Recipe".to_string(),
        last_name: String::new(),
        org_id: Some(org.to_string()),
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
    stub: Arc<StubBox>,
    client: reqwest::Client,
    org: String,
}

/// One person: their account id, and a token that signs as them.
struct Person {
    id: String,
    token: String,
}

async fn harness(database_url: &str) -> Harness {
    harness_with(database_url, StubBox::default()).await
}

/// A harness whose box holds every recipe until the test adds a permit.
async fn gated(database_url: &str) -> Harness {
    harness_with(
        database_url,
        StubBox {
            gate: Some(tokio::sync::Semaphore::new(0)),
            ..StubBox::default()
        },
    )
    .await
}

async fn harness_with(database_url: &str, stub: StubBox) -> Harness {
    let stub = Arc::new(stub);
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
        Arc::new(TokenMinter::new(b"two-hands-test-secret-two-hands!!")),
        "host@og.local".to_string(),
    );
    let agui = AgUiState {
        auth,
        door: Arc::new(MockDoor::echoing()),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer: Some(stub.clone()),
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
        stub,
        client: reqwest::Client::new(),
        org: format!("org_two_hands_{}", uuid::Uuid::now_v7().simple()),
    }
}

impl Harness {
    /// A person in this harness's org: sharing is only ever inside one.
    async fn person(&self) -> Person {
        let email = format!("two-hands-{}@og.local", uuid::Uuid::now_v7().simple());
        let account = seed_account(&self.store, &email, &self.org).await;
        let token = self
            .agui
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
            .expect("mint access");
        Person {
            id: account.as_str().to_string(),
            token,
        }
    }

    async fn call(
        &self,
        who: &Person,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let url = format!("{}{path}", self.base);
        let request = match method {
            "GET" => self.client.get(url),
            "POST" => self.client.post(url),
            "DELETE" => self.client.delete(url),
            _ => panic!("no such method in this harness: {method}"),
        }
        .header("authorization", format!("Bearer {}", who.token));
        let request = match body {
            Some(body) => request.json(&body),
            None => request,
        };
        let response = request.send().await.expect("send");
        let status = response.status().as_u16();
        let text = response.text().await.expect("text");
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    /// A bot of this person's, with the stand-in computer behind it.
    async fn hire(&self, who: &Person) -> String {
        let (status, hired) = self
            .call(who, "POST", "/coworkers", Some(json!({ "name": "Ada" })))
            .await;
        assert_eq!(status, 201, "{hired}");
        assert!(
            hired["boxId"]
                .as_str()
                .is_some_and(|id| id.starts_with("bx_stub_")),
            "the stand-in computer was assigned: {hired}"
        );
        hired["id"].as_str().expect("coworker id").to_string()
    }

    /// A taught recipe, written straight to the store: filtering a tape is proved elsewhere.
    async fn taught(&self, owner: &Person, name: &str) -> String {
        let id = format!("rcp_{}", uuid::Uuid::now_v7());
        self.store
            .create_recipe(
                &id,
                &owner.id,
                Some(&self.org),
                name,
                "",
                (1280, 800),
                now_ms(),
            )
            .await
            .expect("create recipe");
        self.store
            .add_recipe_version(
                &id,
                "filtered",
                &json!({
                    "steps": [{"op": "click", "x": 10, "y": 10, "button": 1}],
                    "stop_on_error": true, "screenshot": "end"
                }),
                "filtered",
                &owner.id,
                now_ms(),
            )
            .await
            .expect("version");
        id
    }

    /// Start a request and hang up on it once the box has started playing: the tab closed, the
    /// proxy timed out, the laptop lid came down.
    async fn hang_up_on(&self, who: &Person, path: &str, body: Value) {
        let request = self
            .client
            .post(format!("{}{path}", self.base))
            .header("authorization", format!("Bearer {}", who.token))
            .json(&body)
            .send();
        let call = tokio::spawn(request);
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.stub.started.notified(),
        )
        .await
        .expect("the box was asked to play");
        call.abort();
        // Long enough for the server to read the closed socket and drop the handler's future.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }

    /// Let one held play finish.
    fn let_one_play(&self) {
        self.stub.gate.as_ref().expect("a gated box").add_permits(1);
    }

    /// The runs of a recipe once every one of them has finished, or a panic after ten seconds.
    async fn finished_runs(&self, who: &Person, id: &str, count: usize) -> Vec<Value> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let (_, detail) = self.call(who, "GET", &format!("/recipes/{id}"), None).await;
            let runs = detail["runs"].as_array().cloned().unwrap_or_default();
            if runs.len() == count && runs.iter().all(|run| run["state"] == "finished") {
                return runs;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{count} finished runs never appeared: {detail}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// What a turn with this bot would offer `run_recipe`, by recipe id.
    async fn offered(&self, bot: &str) -> Vec<String> {
        opengrok_server::recipes::offers_for(&self.agui, &CoworkerId::from_stored(bot.to_string()))
            .await
            .into_iter()
            .map(|offer| offer.id)
            .collect()
    }

    /// What the tool asks at the moment a recipe would play, after the turn's offers were read.
    async fn still_granted(&self, id: &str, bot: &str) -> bool {
        let source = opengrok_server::recipes::StoreRecipes {
            store: self.store.clone(),
        };
        opengrok_tools::RecipeSource::still_granted(
            &source,
            id,
            &CoworkerId::from_stored(bot.to_string()),
        )
        .await
        .is_ok()
    }

    /// Share `id` with `with`, have them accept it, and have them grant it to `bot`.
    async fn share_accept_grant(&self, owner: &Person, with: &Person, id: &str, bot: &str) {
        let (status, body) = self
            .call(
                owner,
                "POST",
                &format!("/recipes/{id}/share"),
                Some(json!({ "scope": "account", "scopeId": with.id })),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        let (status, body) = self
            .call(with, "POST", &format!("/recipes/{id}/accept"), None)
            .await;
        assert_eq!(status, 200, "{body}");
        let (status, body) = self
            .call(
                with,
                "POST",
                &format!("/recipes/{id}/grants"),
                Some(json!({ "coworkerId": bot })),
            )
            .await;
        assert_eq!(status, 200, "{body}");
    }
}

fn granted_bots(detail: &Value) -> Vec<String> {
    detail["grants"]
        .as_array()
        .expect("grants")
        .iter()
        .filter_map(|grant| grant["coworkerId"].as_str().map(str::to_string))
        .collect()
}

// -----------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_share_taken_back_leaves_the_next_turns_offers_and_the_run_route() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let owner = h.person().await;
    let colleague = h.person().await;
    let id = h.taught(&owner, "Pay the rent").await;
    let owners_bot = h.hire(&owner).await;
    let bot = h.hire(&colleague).await;

    let (status, body) = h
        .call(
            &owner,
            "POST",
            &format!("/recipes/{id}/grants"),
            Some(json!({ "coworkerId": owners_bot })),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    h.share_accept_grant(&owner, &colleague, &id, &bot).await;
    assert!(h.offered(&bot).await.contains(&id), "granted, so offered");
    assert!(h.still_granted(&id, &bot).await);

    // A recipient sees the grants of their own bots, not the owner's bot ids.
    let (status, theirs) = h
        .call(&colleague, "GET", &format!("/recipes/{id}"), None)
        .await;
    assert_eq!(status, 200, "{theirs}");
    assert_eq!(granted_bots(&theirs), vec![bot.clone()]);

    // ---- the owner takes it back ----
    let (status, body) = h
        .call(
            &owner,
            "DELETE",
            &format!("/recipes/{id}/share/account/{}", colleague.id),
            None,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        !h.offered(&bot).await.contains(&id),
        "the next turn does not offer a recipe whose share was taken back"
    );
    assert!(
        !h.still_granted(&id, &bot).await,
        "and a turn that read its offers before the share was taken back cannot play it either"
    );
    let (status, why) = h
        .call(
            &colleague,
            "POST",
            &format!("/recipes/{id}/run"),
            Some(json!({ "coworkerId": bot })),
        )
        .await;
    assert!(
        status == 403 || status == 404,
        "and the run route refuses it: {status} {why}"
    );
    let (_, detail) = h.call(&owner, "GET", &format!("/recipes/{id}"), None).await;
    assert_eq!(
        granted_bots(&detail),
        vec![owners_bot.clone()],
        "the owner's page lists only grants that still run"
    );
    assert!(h.offered(&owners_bot).await.contains(&id));

    // ---- shared again, and this time the colleague declines ----
    h.share_accept_grant(&owner, &colleague, &id, &bot).await;
    assert!(h.offered(&bot).await.contains(&id));
    let (status, body) = h
        .call(&colleague, "POST", &format!("/recipes/{id}/decline"), None)
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        !h.offered(&bot).await.contains(&id),
        "a declined recipe is not offered to the bot it was granted to"
    );
    let (status, why) = h
        .call(
            &colleague,
            "POST",
            &format!("/recipes/{id}/run"),
            Some(json!({ "coworkerId": bot })),
        )
        .await;
    assert!(status == 403 || status == 404, "{status} {why}");
    let (_, detail) = h.call(&owner, "GET", &format!("/recipes/{id}"), None).await;
    assert_eq!(granted_bots(&detail), vec![owners_bot]);
}

#[tokio::test]
async fn a_run_whose_caller_hung_up_still_lands_in_history_with_its_pictures() {
    let database_url = database_or_skip!();
    let h = gated(&database_url).await;
    let owner = h.person().await;
    let bot = h.hire(&owner).await;
    let id = h.taught(&owner, "Export the ledger").await;

    h.hang_up_on(
        &owner,
        &format!("/recipes/{id}/run"),
        json!({ "coworkerId": bot }),
    )
    .await;
    // The box is still playing and nobody is listening, and the run is already a row.
    let (_, detail) = h.call(&owner, "GET", &format!("/recipes/{id}"), None).await;
    let runs = detail["runs"].as_array().cloned().unwrap_or_default();
    assert_eq!(
        runs.len(),
        1,
        "a run is a row before the box answers: {detail}"
    );
    assert_eq!(runs[0]["state"], "running", "{detail}");
    let run_id = runs[0]["id"].as_str().expect("run id").to_string();
    let (status, why) = h
        .call(
            &owner,
            "POST",
            &format!("/recipes/{id}/run"),
            Some(json!({ "coworkerId": bot })),
        )
        .await;
    assert_eq!(
        status, 409,
        "one bot plays one recipe at a time, and says so: {why}"
    );

    h.let_one_play();
    let runs = h.finished_runs(&owner, &id, 1).await;
    assert_eq!(runs[0]["id"], run_id.as_str(), "the same row, finished");
    assert_eq!(runs[0]["ok"], true);
    let pictures = runs[0]["artifacts"].as_array().expect("artifacts");
    assert_eq!(
        pictures.len(),
        1,
        "the screenshot was pulled with nobody listening"
    );
    let kept = h
        .store
        .artifacts_for_run(&id, &run_id)
        .await
        .expect("artifacts");
    assert_eq!(kept.len(), 1);
    assert!(kept[0].deleted_at_ms.is_none(), "and nothing swept it away");
}

#[tokio::test]
async fn a_caller_that_prefers_not_to_wait_gets_the_run_id_at_once() {
    let database_url = database_or_skip!();
    let h = gated(&database_url).await;
    let owner = h.person().await;
    let bot = h.hire(&owner).await;
    let id = h.taught(&owner, "Export the ledger").await;

    let answer = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        h.client
            .post(format!("{}/recipes/{id}/run", h.base))
            .header("authorization", format!("Bearer {}", owner.token))
            .header("prefer", "respond-async")
            .json(&json!({ "coworkerId": bot }))
            .send(),
    )
    .await
    .expect("answered while the box is still playing")
    .expect("send");
    assert_eq!(answer.status().as_u16(), 202);
    assert_eq!(
        answer
            .headers()
            .get("preference-applied")
            .and_then(|value| value.to_str().ok()),
        Some("respond-async")
    );
    let accepted: Value = answer.json().await.expect("json");
    let run_id = accepted["runId"].as_str().expect("run id").to_string();
    assert_eq!(accepted["state"], "running");

    h.let_one_play();
    let runs = h.finished_runs(&owner, &id, 1).await;
    assert_eq!(runs[0]["id"], run_id.as_str());
    assert_eq!(runs[0]["artifacts"].as_array().map(Vec::len), Some(1));

    // A caller that waits still gets the whole receipt, as before.
    h.let_one_play();
    let (status, ran) = h
        .call(
            &owner,
            "POST",
            &format!("/recipes/{id}/run"),
            Some(json!({ "coworkerId": bot })),
        )
        .await;
    assert_eq!(status, 200, "{ran}");
    assert_eq!(ran["ok"], true);
    assert_eq!(ran["ran"], 1);
    assert_eq!(ran["artifacts"].as_array().map(Vec::len), Some(1));
}

#[tokio::test]
async fn a_workflow_walk_survives_its_caller_hanging_up() {
    let database_url = database_or_skip!();
    let h = gated(&database_url).await;
    let owner = h.person().await;
    let bot = h.hire(&owner).await;
    let tape = h.taught(&owner, "Export the ledger").await;
    let (status, made) = h
        .call(
            &owner,
            "POST",
            "/workflows",
            Some(json!({ "name": "Month end", "workflow": {
                "workflow": 1, "start": "export",
                "steps": {
                    "export": { "do": "run", "recipe": tape, "then": "done" },
                    "done": { "do": "stop", "outcome": "done" }
                }
            }})),
        )
        .await;
    assert_eq!(status, 200, "{made}");
    let id = made["recipe"]["id"].as_str().expect("id").to_string();

    h.hang_up_on(
        &owner,
        &format!("/workflows/{id}/run"),
        json!({ "coworkerId": bot, "jev": false }),
    )
    .await;
    let (_, detail) = h.call(&owner, "GET", &format!("/recipes/{id}"), None).await;
    assert_eq!(
        detail["runs"][0]["state"], "running",
        "the walk is a row before its first recipe answers: {detail}"
    );

    h.let_one_play();
    let runs = h.finished_runs(&owner, &id, 1).await;
    assert_eq!(runs[0]["ok"], true, "{runs:?}");
    assert_eq!(runs[0]["receipt"]["outcome"], "done");
    assert_eq!(
        h.finished_runs(&owner, &tape, 1).await.len(),
        1,
        "and the recipe it played is written down under its own row"
    );
}

#[tokio::test]
async fn a_recipe_whose_tape_cannot_be_stored_is_not_a_200() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let owner = h.person().await;
    let name = format!("Unstorable {}", uuid::Uuid::now_v7().simple());
    // Postgres refuses a NUL in jsonb, so the tape as taught cannot be kept — while the steps
    // filtered off it carry no key code and could be.
    let (status, body) = h
        .call(
            &owner,
            "POST",
            "/recipes",
            Some(json!({ "name": name, "raw": [
                { "kind": "down", "x": 5, "y": 5, "at": 0, "code": "\u{0}" },
                { "kind": "up", "x": 5, "y": 5, "at": 80 }
            ]})),
        )
        .await;
    if status == 200 {
        let kinds: Vec<&str> = body["versions"]
            .as_array()
            .expect("versions")
            .iter()
            .filter_map(|version| version["kind"].as_str())
            .collect();
        assert!(
            kinds.contains(&"raw") && kinds.contains(&"filtered"),
            "a 200 is a recipe with both of its versions: {body}"
        );
    } else {
        assert!(
            status >= 500,
            "the store refused, and said so: {status} {body}"
        );
        let (_, mine) = h.call(&owner, "GET", "/recipes?filter=mine", None).await;
        assert!(
            mine["recipes"]
                .as_array()
                .expect("recipes")
                .iter()
                .all(|row| row["name"] != name.as_str()),
            "and no half-written recipe is left in the list: {mine}"
        );
    }
}

/// Run a recipe on one of this person's bots and wait for it; the run's id.
async fn run_on(h: &Harness, who: &Person, bot: &str, id: &str) -> String {
    let (status, ran) = h
        .call(
            who,
            "POST",
            &format!("/recipes/{id}/run"),
            Some(json!({ "coworkerId": bot })),
        )
        .await;
    assert_eq!(status, 200, "{ran}");
    ran["runId"].as_str().expect("run id").to_string()
}

/// Every run a person is shown, with every picture in it that they can open.
async fn my_runs(h: &Harness, who: &Person, id: &str) -> Vec<Value> {
    let (status, detail) = h.call(who, "GET", &format!("/recipes/{id}"), None).await;
    assert_eq!(status, 200, "{detail}");
    let runs = detail["runs"].as_array().cloned().unwrap_or_default();
    for run in &runs {
        for picture in run["artifacts"].as_array().cloned().unwrap_or_default() {
            let artifact = picture["id"].as_str().expect("artifact id");
            let (status, _) = h
                .call(who, "GET", &format!("/artifacts/{artifact}/bytes"), None)
                .await;
            assert_eq!(
                status, 200,
                "a picture listed on this person's page opens for them: {run}"
            );
        }
    }
    runs
}

#[tokio::test]
async fn a_recipients_runs_neither_evict_the_owners_history_nor_list_pictures_they_cannot_open() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let owner = h.person().await;
    let colleague = h.person().await;
    let id = h.taught(&owner, "Reconcile the bank").await;
    let owners_bot = h.hire(&owner).await;
    let bot = h.hire(&colleague).await;
    let (status, body) = h
        .call(
            &owner,
            "POST",
            &format!("/recipes/{id}/share"),
            Some(json!({ "scope": "account", "scopeId": colleague.id })),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = h
        .call(&colleague, "POST", &format!("/recipes/{id}/accept"), None)
        .await;
    assert_eq!(status, 200, "{body}");

    let owners_run = run_on(&h, &owner, &owners_bot, &id).await;
    for _ in 0..5 {
        run_on(&h, &colleague, &bot, &id).await;
    }

    let mine = my_runs(&h, &owner, &id).await;
    assert_eq!(
        mine.iter().map(|run| run["id"].clone()).collect::<Vec<_>>(),
        vec![json!(owners_run)],
        "the owner sees their own run, and five runs by somebody else did not evict it"
    );
    let kept = h
        .store
        .artifacts_for_run(&id, &owners_run)
        .await
        .expect("artifacts");
    assert_eq!(kept.len(), 1);
    assert!(
        kept[0].deleted_at_ms.is_none(),
        "and its screenshot survived"
    );

    let theirs = my_runs(&h, &colleague, &id).await;
    assert_eq!(theirs.len(), 5, "the colleague sees their own five");
    assert!(theirs.iter().all(|run| run["coworkerId"] == bot.as_str()));

    // A sixth prunes the colleague's own oldest, and nobody else's.
    run_on(&h, &colleague, &bot, &id).await;
    assert_eq!(my_runs(&h, &colleague, &id).await.len(), 5);
    assert_eq!(my_runs(&h, &owner, &id).await.len(), 1);
}
