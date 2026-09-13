//! A computer made of a self-hosted grok-box guest.
//!
//! WHY THIS EXISTS. `DockerComputer` is a headless debian container driven with `docker exec`.
//! grok-box (`hexuria/box`) is a different guest: two HTTP daemons (exec :1337, host :1340), a
//! noVNC desktop on :6080, and a per-box `BOX_TOKEN`. Speaking that wire through `docker exec`
//! would be the wrong transport, and putting guest HTTP in the harness would scatter the seam.
//! This module is the third `Computer` impl: docker for lifecycle, HTTP for the work.
//!
//! EnsureBox in hexuria/box is a demo of the same pattern (create / ready / stop / destroy). It
//! is not a control plane we adopt — OpenGrok already owns sharing, idle-stop, and the admin
//! surface. We port the docker-run + poll-ready + guest-HTTP shape, not the Node app.
//!
//! BOX_TOKEN is minted at create, injected as container env, and recovered later via
//! `docker inspect`. It never enters Postgres, a client payload, or a log. The vncUrl the
//! desktop draws is the published noVNC URL plus the independent `BOX_VNC_PASSWORD` (x11vnc
//! uses the first 8 characters); that password is for the human's screen, not the guest API.
//!
//! STOP KEEPS THE CONTAINER AND THE VOLUMES (disk survives; billing/CPU pause). DESTROY removes
//! both. Named volumes `{id}-workspace` and `{id}-chrome` are the grok-box mounts that persist
//! across hibernate.
//!
//! CUA (`POST /v1/cua/*`) is intentionally not on `Computer` yet. Shell, files, and `screen_url`
//! are enough for a first local verify; adding screenshot/click/type as default-unsupported
//! trait methods is a follow-up so AsciiBoxes/DockerComputer do not have to grow stubs first.

mod guest;

use async_trait::async_trait;
use tokio::process::Command;

use crate::{BoxError, BoxResult, CommandOutput, Computer, StartedCommand};
use guest::Guest;

/// The image hexuria/box's compose file tags locally. Overridable via `OG_GROK_BOX_IMAGE`.
pub const DEFAULT_IMAGE: &str = "grok-box:local";

/// Kind string stored on `scoped_computer` and advertised to the client.
pub const KIND: &str = "grok-box";

/// Ports published when a box is created. Docker cannot add a publish after start, so the
/// guest wire (exec / host / noVNC) has to be decided up front. 5900 and 9222 stay inside the
/// container — grok-box binds those to loopback and they are not Bearer-authenticated.
pub const PUBLISHED_PORTS: &[u16] = &[1337, 1340, 6080];

const EXEC_PORT: u16 = 1337;
const HOST_PORT: u16 = 1340;
const NOVNC_PORT: u16 = 6080;

#[derive(Clone)]
pub struct GrokBoxComputer {
    pub image: String,
    /// Marks the containers we made, so `destroy` cannot remove somebody else's.
    pub label: String,
    /// A second label naming the RUN that made the box (`dev.opengrok.run=<tag>`), from
    /// `OG_BOX_RUN_TAG`. The gate sets one per invocation and removes everything carrying it on
    /// exit. `None` outside such runs.
    pub run_tag: Option<String>,
    /// Host name written into `screen_url`. Ports themselves are always published on 127.0.0.1;
    /// this is only the URL the desktop opens, so a tunnelled or LAN-rewritten viewer can be
    /// named without binding the guest on a public NIC.
    pub screen_host: String,
    http: reqwest::Client,
}

impl std::fmt::Debug for GrokBoxComputer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrokBoxComputer")
            .field("image", &self.image)
            .field("label", &self.label)
            .field("run_tag", &self.run_tag)
            .field("screen_host", &self.screen_host)
            .finish()
    }
}

impl Default for GrokBoxComputer {
    fn default() -> Self {
        Self::new()
    }
}

