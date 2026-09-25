//! `bundle::place` on a stand-in computer: where the files land, what is refused before any shell
//! sees it, and that the same set is written once.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use opengrok_box::bundle::{place, plain_path};
use opengrok_box::{BoxError, BoxResult, CommandOutput, Computer, StartedCommand};

#[derive(Default)]
struct Disk {
    files: Mutex<BTreeMap<String, String>>,
    commands: Mutex<Vec<String>>,
}

fn out(stdout: &str) -> CommandOutput {
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
impl Computer for Disk {
    async fn create(&self, _ttl: Option<u64>) -> BoxResult<String> {
        Ok("bx".into())
    }
    async fn run(&self, _b: &str, command: &str, _t: u32) -> BoxResult<CommandOutput> {
        self.commands.lock().unwrap().push(command.to_string());
        if command.contains("$HOME") {
            return Ok(out("/root/\n"));
        }
        if let Some((path, _)) = command
            .strip_prefix("cat '")
            .and_then(|rest| rest.split_once('\''))
        {
            let files = self.files.lock().unwrap();
            return Ok(out(files.get(path).map(String::as_str).unwrap_or("")));
        }
        if let Some((dir, _)) = command
            .strip_prefix("rm -rf '")
            .and_then(|rest| rest.split_once('\''))
        {
            let dir = format!("{dir}/");
            self.files
                .lock()
                .unwrap()
                .retain(|path, _| !path.starts_with(&dir));
        }
        Ok(out(""))
    }
    async fn start(&self, _b: &str, _c: &str) -> BoxResult<StartedCommand> {
        Err(BoxError::NoSuchBox)
    }
    async fn watch(&self, _b: &str, _p: &str) -> BoxResult<StartedCommand> {
        Err(BoxError::NoSuchBox)
    }
    async fn read_file(&self, _b: &str, path: &str) -> BoxResult<String> {
        self.files
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .ok_or(BoxError::NoSuchBox)
    }
    async fn write_file(&self, _b: &str, path: &str, content: &str) -> BoxResult<()> {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_string(), content.to_string());
        Ok(())
    }
    async fn expose_port(&self, _b: &str, _p: u16, _t: &str) -> BoxResult<String> {
        Err(BoxError::NoSuchBox)
    }
    async fn stop(&self, _b: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn resume(&self, _b: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn destroy(&self, _b: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn state(&self, _b: &str) -> BoxResult<String> {
        Ok("running".into())
    }
}

fn file(path: &str, bytes: &[u8]) -> (String, Vec<u8>) {
    (path.to_string(), bytes.to_vec())
}

#[test]
fn only_plain_paths_reach_a_shell() {
    for good in [
        "check.sh",
        "scripts/check.sh",
        ".skills/review/v3",
        "a_b-c.d/e",
    ] {
        assert!(plain_path(good), "{good}");
    }
    for bad in [
        "", "/abs", "a//b", "../x", "a/./b", "-rf", "a b", "a'b", "$(id)", "a`b`", "a;b", "é",
    ] {
        assert!(!plain_path(bad), "{bad:?}");
    }
}

#[tokio::test]
async fn files_land_under_home_and_the_same_set_is_written_once() {
    let disk = Disk::default();
    let files = vec![
        file("scripts/check.sh", b"#!/bin/sh\necho ok\n"),
        file("notes.md", b"read me"),
        file("sheet.bin", &[0xC3, 0x28]),
        file("a';id;'", b"never"),
    ];

    let placed = place(&disk, "bx", ".skills/review/v2", &files)
        .await
        .expect("placed");

    assert_eq!(placed.dir, "/root/.skills/review/v2");
    assert_eq!(placed.written, vec!["scripts/check.sh", "notes.md"]);
    let skipped: Vec<&str> = placed.skipped.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(skipped, vec!["sheet.bin", "a';id;'"]);
    assert_eq!(
        disk.read_file("bx", "/root/.skills/review/v2/scripts/check.sh")
            .await
            .expect("written"),
        "#!/bin/sh\necho ok\n"
    );
    let commands = disk.commands.lock().unwrap().clone();
    assert!(
        commands.iter().all(|command| !command.contains("id;")),
        "a refused path never reaches a shell: {commands:?}"
    );
    assert!(
        commands
            .iter()
            .any(|c| c == "chmod +x '/root/.skills/review/v2/scripts/check.sh'"),
        "{commands:?}"
    );

    // Again: the digest matches, nothing is cleared or rewritten.
    let before = disk.commands.lock().unwrap().len();
    let again = place(&disk, "bx", ".skills/review/v2", &files)
        .await
        .expect("placed");
    assert_eq!(again.written, placed.written);
    let after: Vec<String> = disk.commands.lock().unwrap()[before..].to_vec();
    assert!(
        after.iter().all(|c| !c.starts_with("rm -rf")),
        "an unchanged set is not rewritten: {after:?}"
    );

    // A different set in the same directory clears the old one first.
    let other = vec![file("other.md", b"new")];
    place(&disk, "bx", ".skills/review/v2", &other)
        .await
        .expect("placed");
    assert!(
        disk.read_file("bx", "/root/.skills/review/v2/notes.md")
            .await
            .is_err(),
        "no file of the previous set is left beside the new one"
    );
}

#[tokio::test]
async fn a_directory_that_is_not_plain_is_refused_before_anything_runs() {
    let disk = Disk::default();
    let refused = place(&disk, "bx", ".skills/x';id;'/v1", &[file("a.md", b"x")]).await;
    assert!(refused.is_err());
    assert!(disk.commands.lock().unwrap().is_empty());
}
