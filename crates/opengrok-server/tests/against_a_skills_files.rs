//! A chosen skill's bundled files are on the coworker's computer when the model reads the
//! instructions that name them (#192).
//!
//! `/skills` accepted up to 32 files beside the body and stored them, and nothing ever copied
//! them anywhere: a skill saying "run scripts/check.sh" sent the model to a file that did not
//! exist, and it reported it missing or invented what it said.
//!
//! The computer is a stand-in that keeps what is written to it, so a test can read back what a
//! `read_file` on the box would. The door records the one system message, so the sentence the
//! model is given about the files — where they are, or that they are not there — is asserted as
//! sent. Needs Postgres; skips loudly without OG_DATABASE_URL. No Docker daemon.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine as _;
use opengrok_box::{BoxResult, CommandOutput, Computer, StartedCommand};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::AccountId;
use opengrok_harness::{DeltaStream, ModelDelta, ModelDoor, ModelError, ModelRequest};
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::host_state::HostState;
use opengrok_server::persona::{SKILL_CLOSING_LINE, SKILL_FILES_UNAVAILABLE};
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

#[derive(Default)]
struct RecordingDoor {
    systems: Mutex<Vec<String>>,
}

#[async_trait]
impl ModelDoor for RecordingDoor {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
        let said = request.system.clone().unwrap_or_default();
        self.systems.lock().unwrap().push(said);
        Ok(Box::pin(futures::stream::iter(vec![Ok(ModelDelta::Text(
            "done".to_string(),
        ))])))
    }
}

/// A computer that keeps its files in memory and answers the two shell questions a placement
/// asks — where home is, and what the directory's marker says — the way a shell would.
struct DiskBox {
    state: String,
    files: Mutex<BTreeMap<String, String>>,
    writes: Mutex<Vec<String>>,
    commands: Mutex<Vec<String>>,
}

impl DiskBox {
    fn new(state: &str) -> Self {
        Self {
            state: state.to_string(),
            files: Mutex::new(BTreeMap::new()),
            writes: Mutex::new(Vec::new()),
            commands: Mutex::new(Vec::new()),
        }
    }
    fn writes(&self) -> Vec<String> {
        self.writes.lock().unwrap().clone()
    }
}

fn said(stdout: &str) -> CommandOutput {
    CommandOutput {
        exit_code: 0,
        stdout: stdout.to_string(),
        stderr: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
        timed_out: false,
    }
}

