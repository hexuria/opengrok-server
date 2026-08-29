//! box.ascii.dev — persistent Ubuntu VMs behind a plain REST API.
//!
//! Bearer-token auth, JSON in and out, so a `reqwest` client is the whole integration; there is no
//! Rust SDK and none is needed. Verified surface (docs.ascii.dev/box/api/v1):
//!   POST   /boxes                              create
//!   POST   /boxes/{id}/commands                run — sync, or `detached` for background
//!   GET    /boxes/{id}/commands/{processId}    poll a detached command's tail
//!   GET    /boxes/{id}/files?path=             read
//!   PUT    /boxes/{id}/files                   write
//!   POST   /boxes/{id}/host                    expose a port, get a preview URL
//!   POST   /boxes/{id}/stop | /resume | /fork  lifecycle
//!   DELETE /boxes/{id}                         destroy
//!
//! TWO SHAPES ARE NOT YET PINNED and are marked at their call sites rather than guessed: the field
//! name carrying a created box's id, and the confirmation header `DELETE` requires. The first
//! slice's task is to hit a real box and write them down.

pub const DEFAULT_BASE_URL: &str = "https://ascii.dev/api/box/v1";

/// Where the boxes live and the key that opens them.
///
/// The key is read from the environment and never from a coworker's row: a computer is not a
/// credential a client may set, which is the same rule the gateway applies to model keys.
// Scaffold: the `Computer` impl that consumes these lands in slice 4 (docs/GOAL.md). Kept rather
// than deleted because the auth shape is verified against the real API docs and re-deriving it
// would repeat that work.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct AsciiBoxes {
    pub base_url: String,
    api_key: String,
}

impl AsciiBoxes {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key: api_key.into(),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn bearer(&self) -> String {
        format!("Bearer {}", self.api_key)
    }
}
