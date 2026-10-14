//! `POST /ag-ui` — the endpoint openbot adds as a Bot.
//!
//! One request opens one SSE stream carrying the events of a single run. openbot supplies the
//! thread and run ids (`RunAgentInput`), so we do not mint them: the client correlates its own UI
//! against those values, and inventing our own would orphan the reply.
//!
//! THE ENVELOPE IS THE CONTRACT, EVEN WHEN THE MIDDLE IS A STUB. A run that starts must finish or
//! error — `RUN_STARTED` … `RUN_FINISHED` — because a consumer holds its spinner open on the
//! promise of that closing event. This slice streams a real, correctly-shaped conversation with a
//! placeholder body; slice 3 replaces the middle with the harness, and the framing does not change.

use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::Stream;
use opengrok_wire::agui::{Event, RunAgentInput};

use super::{attachments, chat_ui, history, provision, resume};
use crate::auth::AuthState;
use crate::inference::effort_refused;
use opengrok_core::coworker::{CoworkerCommand, CoworkerView, Effort};
use opengrok_core::id::{AccountId, CoworkerId, RunId};
use opengrok_core::inference::{SourceKind, TurnSource};
use opengrok_core::limits::RunLimits;
use opengrok_core::run::{OfferedSkill, RunCommand, RunStatus, RunView};
use opengrok_harness::{
    ChatMessage, EventSink, ModelDoor, ModelRequest, RunBudget, RunContext, ToolRunner,
    local_proxy, run_conversation_within,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// How long the first box-bound tool call of a turn waits for a sleeping box to come up before
/// answering that the computer is down. The wait moved from before the model was asked (every
/// turn paid it) to the tool that needs the box (only those turns pay it).
/// A box.ascii.dev resume restores a snapshot onto a fresh machine: archived → provisioned →
/// running took 10–15s live (bx_ncfmdpem, 2 Sep 2026); 90s leaves room for a slow restore.
pub(crate) const TURN_WAKE_PATIENCE: std::time::Duration = std::time::Duration::from_secs(90);

/// What the endpoint needs: a way to reach a model, and which route to ask for.
///
/// The door is a trait object so `OG_MODEL_DOOR=mock` swaps the whole model layer without the
/// endpoint, the harness or the projection knowing — which is what lets CI exercise this path.
#[derive(Clone)]
pub struct AgUiState {
    pub auth: AuthState,
    pub door: Arc<dyn ModelDoor>,
    pub model: String,
    /// The computer provider. Tools are bound to a coworker's own box **per request**, not here:
    /// a server-wide `ToolRunner` would carry one identity for everybody, which is precisely the
    /// confusion the identity rule exists to prevent.
    pub computer: Option<Arc<dyn opengrok_box::Computer>>,
    /// The route the auto-review judge asks on — the deployment's own, never the coworker's: one
    /// call per reviewed tool call must be cheap, the reviewer must not be the reviewed, and a
    /// coworker-route outage must not become a wall of cards. `OG_AUTO_REVIEW_MODEL`.
    pub auto_review_model: String,
    /// Seals every credential: connector tokens, org computer keys, coworker gateway keys, saved
    /// site logins. `None` is a legitimate deployment that stores none, not a crash.
    pub vault: Option<Arc<opengrok_store::Vault>>,
    /// Provider configuration and the callback URL.
    pub connectors: crate::connections::routes::Connectors,
    /// Plugins installed on this server, by name. Installing one makes it *available*; a coworker
    /// still needs it in their ceiling before its tools run.
    pub plugins: Arc<BTreeMap<String, opengrok_plugins::Plugin>>,
    /// Shared with `HostState.settings` so AG-UI turns see `egressTunnelEnabled`.
    /// `None` until `HostState::new` / `router` attach the Arc; env flags still apply.
    pub host_settings: Option<Arc<Mutex<serde_json::Value>>>,
}

impl AgUiState {
    /// How many tokens a turn on `pin` may send (#90): `models::context_for` on this deployment.
    pub(crate) async fn context_for(&self, pin: &str) -> Option<u64> {
        crate::models::context_for(
            self.auth.model_catalogue.as_deref(),
            self.auth.context_tokens,
            pin,
        )
        .await
    }

    /// Env `OG_EGRESS_TUNNEL_ENABLED=1` / `SAND_EGRESS_TUNNEL_ENABLED=1` (Grok host parity),
    /// or host setting `egressTunnelEnabled`. This is host *intent*. The gateway verb
    /// and Review-an-action gate also need `/v1/info` `egress_tunnel.ready`.
    #[must_use]
    pub fn egress_tunnel_enabled(&self) -> bool {
        let settings = self
            .host_settings
            .as_ref()
            .and_then(|lock| lock.lock().ok().map(|value| value.clone()))
            .unwrap_or_else(crate::host_state::default_settings);
        crate::host_state::egress_tunnel_available(&settings)
    }

    /// Host intent AND this box's `egress_tunnel.ready`. Failed info → false.
    /// Review-an-action for leave-box tools uses this, not host intent alone.
    pub async fn egress_tunnel_for(
        &self,
        computer: &dyn opengrok_box::Computer,
        box_id: &str,
    ) -> bool {
        let host_wants = self.egress_tunnel_enabled();
        if !host_wants {
            return false;
        }
        opengrok_box::EgressTunnel::advertised(true, computer.egress_tunnel(box_id).await)
    }
}

/// Which coworker a run belongs to, and therefore whose computer its tools use.
///
/// AG-UI has no field for this, so the client passes it in `forwardedProps` — and it is a
/// *request*, not an authorisation: the id names a coworker, and the box comes from that
/// coworker's own row. A client naming a coworker it does not own is the next thing policy must
/// check (slice 5); today the row simply has to exist.
/// `POST /ag-ui` lives on `HostState` so a UserForm CUSTOM can mint the gateway card and
/// stamp `entryId` on the SSE frame NativeChat receives. So does the answer to a card: the
/// run it continues can raise a form of its own, and a form with no card entry has no
/// `entryId` for NativeChat to submit to — its Log in button stayed grey (21 Sep 2026).
/// Other AG-UI routes stay on `AgUiState`. The SSE still forwards CUSTOM
/// `run-awaiting-approval`; the card is additive.
/// The stop lives here too: a run parked on a form leaves a card that holds the coworker's
/// screen, and the stop closes it before answering.
pub fn run_router(state: crate::host_state::HostState) -> Router {
    Router::new()
        .route("/ag-ui", post(run))
        .route("/ag-ui/runs/{run_id}/answer", post(answer_run))
        .route("/ag-ui/runs/{run_id}/stop", post(stop_run))
        .with_state(state)
}

pub fn router(state: AgUiState) -> Router {
    Router::new()
        .route("/ag-ui/runs/{run_id}", get(replay_run))
        .route("/ag-ui/runs/{run_id}/hide", post(hide_run))
        .route("/ag-ui/events", get(account_events))
        .route("/ag-ui/threads", get(list_threads))
        .route("/ag-ui/threads/{thread_id}", get(replay_thread))
        .route("/ag-ui/approvals", get(list_awaiting))
        .route(
            "/ag-ui/host-settings",
            get(host_settings).put(patch_host_settings),
        )
        .route("/coworkers", post(hire).get(list_coworkers))
        .route("/models", get(list_models))
        // The org's coworker templates, for the hire picker. Written by the admin
        // (`account_api.rs`, `/admin/templates`).
        .route("/templates", get(list_templates))
        .route("/models/probe", post(probe_model))
        // A fault the desktop app is about to report, weighed first (`crate::triage`).
        .route("/triage", post(crate::triage::triage_fault))
        .route(
            "/coworkers/{coworker_id}",
            axum::routing::patch(repin_coworker).delete(delete_coworker),
        )
        .route("/coworkers/{coworker_id}/approvals", post(set_approvals))
        // Spend limits: the coworker's three meters, read-only here; limits are written by the
        // org admin (`account_api.rs`, `/admin/spend`).
        .route("/coworkers/{coworker_id}/spend", get(get_spend))
        // Points (`points.rs`): usage per model, a report; and the coworker's own limit, the
        // owner's to write (its cap for the month, its brake for the day).
        .route("/coworkers/{coworker_id}/usage", get(get_usage))
        .route(
            "/coworkers/{coworker_id}/limit",
            get(get_limit).put(set_limit),
        )
        .route(
            "/coworkers/{coworker_id}/keys",
            post(mint_bot_key).get(list_bot_keys),
        )
        .route(
            "/coworkers/{coworker_id}/keys/{jti}",
            axum::routing::delete(revoke_bot_key),
        )
        .route("/coworkers/{coworker_id}/mcp-calls", get(list_mcp_calls))
        .route(
            "/coworkers/{coworker_id}/computer",
            get(computer_status).post(ensure_computer),
        )
        .route("/coworkers/{coworker_id}/screen", get(computer_screen))
        .route(
            "/coworkers/{coworker_id}/computer/vnc/{ticket}/{*rest}",
            get(super::screen_proxy::serve),
        )
        .route("/coworkers/{coworker_id}/tools", get(list_tools))
        // The same tools as switches its owner sets (#268, `ceiling.rs`).
        .route(
            "/coworkers/{coworker_id}/ceiling",
            get(super::ceiling::get_ceiling).put(super::ceiling::put_ceiling),
        )
        // One tool's choice for this Bot: always, ask or never (#359, `ceiling.rs`).
        .route(
            "/coworkers/{coworker_id}/tool-mode",
            axum::routing::put(super::ceiling::put_tool_mode),
        )
        // Whether a saved login can be used for this Bot, asked before Touch ID, and the Bot's own
        // computer apart from the account's shared one (`ceiling.rs`, 8 Oct 2026).
        .route(
            "/coworkers/{coworker_id}/saved-login",
            get(super::ceiling::get_saved_login),
        )
        .route(
            "/coworkers/{coworker_id}/own-computer",
            axum::routing::put(super::ceiling::put_own_computer),
        )
        // One plugin skill on or off for this Bot, and its text for its page (`ceiling.rs`).
        .route(
            "/coworkers/{coworker_id}/plugin-skills",
            get(super::ceiling::list_plugin_skills).put(super::ceiling::put_plugin_skill),
        )
        .route(
            "/coworkers/{coworker_id}/plugin-skills/{plugin}/{skill}",
            get(super::ceiling::get_plugin_skill),
        )
        .route(
            "/coworkers/{coworker_id}/computer/update",
            post(computer_update),
        )
        .route(
            "/coworkers/{coworker_id}/computer/egress-policy",
            get(get_egress_policy).put(set_egress_policy),
        )
        .route(
            "/coworkers/{coworker_id}/computer/reset",
            post(computer_reset),
        )
        .with_state(state.clone())
        // Pending user messages: NativeChat's follow-up queue. Nested under the thread they
        // belong to, because `GET /ag-ui/threads/{id}` is already how other clients hydrate.
        .merge(super::pending::router(state))
}

mod turn_input;
#[allow(unused_imports)]
pub use turn_input::*;

mod turn_tools;
#[allow(unused_imports)]
pub use turn_tools::*;

mod coworkers;
pub use coworkers::*;

mod computer;
pub use computer::*;

mod turn;
pub use turn::*;

mod journal;
pub use journal::*;

mod threads;
pub use threads::*;

mod answer;
pub use answer::*;

mod host;
pub use host::*;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "../../tests/unit/routes.rs"]
mod tests;
