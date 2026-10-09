//! The in-chat form schema and its safe fill.
//!
//! The form contract stays separate from the general tool executor so a card can be parsed and
//! sanitized without pulling the executor's policy or network concerns into this crate.

pub mod user_form;
pub use user_form::*;
