//! Slice 1: replacing Cursor's OAuth.
//!
//! `docs/GOAL.md` slice 1. The client's whole auth backend is repointed with one environment
//! variable, so this module is the entire sign-in story for the desktop app.

pub mod routes;
pub mod token;

pub use routes::{AuthState, router};
pub use token::{TokenMinter, hash_refresh_token, mint_refresh_token};
