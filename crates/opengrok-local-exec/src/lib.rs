//! The in-memory transport for commands sent to a person's own machine.
//!
//! Policy stays in the server; this crate only carries approved commands and decodes the daemon's
//! wire frames.

pub mod broker;
pub mod wire;
