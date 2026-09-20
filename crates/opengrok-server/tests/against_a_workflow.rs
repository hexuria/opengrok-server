//! A workflow is a recipe row with a decision tree in it, and it walks.
//!
//! The engine's own arithmetic — the budgets, the circuit breaker, the three fallbacks, the
//! refusal to fall back from a malformed question — is proved in `opengrok-tools`, in memory, with
//! no database. What is proved HERE is the half that only exists on a server: that the tree is
//! stored as the fourth `recipe_version.kind` and never mixes with a tape, that a listing can tell
//! the two apart, that the recipes a tree plays are checked against the caller's own permission
//! before the first step, and that a whole walk — probe, branch, question, two recipes, stop —
//! goes through the route and comes back as a run row somebody can read.
//!
//! NOT ONE BYTE LEAVES THE MACHINE. The box is a stand-in that records what it was asked; Jev is
//! `MockJev`, which has no HTTP client in it at all. Needs Postgres, and skips loudly without
//! OG_DATABASE_URL, the same bargain the other integration tests make.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use opengrok_box::{BoxResult, CommandOutput, Computer, StartedCommand};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::jev::{Answer, JevDoor, JevError, MockJev, NoulAnswer};
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

/// A box that answers every probe the same way and says every recipe played, remembering what it
/// was asked so a test can assert on the order.
#[derive(Default)]
struct StubBox {
    probe_says: String,
    asked: Mutex<Vec<String>>,
}

impl StubBox {
    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait]
impl Computer for StubBox {
    async fn create(&self, _ttl_seconds: Option<u64>) -> BoxResult<String> {
        Ok(format!("bx_stub_{}", uuid::Uuid::now_v7().simple()))
    }
    async fn run(
        &self,
        _box_id: &str,
        command: &str,
        _timeout_seconds: u32,
    ) -> BoxResult<CommandOutput> {
        self.asked.lock().unwrap().push(format!("probe {command}"));
        Ok(CommandOutput {
            exit_code: 0,
            stdout: self.probe_says.clone(),
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
        self.asked.lock().unwrap().push(format!(
            "recipe {}",
            request["name"].as_str().unwrap_or("?")
        ));
        Ok(json!({ "ok": true, "ran": 2 }))
    }
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
    agui: AgUiState,
    store: PgStore,
    account: AccountId,
    stub: Arc<StubBox>,
    client: reqwest::Client,
    email: String,
}

async fn harness(database_url: &str, jev: Option<Arc<dyn JevDoor>>, probe_says: &str) -> Harness {
    let email = format!("workflow-{}@og.local", uuid::Uuid::now_v7().simple());
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let account = seed_account(&store, &email).await;
    let stub = Arc::new(StubBox {
        probe_says: probe_says.to_string(),
        asked: Mutex::new(Vec::new()),
    });
    let auth = AuthState::new(
        store.clone(),
        Arc::new(TokenMinter::new(b"workflow-test-secret-workflow-test!!")),
        email.clone(),
    )
    .with_jev(jev);
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
    let app = opengrok_server::router(agui.clone());
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
        stub,
        client: reqwest::Client::new(),
        email,
    }
}

impl Harness {
    fn token(&self) -> String {
        self.agui
            .auth
            .minter
            .mint_access(
                self.account.as_str(),
                "sess-test",
                &self.email,
                "ultra",
                chrono::Utc::now().timestamp(),
                3600,
            )
            .expect("mint access")
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        let res = self
            .client
            .post(format!("{}{path}", self.base))
            .header("authorization", format!("Bearer {}", self.token()))
            .json(&body)
            .send()
            .await
            .expect("post");
        let status = res.status().as_u16();
        let text = res.text().await.expect("body");
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        let res = self
            .client
            .get(format!("{}{path}", self.base))
            .header("authorization", format!("Bearer {}", self.token()))
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

    /// A coworker with the stand-in computer behind it.
    async fn hire(&self) -> String {
        let (status, hired) = self.post("/coworkers", json!({ "name": "Ada" })).await;
        assert_eq!(status, 201, "{hired}");
        assert!(
            hired["boxId"]
                .as_str()
                .is_some_and(|id| id.starts_with("bx_stub_")),
            "the stand-in computer was assigned: {hired}"
        );
        hired["id"].as_str().expect("coworker id").to_string()
    }

    /// A taught recipe, written straight to the store: this file is about trees, and filtering a
    /// tape is proved elsewhere.
    async fn taught(&self, owner: &str, name: &str) -> String {
        let id = format!("rcp_{}", uuid::Uuid::now_v7());
        self.store
            .create_recipe(&id, owner, None, name, "", (1280, 800), now_ms())
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
                owner,
                now_ms(),
            )
            .await
            .expect("version");
        id
    }
}

/// The tree the run tests walk: look at the screen, decide, clear if needed, search, stop.
fn a_tree(clear: &str, search: &str) -> Value {
    json!({
        "workflow": 1,
        "start": "look",
        "budget": { "steps": 10, "seconds": 60 },
        "parameters": [{
            "name": "term", "description": "what to search for", "required": true,
            "kind": "text", "default": null, "values": null
        }],
        "steps": {
            "look": { "do": "observe", "as": "field",
                      "shell": "xdotool getactivewindow getwindowname", "then": "already-there" },
            "already-there": { "do": "when", "fact": "field",
                               "test": { "contains": "{{term}}" },
                               "yes": "already", "no": "is-it-ready" },
            "is-it-ready": { "do": "ask", "name": "ready",
                             "question": { "kind": "noul",
                                           "instructions": "Is the search field ready for {{term}}?" },
                             "yes": "search", "no": "clear" },
            "clear": { "do": "run", "recipe": clear, "then": "search", "otherwise": "gave-up" },
            "search": { "do": "run", "recipe": search, "values": { "q": "{{term}}" },
                        "then": "done" },
            "already": { "do": "stop", "outcome": "already", "say": "it was already searched" },
            "done": { "do": "stop", "outcome": "done", "say": "searched once" },
            "gave-up": { "do": "stop", "outcome": "gave-up" }
        }
    })
}

// -----------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_tree_is_the_fourth_kind_of_version_and_never_shares_a_row_with_a_tape() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, None, "Google Chrome").await;
    let owner = h.account.as_str().to_string();
    let clear = h.taught(&owner, "Clear the field").await;
    let search = h.taught(&owner, "Search").await;

