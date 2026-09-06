//! A restart mid-answer must not leave the person watching a coworker type forever.
//!
//! `sendPrompt` appends an entry marked `streaming: true` before the turn starts and clears the
//! flag when it finishes. A process that dies in between leaves it set with nothing coming to
//! clear it — and the packaged client has NO timeout for that state: it keeps an empty bubble on
//! screen with typing dots and `aria-busy` for as long as the row says so (`hasText =
//! content.trim().length > 0 || streaming`, confirmed against the app by the client session).
//! Recovery used to fail the RUN and stop there, which is a half-fix that looks complete from the
//! server's own logs while the only escape for the person is a new conversation.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, CoworkerId, RunId};
use opengrok_core::run::{RunCommand, RunStatus, RunView};
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::auth::password::hash_password;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_store::PgStore;
use serde_json::json;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

async fn seed_account(store: &PgStore, email: &str) -> AccountId {
    let id = AccountId::new();
    let hash = hash_password("password1").expect("hash");
    let at_ms = now_ms();
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.to_string(),
            password_hash: hash.clone(),
            first_name: "Test".to_string(),
            last_name: "User".to_string(),
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
        password_hash: Some(hash),
        first_name: "Test".to_string(),
        last_name: "User".to_string(),
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

async fn state(store: PgStore, email: &str) -> AgUiState {
    AgUiState {
        auth: AuthState::new(
            store,
            Arc::new(TokenMinter::new(b"abandoned-bubble-secret")),
            email.to_string(),
        ),
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
    }
}

/// Start a run and abandon it: journal one batch, then never write again. `claim_abandoned_runs`
/// treats silence older than the lease as abandonment, so the run is dated into the past rather
/// than waited for — a test that slept out a 60 s lease is a test nobody runs.
async fn abandon_a_run(
    store: &PgStore,
    coworker: &CoworkerId,
    account: &AccountId,
    thread_id: &str,
) -> RunId {
    let run_id = RunId::new();
    let mut run = opengrok_core::run::Run::default();
    let events = run
        .decide(RunCommand::Start {
            thread_id: thread_id.to_string(),
            coworker_id: Some(coworker.clone()),
            model: Some("oag/cheap".to_string()),
            system: None,
            at_ms: now_ms(),
        })
        .expect("start");
    for event in &events {
        run.apply(event);
    }
    // Old enough that the lease has certainly lapsed; the sweep reads this stamp, not a clock.
    let stale = now_ms() - (opengrok_server::recovery::LEASE_MS * 4);
    let view = RunView {
        id: run_id.clone(),
        thread_id: thread_id.to_string(),
        status: RunStatus::Running,
        event_count: run.emitted.len() as i64,
        updated_at_ms: stale,
    };
    store
        .append_run(&run_id, 0, &events, &view, Some(account))
        .await
        .expect("append run");
    run_id
}

/// Sweep until THIS run is settled, rather than once.
///
/// `sweep_once` claims a bounded batch of whatever is abandoned database-wide (`CLAIM_LIMIT`), and
/// a shared test database accumulates stale runs from every earlier suite — so a single sweep is
/// not guaranteed to include ours, and asserting on its return count tests the fixture rather than
/// the code. Sweeping until our own run reports `Failed` is the honest condition.
async fn sweep_until_settled(state: &AgUiState, store: &PgStore, run_id: &RunId) {
    for _ in 0..40 {
        opengrok_server::recovery::sweep_once(state)
            .await
            .expect("sweep");
        if let Ok((run, _)) = store.load_run(run_id).await
            && run.status == RunStatus::Failed
        {
            return;
        }
    }
    panic!("the sweep never settled the abandoned run");
}

async fn open_a_bubble(
    store: &PgStore,
    coworker: &CoworkerId,
    account: &AccountId,
    id: &str,
    said: &str,
) {
    store
        .append_gateway_entry(
            coworker,
            account,
            &json!({
                "kind": "send-message",
                "id": id,
                "message": { "type": "text", "content": said },
                "timestampMs": now_ms(),
                "streaming": true,
            }),
            now_ms(),
        )
        .await
        .expect("append");
}

async fn connect() -> Option<PgStore> {
    let database_url = std::env::var("OG_DATABASE_URL").ok()?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    Some(PgStore::new(pool))
}

/// The whole of the heal, in one test on purpose.
///
/// These scenarios share one global sweep, so as separate `#[tokio::test]`s they raced each other
/// — one test's sweep settled another's run before that test had finished setting itself up, and
/// the failures pointed at the code rather than at the harness. Sequential here, and each
/// assertion still names the property it is about.
#[tokio::test]
async fn a_restart_closes_the_bubbles_it_left_typing() {
    let Some(store) = connect().await else {
        eprintln!("skipping: OG_DATABASE_URL is not set");
        return;
    };
    let stamp = now_ms();
    let email = format!("abandoned-{stamp}@og.local");
    let mine = seed_account(&store, &email).await;
    let state = state(store.clone(), &email).await;

    // ---- 1. an empty bubble is closed, and says why ----
    let coworker = CoworkerId::new();
    let empty_id = format!("empty-{stamp}");
    open_a_bubble(&store, &coworker, &mine, &empty_id, "").await;
    let run_id = abandon_a_run(
        &store,
        &coworker,
        &mine,
        &format!("gateway-{}", coworker.as_str()),
    )
    .await;
    assert_eq!(
        store
            .streaming_gateway_entries(&coworker, &mine)
            .await
            .expect("read")
            .len(),
        1,
        "the placeholder must start marked streaming, or this test proves nothing"
    );

    sweep_until_settled(&state, &store, &run_id).await;

    assert!(
        store
            .streaming_gateway_entries(&coworker, &mine)
            .await
            .expect("read")
            .is_empty(),
        "a swept run must leave nothing marked streaming"
    );
    let closed = one_entry(&store, &coworker, &mine, &empty_id).await;
    assert!(
        closed.get("streaming").is_none(),
        "`streaming` is REMOVED, not set false — a healthy final frame omits it and a recovered \
         one must be indistinguishable: {closed}"
    );
    let said = closed["message"]["content"].as_str().unwrap_or_default();
    assert!(
        said.contains("did not finish") && said.contains("restart"),
        "the bubble must carry the run's own reason rather than going blank: {closed}"
    );

    // ---- 2. a partial answer survives, with the reason appended ----
    //
    // This is what makes the heal safe once answers stream: text the person has already read must
    // not be thrown away to make room for an error.
    let partial_cw = CoworkerId::new();
    let partial_id = format!("partial-{stamp}");
    open_a_bubble(
        &store,
        &partial_cw,
        &mine,
        &partial_id,
        "I had got this far",
    )
    .await;
    let partial_run = abandon_a_run(
        &store,
        &partial_cw,
        &mine,
        &format!("gateway-{}", partial_cw.as_str()),
    )
    .await;
    sweep_until_settled(&state, &store, &partial_run).await;

    let closed = one_entry(&store, &partial_cw, &mine, &partial_id).await;
    let said = closed["message"]["content"].as_str().unwrap_or_default();
    assert!(
        said.starts_with("I had got this far"),
        "the partial answer must survive: {closed}"
    );
    assert!(
        said.contains("did not finish"),
        "and the reason must follow it: {closed}"
    );
    assert!(closed.get("streaming").is_none(), "{closed}");

    // ---- 3. it is scoped to the pair, not to the coworker ----
    //
    // A shared coworker holds one transcript per person. Scoping the heal to the coworker alone
    // would have a sweep for one person's dead run blank a bubble another person's live run is
    // still writing — which is the multi-replica case, not a hypothetical.
    let shared = CoworkerId::new();
    let theirs = seed_account(&store, &format!("abandoned-theirs-{stamp}@og.local")).await;
    open_a_bubble(&store, &shared, &mine, &format!("mine-{stamp}"), "").await;
    open_a_bubble(&store, &shared, &theirs, &format!("theirs-{stamp}"), "").await;
    let shared_run = abandon_a_run(
        &store,
        &shared,
        &mine,
        &format!("gateway-{}", shared.as_str()),
    )
    .await;
    sweep_until_settled(&state, &store, &shared_run).await;

    assert!(
        store
            .streaming_gateway_entries(&shared, &mine)
            .await
            .expect("mine")
            .is_empty(),
        "the abandoned reader's bubble must be closed"
    );
    assert_eq!(
        store
            .streaming_gateway_entries(&shared, &theirs)
            .await
            .expect("theirs")
            .len(),
        1,
        "the other reader's live bubble must be untouched"
    );
}

async fn one_entry(
    store: &PgStore,
    coworker: &CoworkerId,
    account: &AccountId,
    id: &str,
) -> serde_json::Value {
    store
        .gateway_transcript(coworker, account)
        .await
        .expect("transcript")
        .into_iter()
        .find(|entry| entry["id"] == json!(id))
        .expect("the entry is still there")
}
