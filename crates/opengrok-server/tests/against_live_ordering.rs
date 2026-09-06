//! The two rules the desktop's replica imposes on `/events` stamps, proven over a real socket.
//!
//! 1. A frame written to one subscriber must not consume a sequence. Opening a second stream
//!    used to mint a roster sequence for its private snapshot, so the first stream saw N, N+2 and
//!    resynced the roster after every send. Here: open A, open B, emit one roster frame, and A's
//!    frame is exactly A's opening sequence + 1.
//! 2. Sequences reach the broadcast in the order they were minted, even when many tasks emit on
//!    one agent at once. Here: fifty tasks race `emit_transcript` on one agent and one subscriber
//!    receives 1..=50 in order.
//!
//! SINCE #58 BOTH RULES ARE PER ACCOUNT. Stamped frames are addressed — one account's roster and
//! transcript no longer reach every open stream — so these tests need a REAL account and a REAL
//! coworker to address, and a frame with no resolvable owner is now correctly emitted to nobody.
//! The counter is per (replicaKey, account) while the wire `replicaKey` is unchanged, so a
//! contiguous run per account is exactly what rule 1 still asserts.
//!
//! Needs Postgres (the state carries the store), so it skips — loudly — when OG_DATABASE_URL is
//! absent, the same bargain the other integration tests make.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::coworker::{Coworker, CoworkerCommand, CoworkerView};
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::gateway::{GatewayState, live};
use opengrok_store::PgStore;

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

/// The account these streams belong to. Before #58 the frames went to everyone and no account had
/// to exist; now a frame is addressed, so an unseeded deployment email emits to nobody and every
/// assertion here would wait forever for something correctly never sent.
async fn seed_account(store: &PgStore, email: &str) -> AccountId {
    if let Ok(Some(existing)) = store.account_by_email(email).await {
        return existing.id;
    }
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
    // IDEMPOTENT UNDER A RACE. Every test in this file calls `state()`, which seeds this same
    // deployment email, and they run in parallel — so the existence check above can pass in two
    // tests at once and the loser's append hits a Conflict. Losing that race is fine: what the
    // caller wants is the id, not the honour of having written it.
    match store.append_account(&id, 0, &events, &view).await {
        Ok(_) => id,
        Err(_) => store
            .account_by_email(email)
            .await
            .expect("lookup after a lost race")
            .expect("somebody else seeded it")
            .id,
    }
}

/// The coworker the racing transcript emits address. `audience_for` reads its owner, so without a
/// real row the fifty frames are emitted to an empty audience — which is correct behaviour and a
/// hanging test.
async fn seed_coworker(store: &PgStore, account: &AccountId, name: &str) -> CoworkerId {
    let id = CoworkerId::new();
    let mut coworker = Coworker::default();
    let events = Coworker::default()
        .decide(CoworkerCommand::Hire {
            name: name.to_string(),
            model: "oag/cheap".to_string(),
            at_ms: 1,
        })
        .expect("hire");
    for event in &events {
        coworker.apply(event);
    }
    let view = CoworkerView {
        id: id.clone(),
        name: coworker.name.clone(),
        model: coworker.model.clone(),
        box_id: None,
        retired: false,
        members: Vec::new(),
        updated_at_ms: 2,
        role: None,
        visibility: Default::default(),
    };
    store.append_coworker(&id, account, 0, &events, &view).await.expect("append coworker");
    id
}

async fn state(database_url: &str) -> (axum::Router, GatewayState) {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    seed_account(&store, "host@og.local").await;
    let auth = AuthState::new(
        store,
        Arc::new(TokenMinter::new(b"live-ordering-secret")),
        "host@og.local".to_string(),
    );
    let agui = AgUiState {
        auth,
        door: Arc::new(MockDoor::echoing()),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer: None,
        vault: None,
        connectors: Connectors {
            providers: Arc::new(BTreeMap::new()),
            redirect_uri: "http://127.0.0.1/callback".to_string(),
        },
        plugins: Arc::new(BTreeMap::new()),
    };
    let gateway = GatewayState::new(
        agui.clone(),
        Some("test-bearer".to_string()),
        "host@og.local".to_string(),
        Some("http://opengrok.lan:1447".to_string()),
    )
    // These tests are about ORDERING, not identity: their streams speak as the deployment
    // account, which since 5 Sep 2026 must be asked for rather than assumed.
    .allowing_identity_fallback();
    (opengrok_server::router(agui, gateway.clone()), gateway)
}

async fn spawn(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://127.0.0.1:{}", addr.port())
}

/// Open `/events` on the `agents` channel and hand back the response to read frames from.
async fn open_events(client: &reqwest::Client, base: &str) -> reqwest::Response {
    let res = client
        .get(format!("{base}/events?channels=agents"))
        .header("authorization", "Bearer test-bearer")
        .header("accept", "text/event-stream")
        .send()
        .await
        .expect("events");
    assert_eq!(res.status(), 200);
    res
}

/// Read chunks until a `data:` frame appears; return its JSON payload.
async fn next_frame(res: &mut reqwest::Response, buffer: &mut String) -> serde_json::Value {
    loop {
        if let Some(start) = buffer.find("data: ")
            && let Some(end) = buffer[start..].find("\n\n")
        {
            let line = buffer[start + 6..start + end].to_string();
            buffer.replace_range(..start + end + 2, "");
            return serde_json::from_str(&line).expect("frame json");
        }
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), res.chunk())
            .await
            .expect("a frame within 5s")
            .expect("chunk")
            .expect("bytes");
        buffer.push_str(&String::from_utf8_lossy(&chunk));
    }
}

