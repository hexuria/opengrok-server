//! Account skills attached to one coworker (#270).
//!
//! `GET /coworkers/{id}/skills` answers every skill the owner may attach and which are attached;
//! `PUT` replaces the set. AN ATTACHED SKILL IS ONLY TRUE IF A TURN OBEYS IT, so these read the run
//! path back — the system message and the tools a real turn put in front of the model, what
//! `use_skill` returned to it, and `GET /coworkers/{id}/tools` — rather than trusting the PUT's 200.
//! The door is scripted to call `use_skill`, so the turn that reads a skill is recorded into the
//! wire corpus NativeChat vendors.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine as _;
use opengrok_box::{BoxResult, CommandOutput, Computer, StartedCommand};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_harness::{DeltaStream, ModelDelta, ModelDoor, ModelError, ModelRequest};
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::host_state::HostState;
use opengrok_store::PgStore;
use opengrok_tools::skill::{USE_SKILL, USE_SKILL_DESCRIPTION};
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

/// Keeps every request. While `reads` names a skill, a turn's first round calls `use_skill` for it
/// and the round that sees the result answers in words; otherwise every round is words.
#[derive(Default)]
struct SkillDoor {
    reads: Mutex<Option<String>>,
    asked: Mutex<Vec<ModelRequest>>,
}

#[async_trait]
impl ModelDoor for SkillDoor {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
        self.asked.lock().unwrap().push(request.clone());
        let answered = request
            .messages
            .iter()
            .any(|message| message.role == "tool");
        let id = "call-use-skill".to_string();
        let script = match self.reads.lock().unwrap().clone() {
            Some(name) if !answered => vec![
                ModelDelta::ToolCallStart {
                    id: id.clone(),
                    name: USE_SKILL.to_string(),
                },
                ModelDelta::ToolCallArgs {
                    id: id.clone(),
                    delta: json!({ "name": name }).to_string(),
                },
                ModelDelta::ToolCallEnd { id },
            ],
            _ => vec![ModelDelta::Text("done".to_string())],
        };
        Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
    }
}

/// A computer that is always up and keeps what is written to it, answering the two shell
/// questions a skill's files ask — where home is, and whether the directory's manifest holds — as
/// a shell would (`against_a_skills_files.rs` explains the manifest).
#[derive(Default)]
struct DiskBox {
    files: Mutex<BTreeMap<String, String>>,
}

fn said(stdout: &str, exit_code: i32) -> CommandOutput {
    CommandOutput {
        exit_code,
        stdout: stdout.to_string(),
        stderr: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
        timed_out: false,
    }
}

