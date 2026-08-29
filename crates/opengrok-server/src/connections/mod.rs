//! Connectors: authenticating a provider, and lending the result to a coworker.
//!
//! `oauth` is pure — URLs, state, replies — so the provider quirks are assertable rather than
//! folklore. The HTTP and the endpoints sit beside it.

pub mod flow;
pub mod oauth;

pub use flow::{FlowError, exchange_code, refresh, sign_state, verify_state};
pub use oauth::{Pkce, ProviderConfig, StateClaims, TokenError, TokenResponse};
