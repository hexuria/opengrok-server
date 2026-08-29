//! Drives `DockerComputer` against a real Docker daemon.
//!
//! Unlike the stand-in tests for box.ascii.dev, this one exercises the actual thing — because
//! unlike a hosted service, the daemon is here. It is the only place in the suite where a
//! coworker's computer is genuinely created, written to, run on, stopped and destroyed.
//!
//! SKIPS RATHER THAN FAILS WHEN DOCKER IS ABSENT. A test that fails on a machine without a daemon
//! would be deleted within a week, and then nothing would exercise this at all.

#![allow(clippy::expect_used, clippy::panic)]

use opengrok_box::{Computer, DockerComputer};

/// The image is pulled on first use; a slim base keeps that bearable.
const TEST_IMAGE: &str = "debian:stable-slim";

async fn docker_available() -> bool {
    tokio::process::Command::new("docker")
        .args(["version", "--format", "{{.Server.Version}}"])
        .output()
        .await
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Guard for every test here. Returns `None` when there is no daemon, and the caller returns.
macro_rules! computer_or_skip {
    () => {{
        if !docker_available().await {
            eprintln!("skipping: no Docker daemon");
            return;
        }
        DockerComputer::new().with_image(TEST_IMAGE)
    }};
}

/// The whole lifecycle in one test, because each step needs the box the previous one made and
/// splitting it would mean creating a container per assertion.
#[tokio::test]
async fn a_coworker_can_be_given_a_computer_and_actually_use_it() {
    let computer = computer_or_skip!();

    let box_id = computer
        .create(Some(300))
        .await
        .expect("a box should be created");
    assert!(!box_id.is_empty(), "a box id is needed to reach the box");

    // Destroy no matter how the rest goes: a leaked container outlives the test run and the person
    // who ran it will not know where it came from.
    let cleanup = computer.clone();
    let cleanup_id = box_id.clone();
    let result = run_lifecycle(&computer, &box_id).await;
    let _ = cleanup.destroy(&cleanup_id).await;
    result.expect("the lifecycle should succeed");
}

async fn run_lifecycle(computer: &DockerComputer, box_id: &str) -> Result<(), String> {
    // A command runs, and its output comes back.
    let output = computer
        .run(box_id, "echo hello from the box", 30)
        .await
        .map_err(|error| format!("run: {error}"))?;
    if output.exit_code != 0 || !output.stdout.contains("hello from the box") {
        return Err(format!("unexpected output: {output:?}"));
    }

    // A failing command is a RESULT, not an error: the command ran, and the model must see what it
    // said in order to fix it.
    let failed = computer
        .run(box_id, "exit 3", 30)
        .await
        .map_err(|error| format!("run failing: {error}"))?;
    if failed.exit_code != 3 {
        return Err(format!("expected exit 3, got {}", failed.exit_code));
    }

    // Files are written and read back, including content that would break a shell if it were
    // interpolated into one.
    let awkward = "a 'quoted' \"string\" with $VARS and `backticks`\nand a second line";
    computer
        .write_file(box_id, "/tmp/nested/dir/note.txt", awkward)
        .await
        .map_err(|error| format!("write: {error}"))?;
    let read_back = computer
        .read_file(box_id, "/tmp/nested/dir/note.txt")
        .await
        .map_err(|error| format!("read: {error}"))?;
    if read_back.trim_end() != awkward {
        return Err(format!("file came back changed: {read_back:?}"));
    }

    // A detached command starts, and polling eventually reports it finished with its output.
    let started = computer
        .start(box_id, "sleep 1; echo done working")
        .await
        .map_err(|error| format!("start: {error}"))?;
    if !started.running {
        return Err("a just-started command should be running".to_string());
    }

    let mut polled = started.clone();
    for _ in 0..30 {
        polled = computer
            .watch(box_id, &started.process_id)
            .await
            .map_err(|error| format!("watch: {error}"))?;
        if !polled.running {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    if polled.running {
        return Err("the detached command never finished".to_string());
    }
    if !polled.stdout.contains("done working") {
        return Err(format!("lost the detached output: {polled:?}"));
    }
    if polled.exit_code != Some(0) {
        return Err(format!("expected exit 0, got {:?}", polled.exit_code));
    }

    // A timeout is reported as a timeout, and never as success.
    let timed_out = computer
        .run(box_id, "sleep 30", 2)
        .await
        .map_err(|error| format!("timeout run: {error}"))?;
    if !timed_out.timed_out || timed_out.exit_code == 0 {
        return Err(format!(
            "a timeout must not look like success: {timed_out:?}"
        ));
    }

    // THE PROMISE THAT MAKES THIS A COWORKER AND NOT A SESSION: the filesystem survives the
    // machine being stopped and started again.
    computer
        .stop(box_id)
        .await
        .map_err(|error| format!("stop: {error}"))?;
    computer
        .resume(box_id)
        .await
        .map_err(|error| format!("resume: {error}"))?;
    let after_resume = computer
        .read_file(box_id, "/tmp/nested/dir/note.txt")
        .await
        .map_err(|error| format!("read after resume: {error}"))?;
    if after_resume.trim_end() != awkward {
        return Err("the work did not survive stop and resume".to_string());
    }

    // A published port resolves to a URL on this machine.
    let url = computer
        .expose_port(box_id, 3000, "the app")
        .await
        .map_err(|error| format!("expose: {error}"))?;
    if !url.starts_with("http://127.0.0.1:") {
        return Err(format!("a preview url should be local: {url}"));
    }

    Ok(())
}

/// A box that is gone must be `NoSuchBox`, not a generic refusal — a caller retries one and not
/// the other.
#[tokio::test]
async fn a_destroyed_box_reports_itself_as_missing() {
    let computer = computer_or_skip!();

    let box_id = computer
        .create(Some(60))
        .await
        .expect("a box should be created");
    computer
        .destroy(&box_id)
        .await
        .expect("the box should be destroyed");

    let error = computer
        .run(&box_id, "echo anything", 10)
        .await
        .expect_err("a destroyed box should not run commands");
    assert!(
        matches!(error, opengrok_box::BoxError::NoSuchBox),
        "expected NoSuchBox, got {error:?}"
    );
}

/// Two coworkers with dedicated computers must not see each other's files. This is the property
/// the whole identity rule protects, checked here against real isolation rather than a mock.
#[tokio::test]
async fn two_boxes_do_not_share_a_filesystem() {
    let computer = computer_or_skip!();

    let first = computer.create(Some(120)).await.expect("first box");
    let second = computer.create(Some(120)).await.expect("second box");

    let outcome = async {
        computer
            .write_file(&first, "/tmp/secret.txt", "only mine")
            .await
            .map_err(|error| format!("write: {error}"))?;
        match computer.read_file(&second, "/tmp/secret.txt").await {
            Ok(leaked) => Err(format!("the other box could read it: {leaked:?}")),
            Err(_) => Ok(()),
        }
    }
    .await;

    let _ = computer.destroy(&first).await;
    let _ = computer.destroy(&second).await;
    outcome.expect("boxes must be isolated");
}