#[async_trait]
impl Computer for DiskBox {
    async fn create(&self, _ttl: Option<u64>) -> BoxResult<String> {
        Ok(format!("bx_disk_{}", uuid::Uuid::now_v7().simple()))
    }
    async fn run(&self, _box_id: &str, command: &str, _timeout: u32) -> BoxResult<CommandOutput> {
        if command.contains("$HOME") {
            return Ok(said("/home/box", 0));
        }
        // A fresh directory has no manifest, so the check fails and everything is written.
        if command.starts_with("cd '") {
            return Ok(said("", 1));
        }
        if let Some(dir) = command
            .strip_prefix("rm -rf '")
            .and_then(|rest| rest.split_once('\''))
            .map(|(dir, _)| format!("{dir}/"))
        {
            self.files
                .lock()
                .unwrap()
                .retain(|path, _| !path.starts_with(&dir));
        }
        Ok(said("", 0))
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
    async fn watch(&self, box_id: &str, _process_id: &str) -> BoxResult<StartedCommand> {
        self.start(box_id, "").await
    }
    async fn read_file(&self, _box_id: &str, path: &str) -> BoxResult<String> {
        self.files
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .ok_or(opengrok_box::BoxError::NoSuchBox)
    }
    async fn write_file(&self, _box_id: &str, path: &str, content: &str) -> BoxResult<()> {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_string(), content.to_string());
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
}

struct Person {
    id: AccountId,
    token: String,
}

struct Harness {
    base: String,
    agui: AgUiState,
    store: PgStore,
    door: Arc<SkillDoor>,
    client: reqwest::Client,
    /// Every person in one harness shares this org unless made without one.
    org: String,
}

async fn harness(database_url: &str, computer: Option<Arc<dyn Computer>>) -> Harness {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let door = Arc::new(SkillDoor::default());
    let agui = AgUiState {
        auth: AuthState::new(
            store.clone(),
            Arc::new(TokenMinter::new(b"attached-skills-attached-skills!")),
            "host@og.local".to_string(),
        ),
        door: door.clone(),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer,
        vault: None,
        connectors: Connectors {
            providers: Arc::new(BTreeMap::new()),
            redirect_uri: "http://127.0.0.1/callback".to_string(),
        },
        plugins: Arc::new(BTreeMap::new()),
        host_settings: None,
    };
    let host = HostState::new(agui.clone(), Some("http://opengrok.lan:1447".to_string()));
    let app = opengrok_server::router(agui.clone(), host);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    Harness {
        base,
        agui,
        store,
        door,
        client: reqwest::Client::new(),
        org: format!("org_skills_{}", uuid::Uuid::now_v7().simple()),
    }
}

impl Harness {
    /// A signed-in person, in this harness's org or in none.
    async fn person(&self, first: &str, in_org: bool) -> Person {
        let id = AccountId::new();
        let email = format!("{first}-{}@og.local", uuid::Uuid::now_v7().simple());
        let org = in_org.then(|| self.org.clone());
        let at_ms = chrono::Utc::now().timestamp_millis();
        let events = Account::default()
            .decide(AccountCommand::Register {
                email: email.clone(),
                password_hash: "x".to_string(),
                first_name: first.to_string(),
                last_name: String::new(),
                org_id: org.clone().unwrap_or_default(),
                plan: Plan::Ultra,
                verified: true,
                enabled: true,
                at_ms,
            })
            .expect("register");
        let view = AccountView {
            id: id.clone(),
            email: email.clone(),
            plan: Plan::Ultra,
            trial: false,
            updated_at_ms: at_ms,
            password_hash: Some("x".to_string()),
            first_name: first.to_string(),
            last_name: String::new(),
            org_id: org,
            verified: true,
            enabled: true,
            avatar_url: None,
        };
        self.store
            .append_account(&id, 0, &events, &view)
            .await
            .expect("append account");
        let token = self
            .agui
            .auth
            .minter
            .mint_access(
                id.as_str(),
                "sess-skills",
                &email,
                "ultra",
                chrono::Utc::now().timestamp(),
                3600,
            )
            .expect("mint access");
        Person { id, token }
    }

    /// Any request, answered as status and JSON body.
    async fn call(
        &self,
        who: &Person,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let method = reqwest::Method::from_bytes(method.as_bytes()).expect("method");
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&who.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.expect("request");
        let status = response.status().as_u16();
        let text = response.text().await.expect("body");
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn hire(&self, who: &Person) -> String {
        let (status, hired) = self
            .call(who, "POST", "/coworkers", Some(json!({ "name": "Ada" })))
            .await;
        assert_eq!(status, 201, "{hired}");
        hired["id"].as_str().expect("coworker id").to_string()
    }

    /// A skill with a body, switched on as a person's own skill is born. Its id.
    async fn skill(&self, who: &Person, name: &str, description: &str, body: &str) -> String {
        let made = json!({ "name": name, "description": description, "body": body });
        let (status, made) = self.call(who, "POST", "/skills", Some(made)).await;
        assert_eq!(status, 200, "{made}");
        made["id"].as_str().expect("skill id").to_string()
    }

    async fn switch(&self, who: &Person, skill: &str, enabled: bool) {
        let path = format!("/skills/{skill}");
        let body = json!({ "enabled": enabled });
        let (status, switched) = self.call(who, "PUT", &path, Some(body)).await;
        assert_eq!(status, 200, "{switched}");
    }

    async fn skills(&self, who: &Person, agent: &str) -> Value {
        let path = format!("/coworkers/{agent}/skills");
        let (status, body) = self.call(who, "GET", &path, None).await;
        assert_eq!(status, 200, "{body}");
        assert!(
            body["skills"].is_array(),
            "the rows are always an array: {body}"
        );
        assert!(body["version"].is_i64(), "a version is a number: {body}");
        body
    }

    async fn attach(&self, who: &Person, agent: &str, body: Value) -> (u16, Value) {
        let path = format!("/coworkers/{agent}/skills");
        self.call(who, "PUT", &path, Some(body)).await
    }

    /// One turn as the app sends it: the run id, the SSE body, and every request the door saw.
    async fn turn(&self, who: &Person, agent: &str) -> (String, String, Vec<ModelRequest>) {
        let before = self.door.asked.lock().unwrap().len();
        let run_id = uuid::Uuid::now_v7().to_string();
        let response = self
            .client
            .post(format!("{}/ag-ui", self.base))
            .bearer_auth(&who.token)
            .json(&json!({
                "threadId": format!("thr-{}", uuid::Uuid::now_v7()),
                "runId": run_id,
                "messages": [{ "id": "m1", "role": "user", "content": "triage the new bugs" }],
                "forwardedProps": { "coworkerId": agent },
            }))
            .send()
            .await
            .expect("turn");
        assert_eq!(response.status().as_u16(), 200, "ag-ui turn");
        let sse = response.text().await.expect("sse");
        assert!(sse.contains("RUN_FINISHED"), "{sse}");
        let asked = self.door.asked.lock().unwrap()[before..].to_vec();
        assert!(!asked.is_empty(), "the turn asked the model");
        (run_id, sse, asked)
    }

    /// What `GET /coworkers/{id}/tools` says a turn would be offered.
    async fn listed(&self, who: &Person, agent: &str) -> Vec<Value> {
        let path = format!("/coworkers/{agent}/tools");
        let (status, body) = self.call(who, "GET", &path, None).await;
        assert_eq!(status, 200, "{body}");
        body["tools"].as_array().expect("tools").clone()
    }
}

fn offered(request: &ModelRequest) -> Vec<String> {
    request
        .tools
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_string))
        .collect()
}

