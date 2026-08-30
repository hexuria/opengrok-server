//! The per-account shared computer, end to end against real Docker + Postgres.
//!
//! 1 account = 1 computer: the first agent creates the box, a second agent REUSES it (same box id,
//! no second container), and the box is destroyed only when the account's LAST agent is deleted.
//! Skips loudly without a Docker daemon or OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::process::Command;
use std::sync::Arc;

use opengrok_box::{Computer, DockerComputer};
use opengrok_core::coworker::{Coworker, CoworkerCommand, CoworkerView};
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_server::agui::provision::{
    ensure_account_computer, teardown_account_computer_if_last,
};
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

async fn store(url: &str) -> PgStore {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(url)
        .await
        .expect("connect");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    PgStore::new(pool)
}

/// Hire an agent for `account`, ensure the account's computer, and persist it. Returns its id and
/// the assigned box id.
async fn hire_agent(
    store: &PgStore,
    computer: &Arc<dyn Computer>,
    account: &AccountId,
    name: &str,
) -> (CoworkerId, String) {
    let id = CoworkerId::new();
    let mut coworker = Coworker::default();
    let mut events = coworker
        .clone()
        .decide(CoworkerCommand::Hire {
            name: name.to_string(),
            model: "oag/cheap".to_string(),
            at_ms: 1,
        })
        .expect("hire");
    for event in &events {
        coworker.apply(event);
    }
    let provisioned =
        ensure_account_computer(Some(computer), store, account, &mut coworker, 2).await;
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
    store
        .append_coworker(&id, account, 0, &events, &view)
        .await
        .expect("append");
    (id, box_id)
}

async fn retire(store: &PgStore, account: &AccountId, id: &CoworkerId) {
    let (loaded, seq) = store.load_coworker(id).await.expect("load");
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
    store
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
    let store = store(&database_url).await;
    let docker: Arc<dyn Computer> = Arc::new(DockerComputer::new());
    let account = AccountId::new();

    // First agent creates the account's box.
    let (agent1, box1) = hire_agent(&store, &docker, &account, "One").await;
    assert!(
        container_exists(&box1),
        "the account's box should be running"
    );
    assert_eq!(
        store
            .account_computer(account.as_str())
            .await
            .expect("acct")
            .map(|(b, _)| b),
        Some(box1.clone())
    );

    // Second agent REUSES the same box — one account, one computer.
    let (agent2, box2) = hire_agent(&store, &docker, &account, "Two").await;
    assert_eq!(
        box2, box1,
        "the second agent must share the account's one box, not make another"
    );

    // Deleting the first agent leaves the box (the second still shares it).
    retire(&store, &account, &agent1).await;
    teardown_account_computer_if_last(Some(&docker), &store, &account).await;
    assert!(
        container_exists(&box1),
        "the box survives while another agent shares it"
    );
    assert!(
        store
            .account_computer(account.as_str())
            .await
            .expect("acct")
            .is_some()
    );

    // Deleting the last agent destroys the box and clears the mapping.
    retire(&store, &account, &agent2).await;
    teardown_account_computer_if_last(Some(&docker), &store, &account).await;
    assert!(
        !container_exists(&box1),
        "the account's box must be destroyed on last-agent delete"
    );
    assert!(
        store
            .account_computer(account.as_str())
            .await
            .expect("acct")
            .is_none()
    );

    // Safety net: never leak the container even if an assert regresses.
    if container_exists(&box1) {
        let _ = Command::new("docker").args(["rm", "-f", &box1]).output();
    }
}
