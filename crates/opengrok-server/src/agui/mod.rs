//! Slice 2: the AG-UI endpoint openbot connects to.
//!
//! `docs/GOAL.md` slice 2. This is the spine — the harness, the boxes and the tools all reach a
//! client through this stream.

pub mod routes;

pub use routes::{plan_run, router};