fn system(request: &ModelRequest) -> String {
    request.system.clone().unwrap_or_default()
}

/// What `use_skill` returned to the model, from the round that read it.
fn read_back(asked: &[ModelRequest]) -> String {
    let last = asked.last().expect("a round after the tool");
    let result = last.messages.iter().find(|message| message.role == "tool");
    result.expect("a tool result").content.clone()
}

/// The row with this id, which must be there exactly once.
fn row<'a>(body: &'a Value, id: &str) -> &'a Value {
    let rows: Vec<&Value> = body["skills"]
        .as_array()
        .expect("rows")
        .iter()
        .filter(|row| row["id"] == id)
        .collect();
    assert_eq!(rows.len(), 1, "exactly one row for {id}: {body}");
    rows[0]
}

fn ids(body: &Value) -> Vec<String> {
    body["skills"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| row["id"].as_str().expect("id").to_string())
        .collect()
}

#[tokio::test]
async fn the_owner_reads_their_own_skills_and_their_orgs_switched_on_ones() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, None).await;
    let owner = h.person("owner", true).await;
    let colleague = h.person("colleague", true).await;
    let stranger = h.person("stranger", false).await;
    let agent = h.hire(&owner).await;
    let beta = h.skill(&owner, "beta", "Second.", "Do beta.").await;
    let alpha = h.skill(&owner, "alpha", "First.", "Do alpha.").await;
    h.switch(&owner, &beta, false).await;
    let gamma = h.skill(&colleague, "gamma", "Theirs.", "Do gamma.").await;
    let delta = h.skill(&colleague, "delta", "Off.", "Do delta.").await;
    h.switch(&colleague, &delta, false).await;
    let omega = h.skill(&stranger, "omega", "Nobody's.", "Do omega.").await;

    let body = h.skills(&owner, &agent).await;
    assert_eq!(body["version"], 0, "nothing attached yet: {body}");
    assert_eq!(
        ids(&body),
        vec![alpha.clone(), beta.clone(), gamma.clone()],
        "their own first, switched off too, then the org's that are on, each by name: {body}"
    );
    assert_eq!(
        row(&body, &alpha),
        &json!({ "id": alpha, "name": "alpha", "description": "First.", "scope": "mine",
            "attached": false, "enabled": true })
    );
    assert_eq!(row(&body, &beta)["enabled"], false);
    assert_eq!(
        row(&body, &gamma),
        &json!({ "id": gamma, "name": "gamma", "description": "Theirs.", "scope": "org",
            "attached": false, "enabled": true })
    );
    assert!(!ids(&body).contains(&delta) && !ids(&body).contains(&omega));

    let (status, put) = h
        .attach(&owner, &agent, json!({ "attached": [gamma, alpha] }))
        .await;
    assert_eq!(status, 200, "{put}");
    assert_eq!(
        put,
        h.skills(&owner, &agent).await,
        "a PUT answers what a GET then reads"
    );
    assert_eq!(put["version"], 1);
    assert_eq!(row(&put, &alpha)["attached"], true);
    assert_eq!(row(&put, &gamma)["attached"], true);
    assert_eq!(row(&put, &beta)["attached"], false);
}