    let (status, made) = h
        .post(
            "/workflows",
            json!({ "name": "Search once", "description": "look before you type",
                    "workflow": a_tree(&clear, &search) }),
        )
        .await;
    assert_eq!(status, 200, "{made}");
    let id = made["recipe"]["id"].as_str().expect("id").to_string();
    assert_eq!(made["recipe"]["kind"], "workflow");
    assert_eq!(made["versions"][0]["kind"], "workflow");
    // The body is stored as the engine writes it: the shape number is in the row, and the budget
    // the body asked for has already been clamped.
    assert_eq!(made["versions"][0]["body"]["workflow"], 1);
    assert_eq!(made["versions"][0]["body"]["budget"]["steps"], 10);
    // The parameters ride the summary, so a composer knows what to ask for without a second call.
    assert_eq!(made["recipe"]["parameters"][0]["name"], "term");

    // The store agrees: one version, of the fourth kind, and it is what a run would play.
    let runnable = h
        .store
        .recipe_runnable_version(&id)
        .await
        .expect("read")
        .expect("a runnable version");
    assert_eq!(runnable.kind, "workflow");

    // ONE ROW IS ALL TAPE OR ALL TREE, refused in both directions.
    let (status, why) = h
        .post(
            &format!("/recipes/{id}/versions"),
            json!({ "steps": [{"op": "wait", "ms": 5}] }),
        )
        .await;
    assert_eq!(status, 409, "{why}");
    assert!(
        why.as_str().unwrap_or_default().contains("is a workflow"),
        "{why}"
    );

    let (status, why) = h
        .post(
            &format!("/workflows/{clear}/versions"),
            json!({ "workflow": a_tree(&clear, &search) }),
        )
        .await;
    assert_eq!(status, 409, "{why}");
    assert!(
        why.as_str()
            .unwrap_or_default()
            .contains("is a taught recipe"),
        "{why}"
    );

    // And a tree is never offered to a bot as something `run_recipe` could play.
    let bot = h.hire().await;
    let coworker = CoworkerId::from_stored(bot);
    for recipe in [&id, &clear] {
        h.store
            .grant_recipe(recipe, coworker.as_str(), &owner, now_ms())
            .await
            .expect("grant");
    }
    let offers = opengrok_server::recipes::offers_for(&h.agui, &coworker).await;
    let offered: Vec<&str> = offers.iter().map(|offer| offer.id.as_str()).collect();
    assert_eq!(
        offered,
        vec![clear.as_str()],
        "the granted workflow is not in the tool's list"
    );
}

