//! Proves a retired bot's dedicated box is actually destroyed — not merely marked released.
//!
//! The create paths provision a box; this is the other half of the invariant "no create without
//! teardown". `release_computer` must call the provider's `destroy` for a DEDICATED box, so a
//! deleted bot leaves no running container (docker) or paid box (ascii). Uses a real Docker box, so
//! it SKIPS — loudly — when no daemon is present, the same bargain slice6 makes.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::process::Command;
use std::sync::Arc;

use opengrok_box::{Computer, DockerComputer};
use opengrok_core::coworker::{BoxMode, Coworker, CoworkerCommand, CoworkerEvent};
use opengrok_core::id::BoxId;
use opengrok_server::agui::provision::release_computer;

fn docker_available() -> bool {
    Command::new("docker")
        .args(["version", "--format", "{{.Server.Version}}"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// A container is present iff `docker inspect` succeeds.
fn container_exists(id: &str) -> bool {
    Command::new("docker")
        .args(["inspect", id])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn dedicated_coworker(box_id: &str) -> Coworker {
    let mut coworker = Coworker::default();
    for event in &coworker
        .clone()
        .decide(CoworkerCommand::Hire {
            name: "Bot".to_string(),
            model: "oag/cheap".to_string(),
            at_ms: 1,
        })
        .expect("hire")
    {
        coworker.apply(event);
    }
    for event in &coworker
        .decide(CoworkerCommand::AssignComputer {
            box_id: BoxId::from_stored(box_id.to_string()),
            mode: BoxMode::Dedicated,
            at_ms: 2,
        })
        .expect("assign")
    {
        coworker.apply(event);
    }
    coworker
}

#[tokio::test]
async fn retiring_a_bot_destroys_its_dedicated_box() {
    if !docker_available() {
        eprintln!("skipping: no Docker daemon");
        return;
    }

    let docker: Arc<dyn Computer> = Arc::new(DockerComputer::new());
    let box_id = docker.create(Some(120)).await.expect("create a box");
    assert!(
        container_exists(&box_id),
        "the box should be running after create"
    );

    let mut coworker = dedicated_coworker(&box_id);
    let events = release_computer(Some(&docker), &mut coworker, 3).await;

    // The release is recorded on the aggregate...
    assert!(
        events
            .iter()
            .any(|event| matches!(event, CoworkerEvent::ComputerReleased { .. })),
        "release_computer must emit ComputerReleased"
    );
    assert!(coworker.computer().is_none(), "the box is off the coworker");

    // ...and the real container is gone.
    assert!(
        !container_exists(&box_id),
        "the dedicated box must be destroyed on release, not left running"
    );

    // Belt and braces: if the assert above ever regresses, don't leak the container.
    if container_exists(&box_id) {
        let _ = Command::new("docker").args(["rm", "-f", &box_id]).output();
    }
}