/// Two screens read the same set and both save. The second was looking at a set that no longer
/// exists, so it is told so, and the first one's choice is what stands.
#[tokio::test]
async fn two_puts_from_the_same_read_land_once_and_the_second_is_told_the_skills_changed() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, None).await;
    let owner = h.person("owner", true).await;
    let agent = h.hire(&owner).await;
    let alpha = h.skill(&owner, "alpha", "First.", "Do alpha.").await;
    let beta = h.skill(&owner, "beta", "Second.", "Do beta.").await;
    let read = h.skills(&owner, &agent).await["version"].as_i64().unwrap();

    let first = json!({ "attached": [alpha], "version": read });
    let (status, first) = h.attach(&owner, &agent, first).await;
    assert_eq!(status, 200, "{first}");
    assert_eq!(first["version"], read + 1, "a change moves the version on");
    let second = json!({ "attached": [beta], "version": read });
    let (status, second) = h.attach(&owner, &agent, second).await;
    assert_eq!(status, 409, "{second}");
    assert_eq!(
        second,
        json!({ "error": "the skills changed since you looked", "code": "skills-changed" })
    );
    assert_eq!(
        h.skills(&owner, &agent).await,
        first,
        "nothing of the second landed"
    );

    // Saving what is there, in another order or twice over, is not a change.
    let same = json!({ "attached": [alpha, alpha], "version": read + 1 });
    let (status, same) = h.attach(&owner, &agent, same).await;
    assert_eq!(
        (status, &same["version"]),
        (200, &json!(read + 1)),
        "{same}"
    );
    // Without a version the save is unconditional.
    let (status, both) = h
        .attach(&owner, &agent, json!({ "attached": [beta, alpha] }))
        .await;
    assert_eq!(
        (status, &both["version"]),
        (200, &json!(read + 2)),
        "{both}"
    );
}

#[tokio::test]
async fn an_unknown_id_or_more_than_twenty_is_refused_and_nothing_changes() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, None).await;
    let owner = h.person("owner", true).await;
    let stranger = h.person("stranger", false).await;
    let agent = h.hire(&owner).await;
    let alpha = h.skill(&owner, "alpha", "First.", "Do alpha.").await;
    let theirs = h.skill(&stranger, "theirs", "Not yours.", "Do it.").await;
    let (status, _) = h
        .attach(&owner, &agent, json!({ "attached": [alpha] }))
        .await;
    assert_eq!(status, 200);
    let before = h.skills(&owner, &agent).await;

    let cases = [
        ("skl_nobody", json!([alpha, "skl_nobody"])),
        (theirs.as_str(), json!([theirs])),
    ];
    for (unknown, sent) in cases {
        let (status, refused) = h.attach(&owner, &agent, json!({ "attached": sent })).await;
        assert_eq!(status, 422, "{refused}");
        assert_eq!(
            refused,
            json!({ "error": format!("no skill {unknown}") }),
            "the id, last"
        );
    }
    let many: Vec<String> = (0..21).map(|n| format!("skl_many_{n}")).collect();
    let (status, refused) = h.attach(&owner, &agent, json!({ "attached": many })).await;
    assert_eq!(status, 422, "{refused}");
    assert_eq!(
        refused,
        json!({ "error": "a coworker takes at most 20 skills, and this is 21" })
    );
    for malformed in [
        json!({}),
        json!({ "attach": [alpha] }),
        json!({ "attached": alpha }),
        json!({ "attached": [alpha], "version": "latest" }),
    ] {
        let (status, refused) = h.attach(&owner, &agent, malformed.clone()).await;
        assert_eq!(status, 422, "{malformed}: {refused}");
        assert!(
            !refused["error"].as_str().unwrap_or_default().is_empty(),
            "{refused}"
        );
    }
    let response = h
        .client
        .put(format!("{}/coworkers/{agent}/skills", h.base))
        .bearer_auth(&owner.token)
        .header("content-type", "application/json")
        .body("not json")
        .send()
        .await
        .expect("put");
    assert_eq!(response.status().as_u16(), 422);

    assert_eq!(h.skills(&owner, &agent).await, before, "nothing changed");
}

