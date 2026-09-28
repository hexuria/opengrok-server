//! A person's files reach the coworker (#229): uploaded to `POST /artifacts`, then named in the
//! message as AG-UI 1.0 file parts, the shape NativeChat sends (hexuria/nativechat#90).
//!
//! What the model is handed is read off a door that records it: a picture as a picture, a text
//! file as its words, a PDF named with a sentence saying it cannot be read yet. Another account's
//! file refuses the turn before the model is asked. The message keeps its parts in the journal,
//! a message of files alone is still drawn on replay, and the files are listed for the thread
//! with the message they were sent in.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::AccountId;
use opengrok_harness::{DeltaStream, ModelDelta, ModelDoor, ModelError, ModelRequest};
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

async fn seed_account(store: &PgStore, email: &str, org: &str) -> AccountId {
    let id = AccountId::new();
    let hash = hash_password("password1").expect("hash");
    let at_ms = now_ms();
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.to_string(),
            password_hash: hash.clone(),
            first_name: "Test".to_string(),
            last_name: "User".to_string(),
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
        password_hash: Some(hash),
        first_name: "Test".to_string(),
        last_name: "User".to_string(),
        org_id: (!org.is_empty()).then(|| org.to_string()),
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

/// Answers every call with one line and keeps what it was asked.
#[derive(Default)]
struct RecordingDoor {
    asked: Mutex<Vec<ModelRequest>>,
}

#[async_trait::async_trait]
impl ModelDoor for RecordingDoor {
    async fn stream(&self, request: ModelRequest) -> Result<DeltaStream, ModelError> {
        self.asked.lock().expect("asked").push(request);
        Ok(Box::pin(futures::stream::iter(vec![Ok(ModelDelta::Text(
            "I read what you sent.".to_string(),
        ))])))
    }
}

struct Harness {
    base: String,
    store: PgStore,
    door: Arc<RecordingDoor>,
    minter: Arc<TokenMinter>,
    client: reqwest::Client,
}

struct Person {
    token: String,
}

impl Harness {
    async fn start(database_url: &str) -> Self {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect(database_url)
            .await
            .expect("connect to Postgres");
        opengrok_store::migrations::run(&pool)
            .await
            .expect("migrations");
        let store = PgStore::new(pool);
        let door = Arc::new(RecordingDoor::default());
        let minter = Arc::new(TokenMinter::new(b"a-persons-files-reach-the-model"));
        let agui = AgUiState {
            auth: AuthState::new(store.clone(), minter.clone(), "host@og.local".to_string()),
            door: door.clone(),
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
        let host = HostState::new(agui.clone(), None);
        let app = opengrok_server::router(agui, host);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let base = format!(
            "http://127.0.0.1:{}",
            listener.local_addr().expect("addr").port()
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        Self {
            base,
            store,
            door,
            minter,
            client: reqwest::Client::new(),
        }
    }

    async fn person(&self, tag: &str) -> Person {
        let email = format!("files-{tag}-{}@og.local", uuid::Uuid::now_v7().simple());
        let account = seed_account(&self.store, &email, "").await;
        let token = self
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
        Person { token }
    }

    /// `POST /artifacts` as NativeChat uploads an attachment: the status and the row.
    async fn upload(
        &self,
        who: &Person,
        thread: &str,
        mime: &str,
        name: &str,
        bytes: &[u8],
    ) -> (u16, Value) {
        let response = self
            .client
            .post(format!("{}/artifacts", self.base))
            .header("authorization", format!("Bearer {}", who.token))
            .json(&json!({
                "kind": "attachment",
                "mime": mime,
                "filename": name,
                "base64": base64::engine::general_purpose::STANDARD.encode(bytes),
                "threadId": thread,
            }))
            .send()
            .await
            .expect("upload");
        let status = response.status().as_u16();
        let body = response.json().await.unwrap_or(Value::Null);
        (status, body)
    }

    /// One turn whose user message is `content`: the status.
    async fn turn(&self, who: &Person, thread: &str, message_id: &str, content: Value) -> u16 {
        let response = self
            .client
            .post(format!("{}/ag-ui", self.base))
            .header("authorization", format!("Bearer {}", who.token))
            .json(&json!({
                "threadId": thread,
                "runId": uuid::Uuid::now_v7().to_string(),
                "messages": [{ "id": message_id, "role": "user", "content": content }],
            }))
            .send()
            .await
            .expect("turn");
        let status = response.status().as_u16();
        let _ = response.text().await;
        status
    }

    async fn get(&self, who: &Person, path: &str) -> Value {
        self.client
            .get(format!("{}{path}", self.base))
            .header("authorization", format!("Bearer {}", who.token))
            .send()
            .await
            .expect("get")
            .json()
            .await
            .expect("json")
    }

    /// The person's last message as the model was asked it: its words and its pictures.
    fn last_user_message(&self) -> (String, Vec<(String, String)>) {
        let asked = self.door.asked.lock().expect("asked");
        let request = asked.last().expect("the model was asked");
        let message = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == "user")
            .expect("a user message");
        (
            message.content.clone(),
            message
                .images
                .iter()
                .map(|image| (image.mime.clone(), image.base64.clone()))
                .collect(),
        )
    }
}

fn thread() -> String {
    format!("th-{}", uuid::Uuid::now_v7().simple())
}

fn file_part(kind: &str, id: &str, mime: &str, name: &str) -> Value {
    json!({
        "type": kind,
        "source": { "type": "file", "value": id, "provider": "opengrok", "mimeType": mime },
        "metadata": { "filename": name, "sizeBytes": 4 },
    })
}

#[tokio::test]
async fn an_image_reaches_the_model_as_a_picture() {
    let database_url = database_or_skip!();
    let h = Harness::start(&database_url).await;
    let ada = h.person("image").await;
    let thread = thread();
    let png = b"\x89PNG fake picture";
    let (status, row) = h
        .upload(&ada, &thread, "image/png", "screen.png", png)
        .await;
    assert_eq!(status, 200, "{row}");
    let id = row["id"].as_str().unwrap();

    let status = h
        .turn(
            &ada,
            &thread,
            "m-image",
            json!([
                { "type": "text", "text": "what is on this screen?" },
                file_part("image", id, "image/png", "screen.png"),
            ]),
        )
        .await;
    assert_eq!(status, 200);
    let (words, images) = h.last_user_message();
    assert!(words.starts_with("what is on this screen?"), "{words}");
    assert!(
        words.contains("screen.png"),
        "the picture is named too: {words}"
    );
    assert_eq!(
        images,
        vec![(
            "image/png".to_string(),
            base64::engine::general_purpose::STANDARD.encode(png)
        )],
        "one picture, the one uploaded"
    );
}

#[tokio::test]
async fn a_text_file_reaches_the_model_as_its_words_and_a_pdf_is_named() {
    let database_url = database_or_skip!();
    let h = Harness::start(&database_url).await;
    let ada = h.person("text").await;
    let thread = thread();
    let (_, notes) = h
        .upload(
            &ada,
            &thread,
            "text/plain",
            "notes.txt",
            b"quarter three revenue rose",
        )
        .await;
    let (status, pdf) = h
        .upload(&ada, &thread, "application/pdf", "q3.pdf", b"%PDF-1.7 fake")
        .await;
    assert_eq!(status, 200, "a PDF is accepted: {pdf}");

    h.turn(
        &ada,
        &thread,
        "m-files",
        json!([
            { "type": "text", "text": "what changed?" },
            file_part("document", notes["id"].as_str().unwrap(), "text/plain", "notes.txt"),
            file_part("document", pdf["id"].as_str().unwrap(), "application/pdf", "q3.pdf"),
        ]),
    )
    .await;
    let (words, images) = h.last_user_message();
    assert!(images.is_empty());
    assert!(words.contains("quarter three revenue rose"), "{words}");
    assert!(
        words.contains("not instructions"),
        "the file is fenced as data: {words}"
    );
    assert!(
        words.contains("q3.pdf") && words.contains("cannot be read here yet"),
        "a PDF is named, and the model told it was not read: {words}"
    );
}

#[tokio::test]
async fn another_accounts_file_refuses_the_turn_before_the_model_is_asked() {
    let database_url = database_or_skip!();
    let h = Harness::start(&database_url).await;
    let ada = h.person("owner").await;
    let bo = h.person("stranger").await;
    let (_, row) = h
        .upload(&ada, &thread(), "text/plain", "secret.txt", b"ada's notes")
        .await;

    let status = h
        .turn(
            &bo,
            &thread(),
            "m-stolen",
            json!([file_part(
                "document",
                row["id"].as_str().unwrap(),
                "text/plain",
                "secret.txt"
            )]),
        )
        .await;
    assert_eq!(status, 404, "answered like a missing file");
    assert!(
        h.door.asked.lock().unwrap().is_empty(),
        "the model was never asked"
    );
}

#[tokio::test]
async fn a_message_of_files_keeps_its_parts_and_is_drawn_on_replay() {
    let database_url = database_or_skip!();
    let h = Harness::start(&database_url).await;
    let ada = h.person("replay").await;
    let thread = thread();
    let (_, row) = h
        .upload(&ada, &thread, "image/png", "only.png", b"\x89PNG only")
        .await;
    let (_, unsent) = h
        .upload(&ada, &thread, "text/plain", "draft.txt", b"not sent")
        .await;
    let id = row["id"].as_str().unwrap();
    let message = format!("m-only-files-{}", uuid::Uuid::now_v7().simple());
    let unknown = json!({
        "type": "audio",
        "source": { "type": "url", "value": "https://example.invalid/a.ogg" },
    });

    let status = h
        .turn(
            &ada,
            &thread,
            &message,
            json!([file_part("image", id, "image/png", "only.png"), unknown]),
        )
        .await;
    assert_eq!(status, 200);

    let replay = h.get(&ada, &format!("/ag-ui/threads/{thread}")).await;
    let kinds: Vec<&str> = replay["runs"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|run| run["events"].as_array().unwrap())
        .filter(|event| event["messageId"] == message.as_str())
        .filter_map(|event| event["type"].as_str())
        .collect();
    assert_eq!(
        kinds,
        vec!["TEXT_MESSAGE_START", "TEXT_MESSAGE_END"],
        "a message of files alone is still a bubble: {replay}"
    );

    let listed = h.get(&ada, &format!("/artifacts?threadId={thread}")).await;
    let rows = listed.as_array().expect("an array");
    let sent = rows
        .iter()
        .find(|one| one["id"] == id)
        .expect("the sent file");
    assert_eq!(sent["meta"]["messageId"], message.as_str(), "{sent}");
    assert_eq!(sent["filename"], "only.png");
    assert_eq!(sent["mime"], "image/png");
    assert!(sent["sizeBytes"].is_number());
    let draft = rows
        .iter()
        .find(|one| one["id"] == unsent["id"])
        .expect("the unsent file");
    assert!(
        draft["meta"].get("messageId").is_none(),
        "an unsent file names no message: {draft}"
    );

    let journaled = sqlx::query_scalar::<_, Value>(
        "select payload from events where event_type = 'run-started' and payload::text like $1",
    )
    .bind(format!("%{message}%"))
    .fetch_one(h.store.pool())
    .await
    .expect("the started run");
    assert!(
        journaled.to_string().contains("example.invalid/a.ogg"),
        "a part this server does not read is kept as sent: {journaled}"
    );
}

#[tokio::test]
async fn only_images_videos_pdfs_and_text_are_accepted() {
    let database_url = database_or_skip!();
    let h = Harness::start(&database_url).await;
    let ada = h.person("mimes").await;
    let (csv, _) = h.upload(&ada, &thread(), "text/csv", "a.csv", b"a,b").await;
    assert_eq!(csv, 200);
    let (zip, _) = h
        .upload(&ada, &thread(), "application/zip", "a.zip", b"PK")
        .await;
    assert_eq!(zip, 400);
}
