use async_trait::async_trait;
use opengrok_box::{BoxResult, CommandOutput, Computer, StartedCommand};
use std::sync::Mutex;

#[derive(Default)]
pub struct RecordingComputer {
    boxes: Mutex<Vec<String>>,
    /// Scripted states, the last repeating; empty means "running" from the start.
    states: Mutex<std::collections::VecDeque<&'static str>>,
    resumes: std::sync::atomic::AtomicUsize,
    /// How long every command takes to answer: a tool whose time is known.
    pause_ms: u64,
}

impl RecordingComputer {
    /// A box that reports these states in order (the last one repeats).
    pub fn sleeping(states: &[&'static str]) -> Self {
        Self {
            states: Mutex::new(states.iter().copied().collect()),
            ..Self::default()
        }
    }

    /// A running box whose every command takes `ms` to answer.
    pub fn taking(ms: u64) -> Self {
        Self {
            pause_ms: ms,
            ..Self::default()
        }
    }

    pub fn last_box(&self) -> Option<String> {
        self.boxes
            .lock()
            .ok()
            .and_then(|calls| calls.last().cloned())
    }

    pub fn resumes(&self) -> usize {
        self.resumes.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl Computer for RecordingComputer {
    async fn create(&self, _ttl: Option<u64>) -> BoxResult<String> {
        Ok("box_new".to_string())
    }
    async fn run(&self, box_id: &str, command: &str, _t: u32) -> BoxResult<CommandOutput> {
        if let Ok(mut boxes) = self.boxes.lock() {
            boxes.push(box_id.to_string());
        }
        if self.pause_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(self.pause_ms)).await;
        }
        Ok(CommandOutput {
            exit_code: 0,
            stdout: format!("ran `{command}`"),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            timed_out: false,
        })
    }
    async fn start(&self, _b: &str, _c: &str) -> BoxResult<StartedCommand> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn watch(&self, _b: &str, _p: &str) -> BoxResult<StartedCommand> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn read_file(&self, box_id: &str, _p: &str) -> BoxResult<String> {
        if let Ok(mut boxes) = self.boxes.lock() {
            boxes.push(box_id.to_string());
        }
        Ok(String::new())
    }
    async fn write_file(&self, box_id: &str, _p: &str, _c: &str) -> BoxResult<()> {
        if let Ok(mut boxes) = self.boxes.lock() {
            boxes.push(box_id.to_string());
        }
        Ok(())
    }
    async fn expose_port(&self, _b: &str, _p: u16, _t: &str) -> BoxResult<String> {
        Ok(String::new())
    }
    async fn stop(&self, _b: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn resume(&self, _b: &str) -> BoxResult<()> {
        self.resumes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    async fn state(&self, _b: &str) -> BoxResult<String> {
        let mut states = self
            .states
            .lock()
            .map_err(|_| opengrok_box::BoxError::NoSuchBox)?;
        let next = if states.len() > 1 {
            states.pop_front().unwrap_or("running")
        } else {
            states.front().copied().unwrap_or("running")
        };
        Ok(next.to_string())
    }
    async fn destroy(&self, _b: &str) -> BoxResult<()> {
        Ok(())
    }
}