fn roster_sequence(frame: &serde_json::Value) -> i64 {
    assert_eq!(
        frame["payload"]["ordered"]["replicaKey"], "roster",
        "{frame}"
    );
    frame["payload"]["ordered"]["sequence"]
        .as_i64()
        .expect("sequence")
}

#[tokio::test]
async fn opening_a_second_stream_leaves_no_gap_on_the_first() {
    let database_url = database_or_skip!();
    let (app, gateway) = state(&database_url).await;
    let base = spawn(app).await;
    let client = reqwest::Client::new();

    // Something has been emitted before anyone connects, so the opener has a real number to
    // report rather than zero.
    live::emit_roster_for_caller(&gateway, "host@og.local").await;

    let mut a = open_events(&client, &base).await;
    let mut a_buf = String::new();
    let a_open = next_frame(&mut a, &mut a_buf).await;
    assert_eq!(a_open["payload"]["coverage"]["kind"], "complete-roster");
    let a_seq = roster_sequence(&a_open);

    // A second subscriber opens: its private snapshot must not consume a sequence.
    let mut b = open_events(&client, &base).await;
    let mut b_buf = String::new();
    let b_open = next_frame(&mut b, &mut b_buf).await;
    assert_eq!(
        roster_sequence(&b_open),
        a_seq,
        "the opener reports the current sequence, not a fresh one"
    );

    // One real roster emit: A sees exactly the next number, B too.
    live::emit_roster_for_caller(&gateway, "host@og.local").await;
    let a_next = next_frame(&mut a, &mut a_buf).await;
    assert_eq!(roster_sequence(&a_next), a_seq + 1, "A saw a gap: {a_next}");
    let b_next = next_frame(&mut b, &mut b_buf).await;
    assert_eq!(roster_sequence(&b_next), a_seq + 1);
}

#[tokio::test]
async fn concurrent_emits_on_one_agent_arrive_in_sequence_order() {
    let database_url = database_or_skip!();
    let (_, gateway) = state(&database_url).await;
    let store = &gateway.agui.auth.store;
    let account = seed_account(store, "host@og.local").await;
    // A REAL coworker: `audience_for` resolves the owner off this row, and a made-up id now
    // addresses nobody — which is the fix working, and would hang this test forever.
    let racer = seed_coworker(store, &account, "Racer").await;
    let racer_id = racer.as_str().to_string();
    let mut subscriber = gateway.events_tx.subscribe();

    let mut tasks = Vec::new();
    for i in 0..50 {
        let gateway = gateway.clone();
        let racer_id = racer_id.clone();
        tasks.push(tokio::spawn(async move {
            live::emit_transcript(
                &gateway,
                &racer_id,
                "appended",
                serde_json::json!({ "id": format!("e{i}") }),
            )
            .await;
        }));
    }
    for task in tasks {
        task.await.expect("task");
    }

    let mut seen = Vec::new();
    for _ in 0..50 {
        let live = subscriber.recv().await.expect("frame");
        assert_eq!(live.channel, "transcript");
        // #58 INVERTED THIS. It read `audience == None`, quoting the reasoning that filtering per
        // person would spend sequence numbers other streams expect. That was the deferral's
        // argument and it was wrong: the counter went per (replicaKey, account) while the wire
        // `replicaKey` stayed the transcribed value, so each account keeps a contiguous run and
        // nobody sees a gap. Until then every transcript frame — message content included — went
        // to every open stream.
        assert_eq!(
            live.audience.as_ref(),
            Some(&account),
            "a stamped transcript frame is addressed to the coworker's owner"
        );
        assert_eq!(
            live.payload["ordered"]["replicaKey"],
            format!("transcript:{racer_id}"),
            "the wire replicaKey is unchanged — the per-account split is the server's counter"
        );
        seen.push(
            live.payload["ordered"]["sequence"]
                .as_i64()
                .expect("sequence"),
        );
    }
    let expected: Vec<i64> = (1..=50).collect();
    assert_eq!(
        seen, expected,
        "frames reached the broadcast out of mint order"
    );
}

/// A frame built from ONE person's data reaches that person's stream and nobody else's.
///
/// The bus is a broadcast and the stream filtered by channel NAME only, so a routine belonging
/// to Ada was delivered to Bo's and Cass's streams too. The client declined to render an agent
/// it did not recognise, which is the only reason it never showed — obscurity, not a check, and
/// it stopped being even that once a coworker could be shared.
///
/// Asserted at the bus rather than over HTTP because that is where the audience lives; the
/// stream's own drop is the three-line consequence of it.
#[tokio::test]
async fn an_addressed_frame_is_not_broadcast_to_everybody() {
    let database_url = database_or_skip!();
    let (_, state) = state(&database_url).await;
    let ada = opengrok_core::id::AccountId::new();
    let bo = opengrok_core::id::AccountId::new();

    let mut subscriber = state.events_tx.subscribe();

    live::emit_unstamped_to(
        &state,
        "agents-automation",
        serde_json::json!({ "agentId": "cw_ada", "automations": ["ada's weekly report"] }),
        &ada,
    );
    live::emit_unstamped(
        &state,
        "agents-automation",
        serde_json::json!({ "agentId": "cw_any", "automations": [] }),
    );

    let addressed = subscriber.recv().await.expect("frame");
    assert_eq!(
        addressed.audience.as_ref(),
        Some(&ada),
        "a frame built from one person's routines names them"
    );
    assert_ne!(addressed.audience.as_ref(), Some(&bo), "and it is not Bo's");

    let unaddressed = subscriber.recv().await.expect("frame");
    assert_eq!(
        unaddressed.audience, None,
        "a frame whose payload names nobody still goes to everybody — the audience is a \
         restriction, not a requirement"
    );
}