#[tokio::test]
async fn another_account_is_told_no_such_coworker_on_both_verbs() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, None).await;
    let owner = h.person("owner", true).await;
    // In the owner's org, even: the attached set is the owner's to read and to set.
    let colleague = h.person("colleague", true).await;
    let agent = h.hire(&owner).await;
    let before = h.skills(&owner, &agent).await;
    let path = format!("/coworkers/{agent}/skills");
    let no_such = json!({ "error": "no such coworker" });

    for (method, body) in [
        ("GET", None),
        ("PUT", Some(json!({ "attached": [] }))),
        ("PUT", Some(json!({}))),
    ] {
        let (status, refused) = h.call(&colleague, method, &path, body).await;
        assert_eq!((status, refused), (404, no_such.clone()), "{method}");
    }
    let (status, refused) = h
        .call(&owner, "GET", "/coworkers/cw_nobody/skills", None)
        .await;
    assert_eq!((status, refused), (404, no_such));
    assert_eq!(h.skills(&owner, &agent).await, before);
}

/// A 403 here is a sentence the app can show, on both verbs: a withdrawn grant is not its owner's
/// to read the skills of, nor to set them.
#[tokio::test]
async fn a_withdrawn_grant_is_refused_in_words_and_nothing_changes() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, None).await;
    let owner = h.person("owner", true).await;
    let agent = h.hire(&owner).await;
    let alpha = h.skill(&owner, "alpha", "First.", "Do alpha.").await;
    let coworker = CoworkerId::from_stored(agent.clone());
    let before = h.store.coworker_skills(&coworker).await.expect("before");
    h.store
        .revoke_access(&owner.id, &coworker, 1)
        .await
        .expect("withdraw the grant");

    let path = format!("/coworkers/{agent}/skills");
    for (method, body) in [("GET", None), ("PUT", Some(json!({ "attached": [alpha] })))] {
        let (status, refused) = h.call(&owner, method, &path, body).await;
        assert_eq!(status, 403, "{method}: {refused}");
        let why = refused["error"].as_str().unwrap_or_default();
        assert!(why.contains("revoked"), "{method}: {refused}");
    }
    let after = h.store.coworker_skills(&coworker).await.expect("after");
    assert_eq!(after, before, "nothing changed");
}

/// Any id a GET lists is one a PUT takes, a switched-off skill of their own too: attaching it and
/// keeping it are 200. It is simply offered to no turn while it is off.
#[tokio::test]
async fn a_switched_off_skill_of_their_own_can_be_attached_and_kept_and_is_offered_to_no_turn() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, None).await;
    let owner = h.person("owner", true).await;
    let agent = h.hire(&owner).await;
    let beta = h.skill(&owner, "beta", "Second.", "Do beta.").await;
    h.switch(&owner, &beta, false).await;

    let (status, put) = h
        .attach(&owner, &agent, json!({ "attached": [beta] }))
        .await;
    assert_eq!(status, 200, "{put}");
    assert_eq!(row(&put, &beta)["attached"], true);
    assert_eq!(row(&put, &beta)["enabled"], false);
    let (status, kept) = h
        .attach(&owner, &agent, json!({ "attached": [beta] }))
        .await;
    assert_eq!(status, 200, "{kept}");

    let (_, _, asked) = h.turn(&owner, &agent).await;
    assert!(
        !system(&asked[0]).contains("`beta`"),
        "{}",
        system(&asked[0])
    );
    assert!(!offered(&asked[0]).contains(&USE_SKILL.to_string()));
    let listed = h.listed(&owner, &agent).await;
    assert!(
        !listed.iter().any(|tool| tool["name"] == USE_SKILL),
        "{listed:?}"
    );

    // Switched back on, the next turn has it.
    h.switch(&owner, &beta, true).await;
    let (_, _, asked) = h.turn(&owner, &agent).await;
    assert!(
        system(&asked[0]).contains("- `beta`: Second."),
        "{}",
        system(&asked[0])
    );
}