impl GrokBoxComputer {
    pub fn new() -> Self {
        Self {
            image: std::env::var("OG_GROK_BOX_IMAGE")
                .ok()
                .filter(|image| !image.is_empty())
                .unwrap_or_else(|| DEFAULT_IMAGE.to_string()),
            label: "dev.opengrok.box".to_string(),
            run_tag: std::env::var("OG_BOX_RUN_TAG")
                .ok()
                .filter(|tag| !tag.is_empty()),
            screen_host: std::env::var("OG_GROK_BOX_SCREEN_HOST")
                .ok()
                .filter(|host| !host.is_empty())
                .unwrap_or_else(|| "127.0.0.1".to_string()),
            http: reqwest::Client::new(),
        }
    }

    pub fn with_image(mut self, image: impl Into<String>) -> Self {
        self.image = image.into();
        self
    }

    pub fn with_screen_host(mut self, host: impl Into<String>) -> Self {
        self.screen_host = host.into();
        self
    }

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
        let lowered = stderr.to_lowercase();
        if lowered.contains("no such container") || lowered.contains("no such object") {
            return Err(BoxError::NoSuchBox);
        }
        Err(BoxError::Refused {
            status: output.status.code().unwrap_or(-1).unsigned_abs() as u16,
            body: stderr.chars().take(500).collect(),
        })
    }

    /// The arguments that create a box. Split out so the shape is testable without a daemon.
    /// `token` and `vnc_password` are caller-minted so a test can assert they appear as env
    /// without reading them back from a live container.
    pub fn create_args(&self, box_id: &str, token: &str, vnc_password: &str) -> Vec<String> {
        let mut args = vec![
            "run".to_string(),
            "-d".to_string(),
            "--name".to_string(),
            box_id.to_string(),
            "--shm-size".to_string(),
            "256m".to_string(),
            "--label".to_string(),
            format!("{}=1", self.label),
            "--label".to_string(),
            format!("dev.opengrok.kind={KIND}"),
            "--label".to_string(),
            format!("dev.opengrok.box.id={box_id}"),
        ];
        if let Some(tag) = &self.run_tag {
            args.push("--label".to_string());
            args.push(format!("dev.opengrok.run={tag}"));
        }
        for port in PUBLISHED_PORTS {
            // Bound to loopback: a coworker's box must not be reachable from the network by
            // accident, and 6080 is not Bearer-authenticated.
            args.push("-p".to_string());
            args.push(format!("127.0.0.1::{port}"));
        }
        args.push("-e".to_string());
        args.push(format!("BOX_TOKEN={token}"));
        args.push("-e".to_string());
        args.push(format!("BOX_VNC_PASSWORD={vnc_password}"));
        args.push("-e".to_string());
        args.push(format!("BOX_ID={box_id}"));
        args.push("-e".to_string());
        args.push("BOX_DESKTOP=1".to_string());
        args.push("-e".to_string());
        args.push("BOX_DESKTOP_REQUIRED=1".to_string());
        args.push("-e".to_string());
        args.push("BOX_CHROME=1".to_string());
        args.push("-e".to_string());
        args.push("BOX_CUA=1".to_string());
        args.push("-e".to_string());
        args.push("WORKSPACE_ROOT=/workspace".to_string());
        args.push("-e".to_string());
        args.push("BOX_CHROME_PROFILE=/home/box/chrome-profile".to_string());
        args.push("-e".to_string());
        args.push("BOX_EXEC_BIND=0.0.0.0:1337".to_string());
        args.push("-e".to_string());
        args.push("BOX_HOST_BIND=0.0.0.0:1340".to_string());
        args.push("-v".to_string());
        args.push(format!("{}:/workspace", workspace_volume(box_id)));
        args.push("-v".to_string());
        args.push(format!(
            "{}:/home/box/chrome-profile",
            chrome_volume(box_id)
        ));
        args.push(self.image.clone());
        args
    }

    async fn ensure_volumes(&self, box_id: &str) -> BoxResult<()> {
        for name in [workspace_volume(box_id), chrome_volume(box_id)] {
            let mut args = vec![
                "volume".to_string(),
                "create".to_string(),
                "--label".to_string(),
                format!("{}=1", self.label),
                "--label".to_string(),
                format!("dev.opengrok.kind={KIND}"),
                "--label".to_string(),
                format!("dev.opengrok.box.id={box_id}"),
            ];
            if let Some(tag) = &self.run_tag {
                args.push("--label".to_string());
                args.push(format!("dev.opengrok.run={tag}"));
            }
            args.push(name);
            let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
            self.docker(&borrowed).await?;
        }
        Ok(())
    }

    async fn env_value(&self, box_id: &str, key: &str) -> BoxResult<String> {
        let blob = self
            .docker(&[
                "inspect",
                "-f",
                "{{range .Config.Env}}{{println .}}{{end}}",
                box_id,
            ])
            .await?;
        let prefix = format!("{key}=");
        blob.lines()
            .find_map(|line| line.strip_prefix(&prefix).map(str::to_string))
            .ok_or_else(|| BoxError::Refused {
                status: 500,
                body: format!("the box is missing {key}"),
            })
    }

    async fn published_port(&self, box_id: &str, container_port: u16) -> BoxResult<u16> {
        let mapping = self
            .docker(&["port", box_id, &container_port.to_string()])
            .await?;
        parse_docker_port(&mapping).ok_or_else(|| BoxError::Refused {
            status: 404,
            body: format!("nothing is bound to port {container_port} on this box"),
        })
    }

    async fn guest_for(&self, box_id: &str) -> BoxResult<Guest> {
        let token = self.env_value(box_id, "BOX_TOKEN").await?;
        let exec = self.published_port(box_id, EXEC_PORT).await?;
        let host = self.published_port(box_id, HOST_PORT).await?;
        Ok(Guest::connect(
            format!("http://127.0.0.1:{exec}"),
            format!("http://127.0.0.1:{host}"),
            token,
            self.http.clone(),
        ))
    }

    async fn docker_state(&self, box_id: &str) -> BoxResult<String> {
        match self
            .docker(&["inspect", "-f", "{{.State.Status}}", box_id])
            .await
        {
            Ok(status) if !status.is_empty() => Ok(status),
            Ok(_) => Ok("absent".to_string()),
            Err(BoxError::NoSuchBox) => Ok("absent".to_string()),
            Err(other) => Err(other),
        }
    }

    async fn remove_volumes(&self, box_id: &str) {
        for name in [workspace_volume(box_id), chrome_volume(box_id)] {
            let _ = self.docker(&["volume", "rm", &name]).await;
        }
    }
}

