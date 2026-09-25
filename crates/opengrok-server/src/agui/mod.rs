//! Slice 2: the AG-UI endpoint openbot connects to.
//!
//! `docs/GOAL.md` slice 2. This is the spine — the harness, the boxes and the tools all reach a
//! client through this stream.

pub mod chat_ui;
pub mod credential;
pub(crate) mod history;
pub mod passkeys;
pub mod pending;
pub mod provision;
pub mod resume;
pub mod routes;
pub mod site_logins;
pub mod user_form;

pub use routes::{AgUiState, router, run_router, to_chat_messages};