#[tokio::test]
async fn a_tree_that_could_not_be_walked_is_refused_with_the_step_that_is_wrong() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, None, "").await;

    let (status, why) = h
        .post(
            "/workflows",
            json!({ "name": "Broken", "workflow": {
                "workflow": 1, "start": "look",
                "steps": {
                    "look": { "do": "observe", "as": "field", "shell": "ls", "then": "nowhere" },
                    "done": { "do": "stop", "outcome": "done" }
                }
            }}),
        )
        .await;
    assert_eq!(status, 422, "{why}");
    assert!(
        why.as_str()
            .unwrap_or_default()
            .contains("\"look\" goes to \"nowhere\""),
        "{why}"
    );

    let (status, why) = h
        .post(
            "/workflows",
            json!({ "name": "Shapeless", "workflow": { "start": "a" } }),
        )
        .await;
    assert_eq!(status, 422, "{why}");
    assert!(
        why.as_str().unwrap_or_default().contains("\"workflow\": 1"),
        "{why}"
    );
}

#[tokio::test]
async fn one_listing_shows_both_and_can_be_asked_for_either() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, None, "").await;
    let owner = h.account.as_str().to_string();
    let clear = h.taught(&owner, "Clear the field").await;
    let search = h.taught(&owner, "Search").await;
    let (status, made) = h
        .post(
            "/workflows",
            json!({ "name": "Search once", "workflow": a_tree(&clear, &search) }),
        )
        .await;
    assert_eq!(status, 200, "{made}");
    let id = made["recipe"]["id"].as_str().expect("id").to_string();

    let ids = |body: &Value| -> Vec<String> {
        body["recipes"]
            .as_array()
            .expect("recipes")
            .iter()
            .filter_map(|row| row["id"].as_str().map(str::to_string))
            .collect()
    };

    let (status, all) = h.get("/recipes").await;
    assert_eq!(status, 200, "{all}");
    let everything = ids(&all);
    assert!(everything.contains(&id) && everything.contains(&clear));

    let (_, trees) = h.get("/recipes?kind=workflow").await;
    assert_eq!(ids(&trees), vec![id.clone()]);
    let (_, tapes) = h.get("/recipes?kind=recipe").await;
    let tapes = ids(&tapes);
    assert!(tapes.contains(&clear) && tapes.contains(&search) && !tapes.contains(&id));
}