/// An attached org skill its author switches off stays a row, switched off and saying nothing
/// more, and stays attached — offered to no turn — until the owner saves without it.
#[tokio::test]
async fn an_org_skill_its_author_switches_off_stays_attached_but_is_offered_to_no_turn() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, None).await;
    let owner = h.person("owner", true).await;
    let colleague = h.person("colleague", true).await;
    let agent = h.hire(&owner).await;
    let gamma = h.skill(&colleague, "gamma", "Theirs.", "Do gamma.").await;
    let (status, _) = h
        .attach(&owner, &agent, json!({ "attached": [gamma] }))
        .await;
    assert_eq!(status, 200);
    let (_, _, asked) = h.turn(&owner, &agent).await;
    assert!(
        system(&asked[0]).contains("- `gamma`: Theirs."),
        "{}",
        system(&asked[0])
    );

    h.switch(&colleague, &gamma, false).await;
    let body = h.skills(&owner, &agent).await;
    assert_eq!(
        row(&body, &gamma),
        &json!({ "id": gamma, "name": "gamma", "description": "", "scope": "org",
            "attached": true, "enabled": false })
    );
    let (_, _, asked) = h.turn(&owner, &agent).await;
    assert!(
        !system(&asked[0]).contains("`gamma`"),
        "{}",
        system(&asked[0])
    );
    assert!(!offered(&asked[0]).contains(&USE_SKILL.to_string()));

    // Named, it is kept; left out, it is gone, and not a row it could come back from.
    let (status, kept) = h
        .attach(&owner, &agent, json!({ "attached": [gamma] }))
        .await;
    assert_eq!(status, 200, "{kept}");
    assert_eq!(row(&kept, &gamma)["attached"], true);
    let (status, left) = h.attach(&owner, &agent, json!({ "attached": [] })).await;
    assert_eq!(status, 200, "{left}");
    assert!(!ids(&left).contains(&gamma), "{left}");
    let (status, again) = h
        .attach(&owner, &agent, json!({ "attached": [gamma] }))
        .await;
    assert_eq!(
        (status, again),
        (422, json!({ "error": format!("no skill {gamma}") }))
    );
}

#[tokio::test]
async fn a_deleted_skill_drops_from_the_rows_and_from_the_set_at_the_next_save() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, None).await;
    let owner = h.person("owner", true).await;
    let agent = h.hire(&owner).await;
    let alpha = h.skill(&owner, "alpha", "First.", "Do alpha.").await;
    let beta = h.skill(&owner, "beta", "Second.", "Do beta.").await;
    let (status, put) = h
        .attach(&owner, &agent, json!({ "attached": [alpha, beta] }))
        .await;
    assert_eq!(status, 200, "{put}");
    let (status, _) = h
        .call(&owner, "DELETE", &format!("/skills/{alpha}"), None)
        .await;
    assert_eq!(status, 204);

    let body = h.skills(&owner, &agent).await;
    assert_eq!(ids(&body), vec![beta.clone()], "{body}");
    let (_, _, asked) = h.turn(&owner, &agent).await;
    assert!(
        !system(&asked[0]).contains("`alpha`"),
        "{}",
        system(&asked[0])
    );
    let (status, refused) = h
        .attach(&owner, &agent, json!({ "attached": [alpha] }))
        .await;
    assert_eq!(
        (status, refused),
        (422, json!({ "error": format!("no skill {alpha}") }))
    );
    let (status, saved) = h
        .attach(&owner, &agent, json!({ "attached": [beta] }))
        .await;
    assert_eq!(status, 200, "{saved}");
    assert_eq!(
        saved["version"],
        body["version"].as_i64().unwrap() + 1,
        "the set lost it"
    );
    let coworker = CoworkerId::from_stored(agent.clone());
    let (kept, _) = h.store.coworker_skills(&coworker).await.expect("set");
    assert_eq!(
        kept.iter().map(|s| s.id.clone()).collect::<Vec<_>>(),
        vec![beta]
    );
}

