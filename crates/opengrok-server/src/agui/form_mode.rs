//! Form mode (#220, Tier 1): a card the chat box drives, whose boxes a plugin holds.
//!
//! What a card's verbs decide is in `opengrok_integrations::form_mode`; the server serves its
//! route and gives it what only the server has (`CardHost`): the store and plugins, the Bot's
//! lock, and the Bot's tools for one call.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_integrations::form_mode::{self, CardHost};
use opengrok_integrations::turn::TurnPlugins;
use opengrok_tools::{ToolCall, ToolResult};
use serde_json::Value;

use crate::host_state::HostState;

#[async_trait::async_trait]
impl CardHost for HostState {
    fn store(&self) -> &opengrok_store::PgStore {
        &self.agui.auth.store
    }

    fn operator(&self) -> &BTreeMap<String, opengrok_plugins::Plugin> {
        &self.agui.plugins
    }

    fn lock(&self, coworker: &CoworkerId) -> Arc<tokio::sync::Mutex<()>> {
        crate::mcp_door::coworker_lock(coworker)
    }

    async fn run(
        &self,
        (account, coworker): (&AccountId, &CoworkerId),
        turn: &TurnPlugins,
        (approved, review_approved): (Vec<String>, Vec<String>),
        call: &ToolCall,
    ) -> Option<ToolResult> {
        let gates: (&[String], &[String]) = (&approved, &review_approved);
        let patience = super::routes::TURN_WAKE_PATIENCE;
        let runner =
            super::routes::tools_for_turn(&self.agui, account, coworker, gates, patience, turn);
        Some(runner.await?.run_one(call).await)
    }
}

/// `POST /ag-ui/user-form/action`: one press on a form-mode card. Authenticated like the card's
/// other verbs (`/ag-ui/user-form/submit`): the account bearer, never a host or bot key.
pub fn router(host: HostState) -> Router {
    Router::new()
        .route("/ag-ui/user-form/action", post(action_route))
        .with_state(host)
}

async fn action_route(
    State(host): State<HostState>,
    headers: HeaderMap,
    axum::Json(args): axum::Json<Value>,
) -> Response {
    let Some(account) = super::routes::account_from_bearer(&host.agui, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let (code, body) = form_mode::act(&host, &args, &account).await;
    let code = StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (code, axum::Json(body)).into_response()
}