#[tokio::test]
async fn a_whole_walk_goes_through_the_route_and_comes_back_as_a_run_anybody_can_read() {
    let database_url = database_or_skip!();
    // The window title has nothing to do with the term, so the tree takes the long way round.
    let h = harness(&database_url, None, "New Tab - Google Chrome").await;
    let owner = h.account.as_str().to_string();
    let clear = h.taught(&owner, "Clear the field").await;
    let search = h.taught(&owner, "Search").await;
    let bot = h.hire().await;
    let (_, made) = h
        .post(
            "/workflows",
            json!({ "name": "Search once", "workflow": a_tree(&clear, &search) }),
        )
        .await;
    let id = made["recipe"]["id"].as_str().expect("id").to_string();

    // A required parameter with nothing supplied is refused to the person who typed it, and never
    // becomes a run row.
    let (status, why) = h
        .post(
            &format!("/workflows/{id}/run"),
            json!({ "coworkerId": bot }),
        )
        .await;
    assert_eq!(status, 422, "{why}");
    assert!(why.as_str().unwrap_or_default().contains("'term'"), "{why}");

    // THE SWITCH: run it deterministically on purpose. No Jev is configured here either, and the
    // two are told apart in the receipt by the sentence, not by the outcome.
    let (status, ran) = h
        .post(
            &format!("/workflows/{id}/run"),
            json!({ "coworkerId": bot, "values": { "term": "kabisado" }, "jev": false }),
        )
        .await;
    assert_eq!(status, 200, "{ran}");
    // The reply says WHICH workflow, not only which shape of body it was written in.
    assert_eq!(ran["workflow"], id.as_str());
    assert_eq!(ran["shape"], 1);
    assert_eq!(ran["ok"], true);
    assert_eq!(ran["ending"], "stopped");
    assert_eq!(ran["outcome"], "done");
    assert_eq!(ran["say"], "searched once");
    assert_eq!(
        ran["steps"], 6,
        "probe, branch, question, clear, search, stop"
    );
    assert_eq!(ran["jev"], "off");
    assert_eq!(ran["fallbacks"], 1);
    assert!(
        ran["at"].is_null(),
        "a declared ending names no step: {ran}"
    );

    // The trail is the run's activity: the probe, the branch, the question that answered itself,
    // and the two recipes that were played because of it.
    let trail = ran["trail"].as_array().expect("a trail");
    assert_eq!(trail.len(), 6);
    assert_eq!(trail[0]["do"], "observe");
    assert_eq!(trail[0]["exit"], 0);
    assert_eq!(trail[1]["held"], false);
    assert_eq!(trail[2]["question"], "ready");
    assert_eq!(trail[2]["answer"], "no");
    assert_eq!(
        trail[2]["fallback"]["because"], "Jev was switched off for this run",
        "the fallback says it was a decision, not an outage"
    );
    assert_eq!(trail[3]["recipe"], clear.as_str());
    assert_eq!(trail[4]["recipe"], search.as_str());
    assert!(trail[4]["runId"].is_string(), "the inner run is pointed at");

    // What the tree actually made happen on the box, in order.
    assert_eq!(
        h.stub.asked(),
        vec![
            "probe xdotool getactivewindow getwindowname".to_string(),
            "recipe Clear the field".to_string(),
            "recipe Search".to_string(),
        ]
    );

    // The walk is a run of the workflow row; each recipe it played is a run of its own row.
    let runs = h.store.recipe_runs(&id, 10).await.expect("runs");
    assert_eq!(runs.len(), 1);
    assert!(runs[0].ok);
    assert!(runs[0].stopped_at.is_none());
    assert_eq!(runs[0].receipt["outcome"], "done");
    assert_eq!(runs[0].receipt["fallbacks"], 1);
    assert_eq!(
        h.store.recipe_runs(&search, 10).await.expect("runs").len(),
        1
    );

    // The history is on the workflow's own page, beside its versions.
    let (_, detail) = h.get(&format!("/recipes/{id}")).await;
    assert_eq!(detail["runs"].as_array().expect("runs").len(), 1);
    assert_eq!(detail["recipe"]["kind"], "workflow");

    // And the recipe route refuses to play a tree as a tape, in words.
    let (status, why) = h
        .post(
            &format!("/recipes/{id}/run"),
            json!({ "coworkerId": bot, "values": { "term": "kabisado" } }),
        )
        .await;
    assert_eq!(status, 422, "{why}");
    assert!(
        why.as_str()
            .unwrap_or_default()
            .contains("is a workflow, not a recipe"),
        "{why}"
    );
}

