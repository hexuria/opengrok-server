//! The per-account shared computer, end to end against real Docker + Postgres.
//!
//! 1 account = 1 computer: the first agent creates the box, a second agent REUSES it (same box id,
//! no second container), and the box is destroyed only when the account's LAST agent is deleted.
//! With no vault/org key the provider resolves to a Local VM (server Docker), which is what this
//! test exercises. Skips loudly without a Docker daemon or OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::process::Command;
use std::sync::Arc;

use opengrok_box::Computer;
use opengrok_core::coworker::{Coworker, CoworkerCommand, CoworkerView};
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::agui::provision::{ensure_computer_for, teardown_computer_for};
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
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

fn docker_available() -> bool {
    Command::new("docker")
        .args(["version", "--format", "{{.Server.Version}}"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn container_exists(id: &str) -> bool {
    Command::new("docker")
        .args(["inspect", id])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn container_running(id: &str) -> bool {
    Command::new("docker")
        .args(["inspect", "-f", "{{.State.Running}}", id])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim() == "true")
        .unwrap_or(false)
}

async fn test_state(database_url: &str) -> AgUiState {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(database_url)
        .await
        .expect("connect");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = PgStore::new(pool);
    let auth = AuthState::new(
        store,
        Arc::new(TokenMinter::new(b"box-teardown-test-secret-box-teardown")),
        "host@og.local".to_string(),
    );
    AgUiState {
        auth,
        door: Arc::new(MockDoor::echoing()),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer: None,
        // No vault ⇒ no org box key ⇒ the provider is a Local VM (server Docker).
        vault: None,
        connectors: Connectors {
            providers: Arc::new(BTreeMap::new()),
            redirect_uri: "http://127.0.0.1/callback".to_string(),
        },
        plugins: Arc::new(BTreeMap::new()),
    }
}

/// Hire an agent for `account`, ensure the account's computer, and persist it. Returns its id and
/// the assigned box id.
async fn hire_agent(state: &AgUiState, account: &AccountId, name: &str) -> (CoworkerId, String) {
    let id = CoworkerId::new();
    let mut coworker = Coworker::default();
    let mut events = Coworker::default()
        .decide(CoworkerCommand::Hire {
            name: name.to_string(),
            model: "oag/cheap".to_string(),
            at_ms: 1,
        })
        .expect("hire");
    for event in &events {
        coworker.apply(event);
    }
    let provisioned = ensure_computer_for(state, account, &id, &mut coworker, 2).await;
    assert!(
        provisioned.error.is_none(),
        "provision failed: {:?}",
        provisioned.error
    );
    events.extend(provisioned.events);
    let box_id = coworker.computer().expect("a box").as_str().to_string();
    let view = CoworkerView {
        id: id.clone(),
        name: coworker.name.clone(),
        model: coworker.model.clone(),
        box_id: coworker.computer().cloned(),
        retired: false,
        updated_at_ms: 2,
    };
    state
        .auth
        .store
        .append_coworker(&id, account, 0, &events, &view)
        .await
        .expect("append");
    (id, box_id)
}

async fn retire(state: &AgUiState, account: &AccountId, id: &CoworkerId) {
    let (loaded, seq) = state.auth.store.load_coworker(id).await.expect("load");
    let mut after = loaded;
    let events = after
        .decide(CoworkerCommand::Retire { at_ms: 9 })
        .expect("retire");
    for event in &events {
        after.apply(event);
    }
    let view = CoworkerView {
        id: id.clone(),
        name: after.name.clone(),
        model: after.model.clone(),
        box_id: after.box_id.clone(),
        retired: true,
        updated_at_ms: 9,
    };
    state
        .auth
        .store
        .append_coworker(id, account, seq, &events, &view)
        .await
        .expect("append retire");
}

#[tokio::test]
async fn one_account_one_computer_shared_and_torn_down_on_last_delete() {
    let database_url = database_or_skip!();
    if !docker_available() {
        eprintln!("skipping: no Docker daemon");
        return;
    }
    let state = test_state(&database_url).await;
    let account = AccountId::new();

    // First agent creates the account's box.
    let (agent1, box1) = hire_agent(&state, &account, "One").await;
    assert!(
        container_exists(&box1),
        "the account's box should be running"
    );
    assert_eq!(
        state
            .auth
            .store
            .scoped_computer("account", account.as_str())
            .await
            .expect("acct")
            .map(|(b, _)| b),
        Some(box1.clone())
    );

    // Second agent REUSES the same box — one account, one computer.
    let (agent2, box2) = hire_agent(&state, &account, "Two").await;
    assert_eq!(
        box2, box1,
        "the second agent must share the account's one box, not make another"
    );

    // Deleting the first agent leaves the box (the second still shares it).
    retire(&state, &account, &agent1).await;
    teardown_computer_for(&state, &account, &agent1).await;
    assert!(
        container_exists(&box1),
        "the box survives while another agent shares it"
    );
    assert!(
        state
            .auth
            .store
            .scoped_computer("account", account.as_str())
            .await
            .expect("acct")
            .is_some()
    );

    // Deleting the last agent destroys the box and clears the mapping.
    retire(&state, &account, &agent2).await;
    teardown_computer_for(&state, &account, &agent2).await;
    assert!(
        !container_exists(&box1),
        "the account's box must be destroyed on last-agent delete"
    );
    assert!(
        state
            .auth
            .store
            .scoped_computer("account", account.as_str())
            .await
            .expect("acct")
            .is_none()
    );

    if container_exists(&box1) {
        let _ = Command::new("docker").args(["rm", "-f", &box1]).output();
    }
}

#[tokio::test]
async fn per_bot_mode_gives_each_bot_its_own_box() {
    let database_url = database_or_skip!();
    if !docker_available() {
        eprintln!("skipping: no Docker daemon");
        return;
    }
    let state = test_state(&database_url).await;
    let account = AccountId::new();
    // Override this account to per-bot: each bot gets a dedicated box.
    state
        .auth
        .store
        .set_sharing_mode("account", account.as_str(), "per-bot", 1)
        .await
        .expect("set mode");

    let (agent1, box1) = hire_agent(&state, &account, "One").await;
    let (agent2, box2) = hire_agent(&state, &account, "Two").await;
    assert_ne!(
        box1, box2,
        "per-bot: each bot gets its OWN box, not a shared one"
    );
    assert!(container_exists(&box1) && container_exists(&box2));

    // Deleting one bot destroys ITS box; the other's survives.
    retire(&state, &account, &agent1).await;
    teardown_computer_for(&state, &account, &agent1).await;
    assert!(
        !container_exists(&box1),
        "per-bot: deleting a bot destroys its box"
    );
    assert!(container_exists(&box2), "the other bot's box survives");

    // Cleanup.
    retire(&state, &account, &agent2).await;
    teardown_computer_for(&state, &account, &agent2).await;
    for b in [&box1, &box2] {
        if container_exists(b) {
            let _ = Command::new("docker").args(["rm", "-f", b]).output();
        }
    }
}

/// Idle-stop: a box sitting unused past the threshold is STOPPED (its container paused, not removed)
/// and marked stopped; resuming it and marking it used brings the container back and clears the flag.
/// This is the cost lever — a stopped box keeps its disk and pauses billing.
#[tokio::test]
async fn an_idle_box_is_stopped_then_resumes_on_use() {
    let database_url = database_or_skip!();
    if !docker_available() {
        eprintln!("skipping: no Docker daemon");
        return;
    }
    let state = test_state(&database_url).await;
    let account = AccountId::new();
    let (agent, box_id) = hire_agent(&state, &account, "Idler").await;
    assert!(container_running(&box_id), "a fresh box runs");

    // Sweep with a cutoff far in the future: the box (last used at create) is idle, so it stops.
    let stopped = opengrok_server::agui::provision::idle_stop_once(
        &state,
        chrono::Utc::now().timestamp_millis() + 1,
    )
    .await;
    // At least our box (the count is global and this DB is shared with the sibling tests running
    // in parallel, so assert about OUR box, not the total).
    assert!(stopped >= 1, "the idle box should be stopped");
    assert!(
        !container_running(&box_id),
        "a stopped box's container is paused (not running)"
    );
    assert!(
        container_exists(&box_id),
        "stopped keeps the container (disk)"
    );
    let (_, _, is_stopped) = state
        .auth
        .store
        .scoped_computer_full("account", account.as_str())
        .await
        .expect("full")
        .expect("row");
    assert!(is_stopped, "the row records the box as stopped");

    // A second sweep leaves OUR box alone — an already-stopped box is excluded from the idle list.
    opengrok_server::agui::provision::idle_stop_once(
        &state,
        chrono::Utc::now().timestamp_millis() + 1,
    )
    .await;
    assert!(
        !container_running(&box_id),
        "our stopped box stays stopped across a second sweep"
    );

    // Resume on use: the run path resumes the box and marks it used ⇒ running again, flag cleared.
    let docker = opengrok_box::DockerComputer::new();
    docker.resume(&box_id).await.expect("resume");
    state
        .auth
        .store
        .mark_scoped_used(
            "account",
            account.as_str(),
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .expect("mark used");
    assert!(container_running(&box_id), "resumed box runs again");
    let (_, _, is_stopped) = state
        .auth
        .store
        .scoped_computer_full("account", account.as_str())
        .await
        .expect("full")
        .expect("row");
    assert!(!is_stopped, "marking used clears the stopped flag");

    // Cleanup.
    retire(&state, &account, &agent).await;
    teardown_computer_for(&state, &account, &agent).await;
    if container_exists(&box_id) {
        let _ = Command::new("docker").args(["rm", "-f", &box_id]).output();
    }
}

/// Eager provisioning: ensure_scope_box creates a scope's box with NO coworker, and is idempotent —
/// a second call returns the same box, not a new one. This is what warms the shared org box the
/// moment an admin selects per-org. (No org key here ⇒ Local VM, same as the other tests.)
#[tokio::test]
async fn eager_scope_box_creates_once_and_is_idempotent() {
    let database_url = database_or_skip!();
    if !docker_available() {
        eprintln!("skipping: no Docker daemon");
        return;
    }
    let state = test_state(&database_url).await;
    // A throwaway org scope id unique to this run.
    let scope_id = format!("org_{}", uuid::Uuid::now_v7().simple());

    let box_id =
        opengrok_server::agui::provision::ensure_scope_box(&state, None, "org", &scope_id, 1)
            .await
            .expect("eager provision");
    assert!(
        container_running(&box_id),
        "the eager box should be running"
    );

    // Idempotent: selecting per-org again (or the first bot arriving) reuses the SAME box.
    let again =
        opengrok_server::agui::provision::ensure_scope_box(&state, None, "org", &scope_id, 2)
            .await
            .expect("second call");
    assert_eq!(
        again, box_id,
        "eager provisioning must not make a second box"
    );

    // Cleanup.
    if let Ok(Some((b, kind))) = state.auth.store.scoped_computer("org", &scope_id).await {
        assert_eq!(kind, "local-docker");
        let _ = Command::new("docker").args(["rm", "-f", &b]).output();
        let _ = state
            .auth
            .store
            .clear_scoped_computer("org", &scope_id)
            .await;
    }
}

/// The provider's live `state` word — what getForeverBoxStatus reports so a box says whether it is
/// up, down, or gone instead of spinning "Booting up" forever. A fresh box is "running"; once
/// destroyed it is "absent".
#[tokio::test]
async fn box_state_reports_running_then_absent() {
    if !docker_available() {
        eprintln!("skipping: no Docker daemon");
        return;
    }
    let docker = opengrok_box::DockerComputer::new();
    let box_id = docker.create(Some(120)).await.expect("create");
    assert_eq!(
        docker.state(&box_id).await.expect("state"),
        "running",
        "a fresh box is running"
    );
    docker.destroy(&box_id).await.expect("destroy");
    assert_eq!(
        docker.state(&box_id).await.expect("state"),
        "absent",
        "a destroyed box is absent, not an error"
    );
}