/// A URL a person can open to see this box's screen. The VNC password is independent of
/// `BOX_TOKEN` and is the one thing the browser is allowed to hold — noVNC is not Bearer-authed.
pub fn viewer_url(host: &str, port: u16, vnc_password: &str) -> String {
    format!("http://{host}:{port}/vnc.html?autoconnect=1&resize=scale&password={vnc_password}")
}

/// Host port from `docker port` output (`127.0.0.1:49154`). Prefers an IPv4 loopback line when
/// Docker also prints IPv6.
pub fn parse_docker_port(mapping: &str) -> Option<u16> {
    let preferred = mapping
        .lines()
        .find(|line| line.contains("127.0.0.1:"))
        .or_else(|| mapping.lines().next())
        .unwrap_or("")
        .trim();
    preferred
        .rsplit_once(':')
        .and_then(|(_, port)| port.trim().parse().ok())
}

fn workspace_volume(box_id: &str) -> String {
    format!("{box_id}-workspace")
}

fn chrome_volume(box_id: &str) -> String {
    format!("{box_id}-chrome")
}

/// grok-box rejects short / well-known tokens unless `BOX_ALLOW_INSECURE_DEV=1`, which we never
/// set. 24 random bytes as hex is well above that floor.
pub fn mint_box_token() -> String {
    use rand::RngExt;
    let bytes: [u8; 24] = rand::rng().random();
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// x11vnc uses the first 8 characters. Alphanumeric so the value is safe in a query string
/// without encoding, and so a log of `screen_url` cannot be mistaken for a BOX_TOKEN (hex, longer).
pub fn mint_vnc_password() -> String {
    use rand::RngExt;
    const ALPH: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let bytes: [u8; 8] = rand::rng().random();
    bytes
        .iter()
        .map(|byte| ALPH[*byte as usize % ALPH.len()] as char)
        .collect()
}

fn mint_box_id() -> String {
    use rand::RngExt;
    let bytes: [u8; 5] = rand::rng().random();
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("og-gb-{hex}")
}

/// Paths the guest files API will accept: anything under `/workspace`, or a relative path
/// (jailed to the workspace root). Absolute paths elsewhere go through `POST /v1/exec`.
fn files_api_path(path: &str) -> Option<&str> {
    let path = path.trim();
    if path.is_empty() {
        return Some("");
    }
    if path == "/workspace" {
        return Some("");
    }
    if let Some(rest) = path.strip_prefix("/workspace/") {
        return Some(rest);
    }
    if !path.starts_with('/') {
        return Some(path);
    }
    None
}

#[async_trait]
impl Computer for GrokBoxComputer {
    fn kind(&self) -> &'static str {
        KIND
    }

    async fn create(&self, _ttl_seconds: Option<u64>) -> BoxResult<String> {
        // TTL is a box.ascii.dev / DockerComputer (sleep) concern. grok-box's entrypoint is the
        // guest itself; idle-stop on the OpenGrok side is what parks a forgotten box.
        let box_id = mint_box_id();
        let token = mint_box_token();
        let vnc_password = mint_vnc_password();
        self.ensure_volumes(&box_id).await?;
        let args = self.create_args(&box_id, &token, &vnc_password);
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        match self.docker(&borrowed).await {
            Ok(_) => Ok(box_id),
            Err(error) => {
                let _ = self.docker(&["rm", "-f", &box_id]).await;
                self.remove_volumes(&box_id).await;
                Err(error)
            }
        }
    }

    async fn run(
        &self,
        box_id: &str,
        command: &str,
        timeout_seconds: u32,
    ) -> BoxResult<CommandOutput> {
        self.guest_for(box_id)
            .await?
            .exec(command, timeout_seconds, None)
            .await
    }

    async fn start(&self, box_id: &str, command: &str) -> BoxResult<StartedCommand> {
        let process_id = format!("p{}", mint_box_id().trim_start_matches("og-gb-"));
        // grok-box exec is synchronous. Background the work inside the guest and return; `watch`
        // reads the files the wrapper writes under /workspace/.og.
        let script = format!(
            "mkdir -p /workspace/.og && nohup sh -c '\
             ({command}) >/workspace/.og/{process_id}.out 2>/workspace/.og/{process_id}.err; \
             echo $? >/workspace/.og/{process_id}.code' >/dev/null 2>&1 & echo ok"
        );
        self.guest_for(box_id)
            .await?
            .exec(&script, 15, None)
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
        let script = format!(
            "cat /workspace/.og/{process_id}.out 2>/dev/null; \
             echo '---OG-SPLIT---'; \
             cat /workspace/.og/{process_id}.err 2>/dev/null; \
             echo '---OG-SPLIT---'; \
             cat /workspace/.og/{process_id}.code 2>/dev/null; \
             exit 0"
        );
        let output = self
            .guest_for(box_id)
            .await?
            .exec(&script, 15, None)
            .await?;
        let mut parts = output.stdout.split("---OG-SPLIT---");
        let stdout = parts.next().unwrap_or_default().trim_end().to_string();
        let stderr = parts.next().unwrap_or_default().trim().to_string();
        let exit_code = parts
            .next()
            .and_then(|code| code.trim().parse::<i32>().ok());
        Ok(StartedCommand {
            process_id: process_id.to_string(),
            running: exit_code.is_none(),
            stdout,
            stderr,
            exit_code,
        })
    }

    async fn read_file(&self, box_id: &str, path: &str) -> BoxResult<String> {
        let guest = self.guest_for(box_id).await?;
        if let Some(relative) = files_api_path(path) {
            return guest.read_file(relative).await;
        }
        let quoted = sh_single_quote(path);
        let output = guest.exec(&format!("cat -- {quoted}"), 30, None).await?;
        if output.exit_code != 0 {
            return Err(BoxError::Refused {
                status: 404,
                body: output.stderr,
            });
        }
        Ok(output.stdout)
    }

    async fn write_file(&self, box_id: &str, path: &str, content: &str) -> BoxResult<()> {
        let guest = self.guest_for(box_id).await?;
        if let Some(relative) = files_api_path(path) {
            return guest.write_file(relative, content).await;
        }
        let quoted = sh_single_quote(path);
        // Content rides `stdin` so a quote in the file cannot become a command. The path is
        // single-quoted so it cannot break out of the shell snippet either.
        let script = format!("mkdir -p -- \"$(dirname -- {quoted})\" && cat > {quoted}");
        let output = guest.exec(&script, 30, Some(content)).await?;
        if output.exit_code != 0 {
            return Err(BoxError::Refused {
                status: 500,
                body: output.stderr,
            });
        }
        Ok(())
    }

    async fn expose_port(&self, box_id: &str, port: u16, _title: &str) -> BoxResult<String> {
        if !PUBLISHED_PORTS.contains(&port) {
            return Err(BoxError::Refused {
                status: 400,
                body: format!(
                    "port {port} was not published when this box was created; \
                     this computer publishes {PUBLISHED_PORTS:?}"
                ),
            });
        }
        let host_port = self.published_port(box_id, port).await?;
        Ok(format!("http://127.0.0.1:{host_port}"))
    }

    async fn stop(&self, box_id: &str) -> BoxResult<()> {
        self.docker(&["stop", box_id]).await.map(|_| ())
    }

    async fn resume(&self, box_id: &str) -> BoxResult<()> {
        self.docker(&["start", box_id]).await.map(|_| ())
    }

    async fn destroy(&self, box_id: &str) -> BoxResult<()> {
        match self.docker(&["rm", "-f", box_id]).await {
            Ok(_) | Err(BoxError::NoSuchBox) => {}
            Err(other) => return Err(other),
        }
        self.remove_volumes(box_id).await;
        Ok(())
    }

    async fn state(&self, box_id: &str) -> BoxResult<String> {
        let docker_state = self.docker_state(box_id).await?;
        if docker_state != "running" {
            return Ok(docker_state);
        }
        // Container up is not guest-ready: box-host / Xvfb / noVNC come up after `docker start`.
        // `Computer::wake` already waits on `provisioning` (see `is_starting`), so that is the
        // word we use until GET /v1/ready returns 200 with the minted bearer.
        match self.guest_for(box_id).await {
            Err(BoxError::NoSuchBox) => Ok("absent".to_string()),
            Err(_) => Ok("provisioning".to_string()),
            Ok(guest) => match guest.ready().await {
                Ok(()) => Ok("running".to_string()),
                Err(BoxError::Refused {
                    status: 401 | 403, ..
                }) => Ok("error".to_string()),
                Err(_) => Ok("provisioning".to_string()),
            },
        }
    }

    async fn screen_url(&self, box_id: &str) -> BoxResult<Option<String>> {
        if self.docker_state(box_id).await? != "running" {
            return Ok(None);
        }
        let guest = match self.guest_for(box_id).await {
            Ok(guest) => guest,
            Err(_) => return Ok(None),
        };
        if guest.ready().await.is_err() {
            return Ok(None);
        }
        let port = match self.published_port(box_id, NOVNC_PORT).await {
            Ok(port) => port,
            Err(_) => return Ok(None),
        };
        let password = match self.env_value(box_id, "BOX_VNC_PASSWORD").await {
            Ok(password) => password,
            Err(_) => return Ok(None),
        };
        Ok(Some(viewer_url(&self.screen_host, port, &password)))
    }
}