#[tokio::test]
async fn a_jev_that_answers_decides_the_branch_and_a_jev_that_cannot_says_so_in_the_run() {
    let database_url = database_or_skip!();

    // ---- answered: yes, so the field is already fit to type in and `clear` is skipped ----
    let jev = MockJev::answering(vec![("ready", Answer::Noul(NoulAnswer { noul: 0.93 }))]);
    let h = harness(&database_url, Some(Arc::new(jev)), "New Tab").await;
    let owner = h.account.as_str().to_string();
    let clear = h.taught(&owner, "Clear the field").await;
    let search = h.taught(&owner, "Search").await;
    let bot = h.hire().await;
    let (_, made) = h
        .post(
            "/workflows",
            json!({ "name": "Search once", "workflow": a_tree(&clear, &search) }),
        )
        .await;
    let id = made["recipe"]["id"].as_str().expect("id").to_string();
    let (status, ran) = h
        .post(
            &format!("/workflows/{id}/run"),
            json!({ "coworkerId": bot, "values": { "term": "kabisado" } }),
        )
        .await;
    assert_eq!(status, 200, "{ran}");
    assert_eq!(ran["jev"], "asked");
    assert_eq!(ran["fallbacks"], 0);
    assert_eq!(ran["steps"], 5, "the clear step was not needed: {ran}");
    assert_eq!(ran["trail"][2]["answer"], "yes");
    // A NOUL IS A PROBABILITY, NOT A CONFIDENCE, and the engine reads it the way /jev/ask does.
    assert_eq!(ran["trail"][2]["confidence"], 0.93);
    assert_eq!(
        h.stub.asked(),
        vec![
            "probe xdotool getactivewindow getwindowname".to_string(),
            "recipe Search".to_string(),
        ]
    );

    // ---- unreachable: the agreed fallback, and the reason on the record ----
    let down = MockJev::failing_with(JevError::Unreachable("no route to host".to_string()));
    let h = harness(&database_url, Some(Arc::new(down)), "New Tab").await;
    let owner = h.account.as_str().to_string();
    let clear = h.taught(&owner, "Clear the field").await;
    let search = h.taught(&owner, "Search").await;
    let bot = h.hire().await;
    let (_, made) = h
        .post(
            "/workflows",
            json!({ "name": "Search once", "workflow": a_tree(&clear, &search) }),
        )
        .await;
    let id = made["recipe"]["id"].as_str().expect("id").to_string();
    let (status, ran) = h
        .post(
            &format!("/workflows/{id}/run"),
            json!({ "coworkerId": bot, "values": { "term": "kabisado" } }),
        )
        .await;
    assert_eq!(status, 200, "{ran}");
    assert_eq!(ran["ok"], true, "an absent classifier is not a failed run");
    assert_eq!(ran["jev"], "asked");
    assert_eq!(ran["fallbacks"], 1);
    assert_eq!(ran["trail"][2]["answer"], "no");
    assert_eq!(ran["trail"][2]["fallback"]["rule"], "the branch that skips");
    assert!(
        ran["trail"][2]["fallback"]["because"]
            .as_str()
            .unwrap_or_default()
            .contains("no route to host"),
        "a person reading this can see the model was absent rather than wrong: {ran}"
    );
    // It is in the stored run too, not only in the reply to the caller who happened to be there.
    let runs = h.store.recipe_runs(&id, 10).await.expect("runs");
    assert_eq!(runs[0].receipt["fallbacks"], 1);

    // ---- a malformed question is OURS, and stops the walk rather than answering itself ----
    let ours = MockJev::failing_with(JevError::Asked("a rubric with no levels".to_string()));
    let h = harness(&database_url, Some(Arc::new(ours)), "New Tab").await;
    let owner = h.account.as_str().to_string();
    let clear = h.taught(&owner, "Clear the field").await;
    let search = h.taught(&owner, "Search").await;
    let bot = h.hire().await;
    let (_, made) = h
        .post(
            "/workflows",
            json!({ "name": "Search once", "workflow": a_tree(&clear, &search) }),
        )
        .await;
    let id = made["recipe"]["id"].as_str().expect("id").to_string();
    let (status, ran) = h
        .post(
            &format!("/workflows/{id}/run"),
            json!({ "coworkerId": bot, "values": { "term": "kabisado" } }),
        )
        .await;
    assert_eq!(status, 200, "{ran}");
    assert_eq!(ran["ok"], false);
    assert_eq!(ran["ending"], "broken");
    assert_eq!(ran["at"], "is-it-ready");
    assert_eq!(
        ran["fallbacks"], 0,
        "our own bug is never dressed up as an answer"
    );
    assert!(
        ran["say"]
            .as_str()
            .unwrap_or_default()
            .contains("a rubric with no levels"),
        "{ran}"
    );
    // Nothing was played: the walk stopped at the question rather than carrying on past it.
    assert_eq!(
        h.stub.asked(),
        vec!["probe xdotool getactivewindow getwindowname".to_string()]
    );
    let runs = h.store.recipe_runs(&id, 10).await.expect("runs");
    assert!(!runs[0].ok);
    assert_eq!(runs[0].stopped_at, Some(3));
}

#[tokio::test]
async fn a_recipe_the_caller_may_not_run_refuses_the_walk_before_the_box_is_touched() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, None, "New Tab").await;
    let owner = h.account.as_str().to_string();
    let clear = h.taught(&owner, "Clear the field").await;
    // Somebody else's recipe, shared with nobody. Running the workflow says nothing about it.
    let theirs = h.taught("acct_somebody_else", "Their secret").await;
    let bot = h.hire().await;
    let (_, made) = h
        .post(
            "/workflows",
            json!({ "name": "Search once", "workflow": a_tree(&clear, &theirs) }),
        )
        .await;
    let id = made["recipe"]["id"].as_str().expect("id").to_string();

    let (status, why) = h
        .post(
            &format!("/workflows/{id}/run"),
            json!({ "coworkerId": bot, "values": { "term": "kabisado" }, "jev": false }),
        )
        .await;
    assert_eq!(status, 403, "{why}");
    let why = why.as_str().unwrap_or_default();
    assert!(why.contains(&theirs), "the refusal names which one: {why}");
    assert!(
        h.stub.asked().is_empty(),
        "nothing was played and nothing was looked at"
    );
    assert!(
        h.store.recipe_runs(&id, 10).await.expect("runs").is_empty(),
        "a refused run is not a run"
    );
}
