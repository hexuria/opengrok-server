//! A computer made of a local Docker container.
//!
//! WHY THIS EXISTS AT ALL. box.ascii.dev is hosted-only and needs an account; that is a dependency
//! on somebody else's signup before a coworker can have a computer. This one needs a Docker daemon
//! and nothing else, so the trait can be exercised on a laptop, in a test, and in CI — and a
//! coworker can be given a computer today rather than after a key arrives.
//!
//! `docs/PLAN.md` §7 decided a Docker `Computer` is *additive by construction*. This file is that
//! bet being collected: not one line of the harness, the executor or the projection changed.
//!
//! THE CONTAINER FILESYSTEM PERSISTS ACROSS STOP AND START, which is the same promise
//! box.ascii.dev makes and the reason a coworker's machine can sleep between turns. It does NOT
//! survive `destroy` — that removes the container and its writable layer, exactly as the hosted
//! one deletes a disk.
//!
//! ON SHELLING OUT TO THE CLI RATHER THAN SPEAKING THE ENGINE API. The API would avoid a process
//! per call, but it means a socket path, an API version to track and a client dependency, in
//! exchange for a saving that does not matter next to the container's own startup. `docker` on the
//! PATH is the whole configuration.

use async_trait::async_trait;
use tokio::process::Command;

use crate::{BoxError, BoxResult, CommandOutput, Computer, StartedCommand};

/// A small image with a shell and the usual utilities. Overridable, because a coworker that needs
/// a toolchain should get one rather than installing it on every turn.
pub const DEFAULT_IMAGE: &str = "debian:stable-slim";

/// Ports published when a box is created.
///
/// Docker cannot publish a port on a container that is already running, so the set has to be
/// decided up front. These are the ports a person actually serves something on; `expose_port`
/// refuses anything else with a reason rather than appearing to succeed.
pub const PUBLISHED_PORTS: &[u16] = &[3000, 5173, 8000, 8080];

#[derive(Debug, Clone)]
pub struct DockerComputer {
    pub image: String,
    /// Marks the containers we made, so `destroy` cannot remove somebody else's.
    pub label: String,
}

impl Default for DockerComputer {
    fn default() -> Self {
        Self::new()
    }
}

impl DockerComputer {
    pub fn new() -> Self {
        Self {
            image: std::env::var("OG_DOCKER_IMAGE").unwrap_or_else(|_| DEFAULT_IMAGE.to_string()),
            label: "dev.opengrok.box".to_string(),
        }
    }

    pub fn with_image(mut self, image: impl Into<String>) -> Self {
        self.image = image.into();
        self
    }

    /// Run `docker` and return its output, mapping the ways it can fail.
    async fn docker(&self, args: &[&str]) -> BoxResult<String> {
        let output = Command::new("docker")
            .args(args)
            .output()
            .await
            .map_err(|error| BoxError::Unreachable(format!("could not run docker: {error}")))?;

        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        // Docker says "No such container" for a box that is gone; a caller retries a refusal but
        // not a missing box, so the two must not be conflated.
        if stderr.contains("No such container") || stderr.contains("no such container") {
            return Err(BoxError::NoSuchBox);
        }
        Err(BoxError::Refused {
            status: output.status.code().unwrap_or(-1).unsigned_abs() as u16,
            body: stderr.chars().take(500).collect(),
        })
    }

    /// The arguments that create a box. Split out so the shape is testable without a daemon.
    pub fn create_args(&self, ttl_seconds: Option<u64>) -> Vec<String> {
        let mut args = vec![
            "run".to_string(),
            "-d".to_string(),
            "--label".to_string(),
            format!("{}=1", self.label),
        ];
        for port in PUBLISHED_PORTS {
            // Bound to loopback: a coworker's box must not be reachable from the network by
            // accident, and a person opening a preview is on this machine.
            args.push("-p".to_string());
            args.push(format!("127.0.0.1::{port}"));
        }
        args.push(self.image.clone());
        // `sleep infinity` keeps the container alive with no service in it; the TTL is enforced by
        // the shell so a forgotten box stops on its own rather than running until somebody notices.
        args.push("sh".to_string());
        args.push("-c".to_string());
        args.push(match ttl_seconds {
            Some(seconds) => format!("sleep {seconds}"),
            None => "sleep infinity".to_string(),
        });
        args
    }
}

