//! Drives `GrokBoxComputer` against a real Docker daemon *and* a grok-box guest image.
//!
//! SKIPS when Docker is absent OR the image (`OG_GROK_BOX_IMAGE`, default `grok-box:local`) is
//! not present. CI and a laptop without hexuria/box stay green; a local verify that built the
//! guest exercises create → ready → HTTP exec/files → screen_url → stop (volumes kept) →
//! resume → destroy (volumes gone).

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use opengrok_box::{Computer, GrokBoxComputer};

fn image() -> String {
    std::env::var("OG_GROK_BOX_IMAGE")
        .ok()
        .filter(|image| !image.is_empty())
        .unwrap_or_else(|| opengrok_box::grok_box::DEFAULT_IMAGE.to_string())
}

async fn docker_available() -> bool {
    tokio::process::Command::new("docker")
        .args(["version", "--format", "{{.Server.Version}}"])
        .output()
        .await
        .map(|output| output.status.success())
        .unwrap_or(false)
}

async fn image_present(name: &str) -> bool {
    tokio::process::Command::new("docker")
        .args(["image", "inspect", name])
        .output()
        .await
        .map(|output| output.status.success())
        .unwrap_or(false)
}

macro_rules! grok_box_or_skip {
    () => {{
        if !docker_available().await {
            eprintln!("skipping: no Docker daemon");
            return;
        }
        let image = image();
        if !image_present(&image).await {
            eprintln!("skipping: grok-box image {image} is not present (docker compose build in hexuria/box)");
            return;
        }
        GrokBoxComputer::new().with_image(image)
    }};
}

async fn volume_exists(name: &str) -> bool {
    tokio::process::Command::new("docker")
        .args(["volume", "inspect", name])
        .output()
        .await
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn a_grok_box_guest_is_ready_over_http_and_has_a_screen() {
    let computer = grok_box_or_skip!();
    let box_id = computer
        .create(None)
        .await
        .expect("a grok-box should be created");
    let cleanup = computer.clone();
    let cleanup_id = box_id.clone();
    let result = run_lifecycle(&computer, &box_id).await;
    let _ = cleanup.destroy(&cleanup_id).await;
    result.expect("the grok-box lifecycle should succeed");
}

async fn run_lifecycle(computer: &GrokBoxComputer, box_id: &str) -> Result<(), String> {
    let reached = computer
        .wake(box_id, std::time::Duration::from_secs(90))
        .await
        .map_err(|error| format!("wake: {error}"))?;
    if reached != "running" {
        return Err(format!("guest never became ready: {reached}"));
    }

    let output = computer
        .run(box_id, "echo hello from grok-box", 30)
        .await
        .map_err(|error| format!("run: {error}"))?;
    if output.exit_code != 0 || !output.stdout.contains("hello from grok-box") {
        return Err(format!("unexpected exec output: {output:?}"));
    }

    let awkward = "a 'quoted' \"string\" with $VARS and `backticks`\nand a second line";
    computer
        .write_file(box_id, "nested/dir/note.txt", awkward)
        .await
        .map_err(|error| format!("write: {error}"))?;
    let read_back = computer
        .read_file(box_id, "nested/dir/note.txt")
        .await
        .map_err(|error| format!("read: {error}"))?;
    if read_back.trim_end() != awkward {
        return Err(format!("file came back changed: {read_back:?}"));
    }

    let screen = computer
        .screen_url(box_id)
        .await
        .map_err(|error| format!("screen: {error}"))?
        .ok_or_else(|| "running grok-box must have a screen_url".to_string())?;
    if !screen.contains("/vnc.html") {
        return Err(format!("screen url should be noVNC: {screen}"));
    }
    if screen.to_lowercase().contains("box_token") {
        return Err(format!("BOX_TOKEN must not appear in screen_url: {screen}"));
    }

    let workspace = format!("{box_id}-workspace");
    let chrome = format!("{box_id}-chrome");
    computer
        .stop(box_id)
        .await
        .map_err(|error| format!("stop: {error}"))?;
    if !volume_exists(&workspace).await || !volume_exists(&chrome).await {
        return Err("stop must keep the volumes".to_string());
    }
    computer
        .resume(box_id)
        .await
        .map_err(|error| format!("resume: {error}"))?;
    let after = computer
        .wake(box_id, std::time::Duration::from_secs(90))
        .await
        .map_err(|error| format!("wake after resume: {error}"))?;
    if after != "running" {
        return Err(format!("resumed guest never became ready: {after}"));
    }
    let survived = computer
        .read_file(box_id, "nested/dir/note.txt")
        .await
        .map_err(|error| format!("read after resume: {error}"))?;
    if survived.trim_end() != awkward {
        return Err("the workspace did not survive stop and resume".to_string());
    }

    computer
        .destroy(box_id)
        .await
        .map_err(|error| format!("destroy: {error}"))?;
    if volume_exists(&workspace).await || volume_exists(&chrome).await {
        return Err("destroy must remove the volumes".to_string());
    }
    Ok(())
}

#[tokio::test]
async fn two_grok_boxes_do_not_share_a_workspace() {
    let computer = grok_box_or_skip!();
    let first = match computer.create(None).await {
        Ok(id) => id,
        Err(error) => {
            eprintln!("skipping: could not create first grok-box: {error}");
            return;
        }
    };
    let second = match computer.create(None).await {
        Ok(id) => id,
        Err(error) => {
            let _ = computer.destroy(&first).await;
            eprintln!("skipping: could not create second grok-box: {error}");
            return;
        }
    };
    let outcome = async {
        computer
            .wake(&first, std::time::Duration::from_secs(90))
            .await
            .map_err(|error| format!("wake first: {error}"))?;
        computer
            .wake(&second, std::time::Duration::from_secs(90))
            .await
            .map_err(|error| format!("wake second: {error}"))?;
        computer
            .write_file(&first, "secret.txt", "only mine")
            .await
            .map_err(|error| format!("write: {error}"))?;
        match computer.read_file(&second, "secret.txt").await {
            Ok(leaked) => Err(format!("the other box could read it: {leaked:?}")),
            Err(_) => Ok(()),
        }
    }
    .await;
    let _ = computer.destroy(&first).await;
    let _ = computer.destroy(&second).await;
    outcome.expect("per-bot grok-boxes must be isolated");
}
