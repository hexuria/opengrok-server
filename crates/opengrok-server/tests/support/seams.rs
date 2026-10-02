//! Test seams: the builders and reads only tests call, and the TXT lookup a test answers from a
//! map.
//!
//! Mounted from `src/lib.rs` with `#[path]`, like `tests/support/mock_jev.rs`: they compile into
//! the crate every integration test links, yet live beside the tests that use them — nothing the
//! server runs reaches for one, so none of it is counted as the server (`scripts/crate-size.sh`).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::auth::AuthState;
use crate::domain_proof::TxtLookup;

impl AuthState {
    /// Where an org's box.ascii.dev key is used (see `ascii_base_url`).
    #[must_use]
    pub fn with_ascii_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.ascii_base_url = base_url.into();
        self
    }

    /// Allow a client id metadata document on a loopback address.
    #[must_use]
    pub fn with_cimd_loopback(mut self) -> Self {
        self.cimd_allow_loopback = true;
        self
    }

    /// Send mail somewhere other than Resend — a test's stand-in mailbox.
    #[must_use]
    pub fn with_resend_endpoint(mut self, endpoint: String) -> Self {
        self.resend_endpoint = endpoint;
        self
    }

    /// Point the catalogue at an explicit gateway — a stand-in for a real one, without touching
    /// the process environment.
    #[must_use]
    pub fn with_model_catalogue(
        mut self,
        catalogue: Option<Arc<crate::models::ModelCatalogue>>,
    ) -> Self {
        self.model_catalogue = catalogue;
        self
    }

    /// Hold turns to `tokens` when the catalogue cannot say; `None` turns the guard off.
    #[must_use]
    pub fn with_context_tokens(mut self, tokens: Option<u64>) -> Self {
        self.context_tokens = tokens;
        self
    }

    /// Point the gateway's admin door somewhere explicit — a stand-in for the real gateway,
    /// without touching the process environment.
    #[must_use]
    pub fn with_gateway_admin(mut self, admin: Option<crate::gateway_admin::GatewayAdmin>) -> Self {
        self.gateway_admin = admin;
        self
    }

    /// Point Jev somewhere explicit — to answer deterministically with no key, no spend and no
    /// network, the way `MockDoor` stands in for the model door.
    #[must_use]
    pub fn with_jev(mut self, jev: Option<crate::jev::SharedJev>) -> Self {
        self.jev = jev;
        self
    }
}

impl crate::spend::GuardedDoor {
    /// Reuse a reading for this long. Zero ⇒ every call reads the meter — for tests that assert
    /// on the meter itself; production keeps the default so a tool loop pays for one read.
    #[must_use]
    pub fn with_fresh_ms(mut self, fresh_ms: i64) -> Self {
        self.fresh_ms = fresh_ms;
        self
    }
}

/// Take the pending allow-once, as the door does, without the stamp the door gives back with it.
/// Reached as `mcp_door::take_mcp_allow_once`.
pub async fn take_mcp_allow_once(
    store: &opengrok_store::PgStore,
    coworker: &opengrok_core::id::CoworkerId,
    account: Option<&str>,
    tool: &str,
    arguments: &serde_json::Value,
) -> Option<(String, bool)> {
    crate::mcp_door::take_mcp_allow_once_stamped(store, coworker, account, tool, arguments)
        .await
        .map(|(call_id, gate, _)| (call_id, gate))
}

/// A body's messages in the model's vocabulary, with no files read: what the unit tests turn a
/// body into (`agui::routes::to_chat_messages_with`).
#[cfg(test)]
pub(crate) fn to_chat_messages(
    input: &opengrok_wire::agui::RunAgentInput,
) -> Vec<opengrok_harness::ChatMessage> {
    let attached = crate::agui::attachments::Attached::default();
    crate::agui::routes::to_chat_messages_with(input, &attached)
}

impl crate::local_exec::broker::ExecOutcome {
    /// Did the command run to a normal completion (exit 0)?
    pub fn succeeded(&self) -> bool {
        self.case == "success" && self.exit_code == Some(0)
    }
}

/// A TXT lookup answered from a map, which a test publishes records into — so a claim can be
/// proven end to end without owning a domain. Reached as `domain_proof::StaticDns`.
#[derive(Default)]
pub struct StaticDns {
    records: tokio::sync::RwLock<BTreeMap<String, Vec<String>>>,
}

impl StaticDns {
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish (or replace) the TXT values at `name`.
    pub async fn publish(&self, name: &str, values: Vec<String>) {
        self.records
            .write()
            .await
            .insert(name.trim_end_matches('.').to_string(), values);
    }
}

#[async_trait]
impl TxtLookup for StaticDns {
    async fn txt(&self, name: &str) -> Result<Vec<String>, String> {
        Ok(self
            .records
            .read()
            .await
            .get(name.trim_end_matches('.'))
            .cloned()
            .unwrap_or_default())
    }
}

/// `message_bot` as one run of `sender`'s, driven by `person`, is given it (#314): what a test
/// carries one call out with twice, as a sender resumed after its call would.
pub async fn message_bot_runner(
    state: &crate::agui::AgUiState,
    person: &opengrok_core::id::AccountId,
    sender: &opengrok_core::id::CoworkerId,
    run_id: &str,
) -> Option<opengrok_harness::ToolRunner> {
    crate::pairs::onto(state, (person, sender), Some(run_id), None, None).await
}
