//! The coworker's computer.
//!
//! ONE TRAIT, SEVERAL COMPUTERS. A coworker's computer is a seam, not a vendor: the harness asks
//! for a shell, a file, a port; something behind this trait provides them. The first
//! implementation drives box.ascii.dev; a local Docker one for tests and self-hosting comes next.
//! The client already models its own computer this way (`BoxEndpoint { host, port, authToken }`),
//! so keeping the seam here is what lets the same coworker run on either.
//!
//! WHY STREAMING IS ITS OWN METHOD. box.ascii.dev executes a command either synchronously
//! (blocking to 600s) or detached with a poll-only status endpoint — there is no live socket for
//! stdout. A caller that wants to show output while it happens must therefore poll, and pretending
//! otherwise behind a nice `Stream` would hide the latency from the person choosing a timeout. So
//! `run` is honest about being a single result, and `watch` is honest about being a poll.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub mod ascii;
pub mod docker;

pub use ascii::AsciiBoxes;
pub use docker::DockerComputer;

/// What a command did. `truncated` is carried rather than dropped: a tail is not the output, and a
/// coworker reasoning over a silently clipped log reaches confident wrong conclusions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub timed_out: bool,
}

/// A command still running, identified so its output can be polled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartedCommand {
    pub process_id: String,
    pub running: bool,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
}

#[derive(Debug, thiserror::Error)]
pub enum BoxError {
    #[error("the box is unreachable: {0}")]
    Unreachable(String),
    #[error("the box refused: {status} {body}")]
    Refused { status: u16, body: String },
    #[error("no box with that id")]
    NoSuchBox,
}

impl BoxError {
    /// A stable code the client maps to copy, independent of the (rewordable) message. Matches the
    /// server/client contract: invalid_key | quota_exceeded | provider_unreachable | provider_error.
    pub fn code(&self) -> &'static str {
        match self {
            BoxError::Unreachable(_) => "provider_unreachable",
            BoxError::Refused { status, .. } if *status == 401 || *status == 403 => "invalid_key",
            BoxError::Refused { status, .. } if *status == 402 || *status == 429 => {
                "quota_exceeded"
            }
            BoxError::Refused { .. } => "provider_error",
            BoxError::NoSuchBox => "provider_error",
        }
    }
}

pub type BoxResult<T> = std::result::Result<T, BoxError>;

/// A computer a coworker can work on.
#[async_trait]
pub trait Computer: Send + Sync {
    /// Bring a computer up. Returns the id the coworker's row will remember it by.
    async fn create(&self, ttl_seconds: Option<u64>) -> BoxResult<String>;

    /// Run to completion. For anything that might outlive a request, use `start` + `watch`.
    async fn run(
        &self,
        box_id: &str,
        command: &str,
        timeout_seconds: u32,
    ) -> BoxResult<CommandOutput>;

    /// Start something long-running and return immediately.
    async fn start(&self, box_id: &str, command: &str) -> BoxResult<StartedCommand>;

    /// Ask again. The tail is bounded by the provider; see the note about truncation above.
    async fn watch(&self, box_id: &str, process_id: &str) -> BoxResult<StartedCommand>;

    async fn read_file(&self, box_id: &str, path: &str) -> BoxResult<String>;
    async fn write_file(&self, box_id: &str, path: &str, content: &str) -> BoxResult<()>;

    /// Publish a port and get a URL a person can open.
    async fn expose_port(&self, box_id: &str, port: u16, title: &str) -> BoxResult<String>;

    /// Stop billing, keep the disk. `resume` brings the same filesystem back.
    async fn stop(&self, box_id: &str) -> BoxResult<()>;
    async fn resume(&self, box_id: &str) -> BoxResult<()>;

    /// Permanent. The disk goes with it.
    async fn destroy(&self, box_id: &str) -> BoxResult<()>;

    /// Which kind of computer this is, for advertising the options to a client:
    /// `"local-docker"` (a VM on the server host) or `"ascii"` (a box.ascii.dev box). Defaults to
    /// local-docker; the ascii provider overrides it.
    fn kind(&self) -> &'static str {
        "local-docker"
    }
}
