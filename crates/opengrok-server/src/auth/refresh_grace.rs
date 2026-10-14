pub use opengrok_auth::refresh_grace::{GraceSlot, RefreshGrace};

#[cfg(test)]
#[path = "../../tests/unit/auth_refresh_grace.rs"]
mod tests;