#[async_trait]
impl Computer for DockerComputer {
    async fn create(&self, ttl_seconds: Option<u64>) -> BoxResult<String> {
        let args = self.create_args(ttl_seconds);
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        let id = self.docker(&borrowed).await?;
        // Docker prints the full 64-character id; the short form is what a person sees everywhere
        // else, and either works as a reference.
        Ok(id.chars().take(12).collect())
    }

    async fn run(
        &self,
        box_id: &str,
        command: &str,
        timeout_seconds: u32,
    ) -> BoxResult<CommandOutput> {
        let output = Command::new("docker")
            .args(["exec", box_id, "sh", "-c", command])
            .output();

        // The timeout is ours, not Docker's: `docker exec` has none, so without this a command
        // that never returns holds a run open forever.
        let output = match tokio::time::timeout(
            std::time::Duration::from_secs(timeout_seconds.clamp(1, 600).into()),
            output,
        )
        .await
        {
            Ok(result) => result
                .map_err(|error| BoxError::Unreachable(format!("could not run docker: {error}")))?,
            Err(_) => {
                return Ok(CommandOutput {
                    // Conventional for "killed by timeout", and never 0: a coworker reading 0
                    // would conclude the command succeeded.
                    exit_code: 124,
                    stdout: String::new(),
                    stderr: String::new(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                    timed_out: true,
                });
            }
        };

        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if !output.status.success() && stderr.contains("No such container") {
            return Err(BoxError::NoSuchBox);
        }

        Ok(CommandOutput {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr,
            // Docker hands back the whole output; nothing here clips it, so saying it was
            // truncated would be a lie.
            stdout_truncated: false,
            stderr_truncated: false,
            timed_out: false,
        })
    }

    async fn start(&self, box_id: &str, command: &str) -> BoxResult<StartedCommand> {
        // The process id is a name we choose, because `docker exec -d` does not report one. It
        // names the log files, which is how `watch` finds the output later.
        let process_id = format!("p{}", uuid_like());
        let script = format!(
            "mkdir -p /tmp/og && ({command}) >/tmp/og/{process_id}.out 2>/tmp/og/{process_id}.err; \
             echo $? >/tmp/og/{process_id}.code"
        );
        self.docker(&["exec", "-d", box_id, "sh", "-c", &script])
            .await?;

        Ok(StartedCommand {
            process_id,
            running: true,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
        })
    }

    async fn watch(&self, box_id: &str, process_id: &str) -> BoxResult<StartedCommand> {
        // One exec rather than three: a poll that costs three container round-trips is a poll
        // people turn off.
        //
        // `exit 0` at the end is load-bearing. While the command is still running there is no exit
        // code file yet, so that last `cat` fails and takes the script's status with it — which
        // would report a perfectly healthy in-progress poll as a refusal from the box.
        let script = format!(
            "cat /tmp/og/{process_id}.out 2>/dev/null; \
             echo '---OG-SPLIT---'; \
             cat /tmp/og/{process_id}.err 2>/dev/null; \
             echo '---OG-SPLIT---'; \
             cat /tmp/og/{process_id}.code 2>/dev/null; \
             exit 0"
        );
        let output = self.docker(&["exec", box_id, "sh", "-c", &script]).await?;
        let mut parts = output.split("---OG-SPLIT---");
        let stdout = parts.next().unwrap_or_default().trim_end().to_string();
        let stderr = parts.next().unwrap_or_default().trim().to_string();
        let exit_code = parts
            .next()
            .and_then(|code| code.trim().parse::<i32>().ok());

        Ok(StartedCommand {
            process_id: process_id.to_string(),
            // The exit code file appears only when the command finished, which is what makes this
            // an honest answer rather than a guess.
            running: exit_code.is_none(),
            stdout,
            stderr,
            exit_code,
        })
    }

    async fn read_file(&self, box_id: &str, path: &str) -> BoxResult<String> {
        self.docker(&["exec", box_id, "cat", path]).await
    }

    async fn write_file(&self, box_id: &str, path: &str, content: &str) -> BoxResult<()> {
        // Written through stdin rather than interpolated into a shell string: content with a quote
        // in it would otherwise become part of the command.
        use tokio::io::AsyncWriteExt;

        let mut child = Command::new("docker")
            .args([
                "exec",
                "-i",
                box_id,
                "sh",
                "-c",
                &format!("mkdir -p \"$(dirname '{path}')\" && cat > '{path}'"),
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|error| BoxError::Unreachable(format!("could not run docker: {error}")))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(content.as_bytes())
                .await
                .map_err(|error| BoxError::Unreachable(error.to_string()))?;
            stdin
                .shutdown()
                .await
                .map_err(|error| BoxError::Unreachable(error.to_string()))?;
        }

        let output = child
            .wait_with_output()
            .await
            .map_err(|error| BoxError::Unreachable(error.to_string()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("No such container") {
                return Err(BoxError::NoSuchBox);
            }
            return Err(BoxError::Refused {
                status: output.status.code().unwrap_or(-1).unsigned_abs() as u16,
                body: stderr.chars().take(500).collect(),
            });
        }
        Ok(())
    }

    async fn expose_port(&self, box_id: &str, port: u16, _title: &str) -> BoxResult<String> {
        // Docker cannot publish a port on a running container, so a port that was not published at
        // creation cannot be exposed now. Saying so is better than returning a URL that refuses
        // every connection.
        if !PUBLISHED_PORTS.contains(&port) {
            return Err(BoxError::Refused {
                status: 400,
                body: format!(
                    "port {port} was not published when this box was created; \
                     this computer publishes {PUBLISHED_PORTS:?}"
                ),
            });
        }
        let mapping = self.docker(&["port", box_id, &port.to_string()]).await?;
        // `docker port` answers like `127.0.0.1:49154`, one line per binding.
        let bound = mapping
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_string();
        if bound.is_empty() {
            return Err(BoxError::Refused {
                status: 404,
                body: format!("nothing is bound to port {port} on this box"),
            });
        }
        Ok(format!("http://{bound}"))
    }

    async fn stop(&self, box_id: &str) -> BoxResult<()> {
        self.docker(&["stop", box_id]).await.map(|_| ())
    }

    async fn resume(&self, box_id: &str) -> BoxResult<()> {
        self.docker(&["start", box_id]).await.map(|_| ())
    }

    async fn destroy(&self, box_id: &str) -> BoxResult<()> {
        self.docker(&["rm", "-f", box_id]).await.map(|_| ())
    }
}

/// A short unique-enough token for naming a process's log files.
///
/// Not a UUID crate: this names two files inside one container, and the id only has to be unique
/// among that container's own concurrent processes.
fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_box_is_created_on_loopback_and_labelled_as_ours() {
        let args = DockerComputer::new().create_args(Some(60));
        assert!(args.contains(&"run".to_string()));
        assert!(args.contains(&"-d".to_string()));
        // Labelled, so destroy cannot remove a container somebody else made.
        assert!(args.iter().any(|arg| arg == "dev.opengrok.box=1"));
        // Every published port bound to loopback, never 0.0.0.0.
        for port in PUBLISHED_PORTS {
            assert!(
                args.iter().any(|arg| arg == &format!("127.0.0.1::{port}")),
                "port {port} should be published on loopback"
            );
        }
        assert!(args.iter().any(|arg| arg.contains("sleep 60")));
    }

    /// A box with no TTL still has to be created, but it must not silently become a 0-second one.
    #[test]
    fn a_box_without_a_ttl_sleeps_forever_rather_than_not_at_all() {
        let args = DockerComputer::new().create_args(None);
        assert!(args.iter().any(|arg| arg == "sleep infinity"), "{args:?}");
    }

    #[test]
    fn the_image_can_be_chosen() {
        let args = DockerComputer::new()
            .with_image("rust:1-slim")
            .create_args(None);
        assert!(args.contains(&"rust:1-slim".to_string()));
    }

    /// A port that was not published cannot be exposed later, and saying so beats handing back a
    /// URL that refuses every connection.
    #[tokio::test]
    async fn an_unpublished_port_is_refused_with_the_reason() {
        let error = DockerComputer::new()
            .expose_port("nonexistent", 9999, "app")
            .await
            .expect_err("should refuse");
        match error {
            BoxError::Refused { body, .. } => {
                assert!(body.contains("not published"), "{body}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }
}