/// Every turn lists each attached skill that is on, in its system message, and offers `use_skill`
/// exactly then — and `GET /coworkers/{id}/tools` says the same.
#[tokio::test]
async fn a_turn_lists_its_attached_skills_and_offers_use_skill_only_while_one_is_on() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, None).await;
    let owner = h.person("owner", true).await;
    let agent = h.hire(&owner).await;
    let (_, _, asked) = h.turn(&owner, &agent).await;
    assert!(
        !system(&asked[0]).contains("Skills attached to you"),
        "{}",
        system(&asked[0])
    );
    assert!(!offered(&asked[0]).contains(&USE_SKILL.to_string()));

    let alpha = h
        .skill(
            &owner,
            "alpha",
            "Sort incoming\n  bugs by severity.",
            "Do alpha.",
        )
        .await;
    let beta = h.skill(&owner, "beta", "Second.", "Do beta.").await;
    h.switch(&owner, &beta, false).await;
    let (status, _) = h
        .attach(&owner, &agent, json!({ "attached": [alpha, beta] }))
        .await;
    assert_eq!(status, 200);

    let (_, _, asked) = h.turn(&owner, &agent).await;
    let said = system(&asked[0]);
    assert!(said.contains("Skills attached to you"), "{said}");
    assert!(
        said.contains("\n- `alpha`: Sort incoming bugs by severity."),
        "one line, the description folded onto it: {said}"
    );
    assert!(!said.contains("`beta`"), "{said}");
    let tools = &asked[0].tools;
    let use_skill: Vec<&Value> = tools
        .iter()
        .filter(|tool| tool["function"]["name"] == USE_SKILL)
        .collect();
    assert_eq!(use_skill.len(), 1, "{tools:?}");
    assert_eq!(
        use_skill[0]["function"]["parameters"]["properties"]["name"]["enum"],
        json!(["alpha"])
    );
    let listed = h.listed(&owner, &agent).await;
    let row: Vec<&Value> = listed
        .iter()
        .filter(|tool| tool["name"] == USE_SKILL)
        .collect();
    assert_eq!(
        row,
        vec![
            &json!({ "name": USE_SKILL, "kind": "builtin", "description": USE_SKILL_DESCRIPTION })
        ]
    );

    let (status, _) = h.attach(&owner, &agent, json!({ "attached": [] })).await;
    assert_eq!(status, 200);
    let (_, _, asked) = h.turn(&owner, &agent).await;
    assert!(!system(&asked[0]).contains("Skills attached to you"));
    assert!(!offered(&asked[0]).contains(&USE_SKILL.to_string()));
    let listed = h.listed(&owner, &agent).await;
    assert!(
        !listed.iter().any(|tool| tool["name"] == USE_SKILL),
        "{listed:?}"
    );
}

/// The recording NativeChat asked for: a turn calls `use_skill` and reads the skill's body back.
#[tokio::test]
async fn a_turn_reads_an_attached_skill_with_use_skill_and_gets_its_body() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, None).await;
    let owner = h.person("owner", true).await;
    let agent = h.hire(&owner).await;
    let body = "Sort the new bugs by severity, P0 first.";
    let triage = h.skill(&owner, "triage", "Sort incoming bugs.", body).await;
    let (status, _) = h
        .attach(&owner, &agent, json!({ "attached": [triage] }))
        .await;
    assert_eq!(status, 200);
    *h.door.reads.lock().unwrap() = Some("triage".to_string());

    let (run_id, sse, asked) = h.turn(&owner, &agent).await;
    assert_eq!(asked.len(), 2, "one round to read the skill, one to answer");
    assert_eq!(
        read_back(&asked),
        body,
        "their own words, whole, and no files to place"
    );
    assert!(sse.contains("\"toolCallName\":\"use_skill\""), "{sse}");
    assert!(
        sse.contains("TOOL_CALL_RESULT") && sse.contains("P0 first"),
        "{sse}"
    );

    // Read back as a client replays it, so the corpus keeps this turn whatever its shapes.
    let (status, replay) = h
        .call(&owner, "GET", &format!("/ag-ui/runs/{run_id}"), None)
        .await;
    assert_eq!(status, 200, "{replay}");
    assert!(replay.to_string().contains(USE_SKILL), "{replay}");
}