#[async_trait]
impl Computer for DiskBox {
    async fn create(&self, _ttl_seconds: Option<u64>) -> BoxResult<String> {
        Ok(format!("bx_disk_{}", uuid::Uuid::now_v7().simple()))
    }
    async fn run(&self, _box_id: &str, command: &str, _timeout: u32) -> BoxResult<CommandOutput> {
        self.commands.lock().unwrap().push(command.to_string());
        if command.contains("$HOME") {
            return Ok(said("/home/box"));
        }
        if let Some(path) = command
            .strip_prefix("cat '")
            .and_then(|rest| rest.split_once('\''))
            .map(|(path, _)| path)
        {
            let files = self.files.lock().unwrap();
            return Ok(said(files.get(path).map(String::as_str).unwrap_or("")));
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
        Ok(said(""))
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
        self.writes.lock().unwrap().push(path.to_string());
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
        Ok(self.state.clone())
    }
}

async fn seed_account(store: &PgStore, email: &str) -> AccountId {
    let id = AccountId::new();
    let at_ms = chrono::Utc::now().timestamp_millis();
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.to_string(),
            password_hash: "x".to_string(),
            first_name: "Files".to_string(),
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
        first_name: "Files".to_string(),
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
    state: AgUiState,
    door: Arc<RecordingDoor>,
    client: reqwest::Client,
    token: String,
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
    let door = Arc::new(RecordingDoor::default());
    let state = AgUiState {
        auth: AuthState::new(
            PgStore::new(pool),
            Arc::new(TokenMinter::new(b"skill-files-test-secret")),
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
    let email = format!("files-{}@og.local", uuid::Uuid::now_v7().simple());
    let account = seed_account(&state.auth.store, &email).await;
    let token = state
        .auth
        .minter
        .mint_access(
            account.as_str(),
            "sess-files",
            &email,
            "ultra",
            chrono::Utc::now().timestamp(),
            3600,
        )
        .expect("mint access");
    let gateway = HostState::new(state.clone(), None);
    let app = opengrok_server::router(state.clone(), gateway);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    Harness {
        base: format!("http://127.0.0.1:{}", addr.port()),
        state,
        door,
        client: reqwest::Client::new(),
        token,
    }
}

impl Harness {
    async fn post(&self, path: &str, body: Value) -> Value {
        let response = self
            .client
            .post(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .expect("post");
        let status = response.status().as_u16();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        assert!(status == 200 || status == 201, "{path}: {status} {body}");
        body
    }

    async fn hire(&self) -> String {
        self.post("/coworkers", json!({ "name": "Files" })).await["id"]
            .as_str()
            .expect("id")
            .to_string()
    }

    /// A skill whose one version carries `files` (path, bytes). Returns its id and name.
    async fn skill(&self, body: &str, files: &[(&str, &[u8])]) -> (String, String) {
        let name = format!("checks-{}", uuid::Uuid::now_v7().simple());
        let made = self.post("/skills", json!({ "name": name })).await;
        let id = made["id"].as_str().expect("id").to_string();
        let files: Vec<Value> = files
            .iter()
            .map(|(path, bytes)| {
                json!({
                    "path": path,
                    "bytes": base64::engine::general_purpose::STANDARD.encode(bytes),
                })
            })
            .collect();
        self.post(
            &format!("/skills/{id}/versions"),
            json!({ "body": body, "files": files }),
        )
        .await;
        (id, name)
    }

    /// One turn with the skill chosen; the system message the model was handed.
    async fn turn(&self, coworker: &str, skill: &str) -> String {
        let before = self.door.systems.lock().unwrap().len();
        let sse = self
            .client
            .post(format!("{}/ag-ui", self.base))
            .bearer_auth(&self.token)
            .json(&json!({
                "threadId": format!("thr-{}", uuid::Uuid::now_v7()),
                "runId": uuid::Uuid::now_v7().to_string(),
                "messages": [{ "id": "m1", "role": "user", "content": "check it" }],
                "forwardedProps": { "coworkerId": coworker, "skill": skill },
            }))
            .send()
            .await
            .expect("turn")
            .text()
            .await
            .expect("sse");
        assert!(
            sse.contains("RUN_FINISHED"),
            "a turn is never failed by files: {sse}"
        );
        let systems = self.door.systems.lock().unwrap();
        assert_eq!(systems.len(), before + 1, "one model call");
        systems.last().cloned().unwrap_or_default()
    }
}

const SCRIPT: &str = "#!/bin/sh\necho ok\n";

#[tokio::test]
async fn a_chosen_skills_files_are_on_the_computer_before_the_model_reads_it() {
    let database_url = database_or_skip!();
    let disk = Arc::new(DiskBox::new("running"));
    let h = harness(&database_url, Some(disk.clone())).await;
    let coworker = h.hire().await;
    let (id, name) = h
        .skill(
            "Run scripts/check.sh first.",
            &[
                ("scripts/check.sh", SCRIPT.as_bytes()),
                ("reference/sheet.bin", &[0xC3, 0x28, 0xFF]),
            ],
        )
        .await;

    let system = h.turn(&coworker, &id).await;

    let dir = format!("/home/box/.skills/{name}/v1");
    let script = format!("{dir}/scripts/check.sh");
    assert_eq!(
        disk.read_file("bx", &script)
            .await
            .expect("the file is on the box"),
        SCRIPT
    );
    assert!(
        disk.commands
            .lock()
            .unwrap()
            .iter()
            .any(|command| command.starts_with("chmod +x") && command.contains(&script)),
        "a script with a shebang is made runnable"
    );
    assert!(
        system.contains(&format!("`{dir}/`")),
        "the skill line names where the files are: {system}"
    );
    assert!(
        system.contains("1 more could not be copied"),
        "the binary file is counted, not hidden: {system}"
    );
    assert!(
        !system.contains("reference/sheet.bin"),
        "bundle paths are the author's words and stay out of ours: {system}"
    );
    assert!(
        system.ends_with(SKILL_CLOSING_LINE),
        "the closing restatement is still the last word: {system}"
    );

    // The same version again writes nothing: the directory's digest already matches.
    let written = disk.writes().len();
    let again = h.turn(&coworker, &id).await;
    assert_eq!(disk.writes().len(), written, "{:?}", disk.writes());
    assert!(again.contains(&format!("`{dir}/`")), "{again}");
}

#[tokio::test]
async fn a_skill_with_files_and_no_computer_says_they_are_unavailable() {
    let database_url = database_or_skip!();
    let h = harness(&database_url, None).await;
    let coworker = h.hire().await;
    let (id, _) = h
        .skill(
            "Run scripts/check.sh first.",
            &[("scripts/check.sh", SCRIPT.as_bytes())],
        )
        .await;

    let system = h.turn(&coworker, &id).await;

    assert!(system.contains("Run scripts/check.sh first."), "{system}");
    assert!(system.contains(SKILL_FILES_UNAVAILABLE), "{system}");
    assert!(
        !system.contains(".skills/"),
        "no path that is not there: {system}"
    );
    assert!(system.ends_with(SKILL_CLOSING_LINE), "{system}");
}

#[tokio::test]
async fn an_asleep_computer_is_not_woken_for_a_skills_files() {
    let database_url = database_or_skip!();
    let disk = Arc::new(DiskBox::new("exited"));
    let h = harness(&database_url, Some(disk.clone())).await;
    let coworker = h.hire().await;
    let (id, _) = h
        .skill("Run the check.", &[("check.sh", SCRIPT.as_bytes())])
        .await;

    let system = h.turn(&coworker, &id).await;

    assert!(system.contains(SKILL_FILES_UNAVAILABLE), "{system}");
    assert!(disk.writes().is_empty(), "{:?}", disk.writes());
}

#[tokio::test]
async fn a_skill_without_files_asks_nothing_of_the_computer() {
    let database_url = database_or_skip!();
    let disk = Arc::new(DiskBox::new("running"));
    let h = harness(&database_url, Some(disk.clone())).await;
    let coworker = h.hire().await;
    let (id, _) = h.skill("Be brief.", &[]).await;

    let system = h.turn(&coworker, &id).await;

    assert!(!system.contains("came with"), "{system}");
    assert!(disk.commands.lock().unwrap().is_empty());
    assert!(disk.writes().is_empty());
    assert!(
        h.state
            .auth
            .store
            .skill(&id)
            .await
            .expect("skill")
            .is_some()
    );
}