/// Quote a path for a `sh -c` snippet so a quote in the path cannot break out of the command.
fn sh_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_box_is_created_on_loopback_with_the_guest_ports_and_volumes() {
        let computer = GrokBoxComputer::new();
        let args = computer.create_args("og-gb-test1", "tokentokentokentoken12", "vncPass1");
        assert!(args.contains(&"run".to_string()));
        assert!(args.contains(&"-d".to_string()));
        assert!(args.iter().any(|arg| arg == "dev.opengrok.box=1"));
        assert!(args.iter().any(|arg| arg == "dev.opengrok.kind=grok-box"));
        assert!(args.contains(&"--name".to_string()));
        assert!(args.contains(&"og-gb-test1".to_string()));
        assert!(args.contains(&"--shm-size".to_string()));
        for port in PUBLISHED_PORTS {
            assert!(
                args.iter().any(|arg| arg == &format!("127.0.0.1::{port}")),
                "port {port} should be published on loopback: {args:?}"
            );
        }
        assert!(
            !args.iter().any(|arg| arg.contains("127.0.0.1::3000")),
            "headless DockerComputer ports must not leak into grok-box: {args:?}"
        );
        assert!(
            args.iter()
                .any(|arg| arg == "BOX_TOKEN=tokentokentokentoken12")
        );
        assert!(args.iter().any(|arg| arg == "BOX_VNC_PASSWORD=vncPass1"));
        assert!(
            args.iter()
                .any(|arg| arg == "og-gb-test1-workspace:/workspace")
        );
        assert!(
            args.iter()
                .any(|arg| arg == "og-gb-test1-chrome:/home/box/chrome-profile")
        );
        assert!(
            !args.iter().any(|arg| arg.contains("sleep")),
            "grok-box has its own entrypoint; do not override it with sleep: {args:?}"
        );
        assert!(
            !args
                .iter()
                .any(|arg| arg.contains("BOX_ALLOW_INSECURE_DEV")),
            "never relax guest token checks: {args:?}"
        );
        assert_eq!(computer.kind(), "grok-box");
    }

    #[test]
    fn a_run_tag_is_labelled_when_given() {
        let tagged = GrokBoxComputer {
            run_tag: Some("gate-42".to_string()),
            ..GrokBoxComputer::new()
        };
        assert!(
            tagged
                .create_args("og-gb-x", "tokentokentokentoken12", "vncPass1")
                .iter()
                .any(|arg| arg == "dev.opengrok.run=gate-42")
        );
    }

    #[test]
    fn the_image_can_be_chosen() {
        let args = GrokBoxComputer::new()
            .with_image("grok-box:ci")
            .create_args("og-gb-x", "tokentokentokentoken12", "vncPass1");
        assert!(args.contains(&"grok-box:ci".to_string()));
    }

    #[test]
    fn minted_tokens_are_long_enough_that_the_guest_will_not_reject_them() {
        let token = mint_box_token();
        assert!(
            token.len() >= 32,
            "grok-box rejects short tokens: len {}",
            token.len()
        );
        assert_ne!(token, "dev-box-token");
        let password = mint_vnc_password();
        assert_eq!(password.len(), 8, "{password}");
    }

    #[test]
    fn the_viewer_url_carries_the_vnc_password_and_never_the_box_token() {
        let token = "super-secret-box-token-aaaaaaaa";
        let password = "Ab3DefgH";
        let url = viewer_url("127.0.0.1", 6080, password);
        assert!(url.contains("127.0.0.1:6080/vnc.html"), "{url}");
        assert!(url.contains("password=Ab3DefgH"), "{url}");
        assert!(!url.contains(token), "{url}");
        assert!(!url.contains("BOX_TOKEN"), "{url}");
    }

    #[test]
    fn docker_port_output_prefers_loopback_ipv4() {
        assert_eq!(
            parse_docker_port("127.0.0.1:49154\n[::1]:49154"),
            Some(49154)
        );
        assert_eq!(parse_docker_port("0.0.0.0:60080"), Some(60080));
        assert_eq!(parse_docker_port(""), None);
    }

    #[test]
    fn workspace_paths_use_the_files_api_and_tmp_does_not() {
        assert_eq!(files_api_path("/workspace/notes.txt"), Some("notes.txt"));
        assert_eq!(files_api_path("/workspace"), Some(""));
        assert_eq!(files_api_path("notes.txt"), Some("notes.txt"));
        assert_eq!(files_api_path("/tmp/secret.txt"), None);
    }

    #[tokio::test]
    async fn an_unpublished_port_is_refused_with_the_reason() {
        let error = GrokBoxComputer::new()
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