#[tokio::test]
async fn use_skill_with_a_name_the_turn_does_not_offer_is_refused_in_words() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, None).await;
    let owner = h.person("owner", true).await;
    let agent = h.hire(&owner).await;
    let triage = h
        .skill(&owner, "triage", "Sort incoming bugs.", "Sort them.")
        .await;
    let beta = h.skill(&owner, "beta", "Off.", "Do beta.").await;
    h.switch(&owner, &beta, false).await;
    let (status, _) = h
        .attach(&owner, &agent, json!({ "attached": [triage, beta] }))
        .await;
    assert_eq!(status, 200);

    // Attached, but switched off: not a name this turn offers, any more than one never attached.
    for name in ["beta", "nope"] {
        *h.door.reads.lock().unwrap() = Some(name.to_string());
        let (_, sse, asked) = h.turn(&owner, &agent).await;
        let refused = format!(
            "refused: no skill called {name:?} is attached for this turn; the ones that are: \
             `triage`"
        );
        // What the loop adds after any failed call is the harness's, not the tool's.
        let read = read_back(&asked);
        assert!(read.starts_with(&refused), "{read}");
        assert!(
            !sse.contains("RUN_ERROR"),
            "a refusal is a result, not a failed run: {sse}"
        );
    }
}

/// A member's turn on an org-shared coworker lists the skills its owner attached that the member
/// may use — the owner's own org skills, which `/skills` already shows them — and reads one in the
/// owner's words, said to be a colleague's.
#[tokio::test]
async fn a_members_turn_on_a_shared_coworker_gets_the_owners_attached_skill() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, None).await;
    let owner = h.person("owner", true).await;
    let member = h.person("member", true).await;
    let agent = h.hire(&owner).await;
    let (status, shared) = h
        .call(
            &owner,
            "PATCH",
            &format!("/coworkers/{agent}"),
            Some(json!({ "visibility": "org" })),
        )
        .await;
    assert_eq!(status, 200, "{shared}");
    let body = "File each bug under its owner.";
    let triage = h.skill(&owner, "triage", "Sort incoming bugs.", body).await;
    let (status, _) = h
        .attach(&owner, &agent, json!({ "attached": [triage] }))
        .await;
    assert_eq!(status, 200);

    *h.door.reads.lock().unwrap() = Some("triage".to_string());
    let (_, _, asked) = h.turn(&member, &agent).await;
    assert!(
        system(&asked[0]).contains("- `triage`: Sort incoming bugs."),
        "{}",
        system(&asked[0])
    );
    assert!(offered(&asked[0]).contains(&USE_SKILL.to_string()));
    let read = read_back(&asked);
    assert!(
        read.starts_with("A colleague in their organisation wrote these instructions"),
        "{read}"
    );
    assert!(read.contains(body), "{read}");
    // The set is still the owner's alone to read.
    let (status, _) = h
        .call(&member, "GET", &format!("/coworkers/{agent}/skills"), None)
        .await;
    assert_eq!(status, 404);
}

/// `use_skill` puts a skill's files on the coworker's computer as choosing it with `/name` does,
/// and says where.
#[tokio::test]
async fn use_skill_puts_the_skills_files_on_the_computer_and_says_where() {
    let database_url = database_or_skip!();
    let disk = Arc::new(DiskBox::default());
    let h = harness(&database_url, Some(disk.clone())).await;
    let owner = h.person("owner", true).await;
    let agent = h.hire(&owner).await;
    let checks = h
        .skill(&owner, "checks", "Run the checks.", "Run check.sh.")
        .await;
    let script = "#!/bin/sh\necho ok\n";
    let files = json!([{ "path": "check.sh",
        "bytes": base64::engine::general_purpose::STANDARD.encode(script) }]);
    let (status, added) = h
        .call(
            &owner,
            "POST",
            &format!("/skills/{checks}/versions"),
            Some(json!({ "body": "Run check.sh first.", "files": files })),
        )
        .await;
    assert_eq!(status, 200, "{added}");
    let (status, _) = h
        .attach(&owner, &agent, json!({ "attached": [checks] }))
        .await;
    assert_eq!(status, 200);
    *h.door.reads.lock().unwrap() = Some("checks".to_string());

    let (_, _, asked) = h.turn(&owner, &agent).await;
    let read = read_back(&asked);
    let dir = format!("/home/box/.skills/checks/{checks}/v2");
    assert!(
        read.starts_with("Run check.sh first."),
        "the newest body: {read}"
    );
    assert!(read.contains(&format!("`{dir}/`")), "{read}");
    assert_eq!(
        disk.read_file("bx", &format!("{dir}/check.sh"))
            .await
            .expect("on the box"),
        script
    );
}
