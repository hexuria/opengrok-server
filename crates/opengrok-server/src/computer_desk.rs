//! The computer tools' desk (7 Oct 2026, the owner's step 7): `opengrok_tools::computer_desk`'s
//! calls answered with the same functions the Computer pane's routes use (`agui/routes.rs`:
//! `computer_status`, `ensure_computer`, `computer_update`, `computer_reset`, `set_egress_policy`),
//! so a Bot may do to its own computer what its owner's pane may, refused in the same words.
//! Every call is answered for the `ToolContext`'s account and coworker only.

use opengrok_tools::ToolContext;
use opengrok_tools::computer_desk::{Ask, ComputerDesk};
use serde_json::{Value, json};

use crate::agui::AgUiState;
use crate::agui::provision;

pub struct Tools {
    pub state: AgUiState,
}

/// The pane's word for a network setting, from the tool's.
fn egress_mode(mode: &str) -> Option<&'static str> {
    Some(match mode {
        "always" => "bypass",
        "ask" => "ask",
        "never" => "never",
        _ => return None,
    })
}

impl Tools {
    /// The computer as the pane reads it, less its screen address (a model has no use for it).
    async fn status(&self, context: &ToolContext) -> Value {
        let mut body = provision::coworker_screen(
            &self.state,
            &axum::http::HeaderMap::new(),
            &context.account_id,
            &context.coworker_id,
        )
        .await;
        if let Some(object) = body.as_object_mut() {
            object.remove("vncUrl");
        }
        body
    }

    /// Make the computer if there is none yet, as the pane's Start does, then wake it.
    async fn start(&self, context: &ToolContext) -> Result<Value, String> {
        let (state, account, coworker_id) =
            (&self.state, &context.account_id, &context.coworker_id);
        let Ok((mut coworker, seq)) = state.auth.store.load_coworker(coworker_id).await else {
            return Err("this Bot could not be read now".into());
        };
        let at_ms = crate::now_ms();
        let provisioned =
            provision::ensure_computer_for(state, account, coworker_id, &mut coworker, at_ms).await;
        if !provisioned.events.is_empty() {
            for event in &provisioned.events {
                coworker.apply(event);
            }
            let view =
                opengrok_core::coworker::CoworkerView::of(coworker_id.clone(), &coworker, at_ms);
            state
                .auth
                .store
                .append_coworker(coworker_id, account, seq, &provisioned.events, &view)
                .await
                .map_err(|_| "this Bot's computer could not be recorded now".to_string())?;
        }
        provision::wake_coworker_computer(state, account, coworker_id).await;
        Ok(self.status(context).await)
    }

    async fn shutdown(&self, context: &ToolContext) -> Result<Value, String> {
        let Some(row) =
            provision::scoped_box_row_for(&self.state, &context.account_id, &context.coworker_id)
                .await
        else {
            return Err("this Bot has no computer to shut down".into());
        };
        let Some(provider) =
            provision::provider_for(&self.state, row.org_id.as_deref(), &row.kind).await
        else {
            return Err("this Bot's computer cannot be reached from this server".into());
        };
        provider
            .stop(&row.box_id)
            .await
            .map_err(|error| format!("the computer did not shut down: {}", error.code()))?;
        crate::agui::screen_proxy::forget_box(&row.box_id);
        let _ = self
            .state
            .auth
            .store
            .mark_scoped_stopped(row.scope, &row.scope_id)
            .await;
        Ok(self.status(context).await)
    }
}

#[async_trait::async_trait]
impl ComputerDesk for Tools {
    async fn answer(&self, context: &ToolContext, ask: Ask) -> Result<Value, String> {
        let (state, account, coworker) = (&self.state, &context.account_id, &context.coworker_id);
        match ask {
            Ask::Status => Ok(self.status(context).await),
            Ask::Start => self.start(context).await,
            Ask::Shutdown => self.shutdown(context).await,
            Ask::Restart => {
                self.shutdown(context).await?;
                self.start(context).await
            }
            Ask::Reset => {
                provision::reset_for_coworker(state, account, coworker)
                    .await
                    .map_err(|(_, message)| message)?;
                Ok(self.status(context).await)
            }
            Ask::Update => {
                let update = provision::begin_update_for_coworker(state, account, coworker)
                    .await
                    .map_err(|(_, message)| message)?;
                tokio::spawn(update);
                Ok(json!({ "updating": true,
                    "note": "the computer is being rebuilt on the newest image; its files are kept" }))
            }
            Ask::SetNetwork { mode } => {
                let Some(stored) = egress_mode(&mode) else {
                    return Err("mode is always, ask or never".into());
                };
                let Some(row) = provision::scoped_box_row_for(state, account, coworker).await
                else {
                    return Err("this Bot has no computer yet".into());
                };
                // An org-shared computer's network is the org admin's to set, as on the pane.
                if row.scope == "org" {
                    return Err(
                        "this computer is shared by the org: its network setting is the org \
                         admin's to change"
                            .into(),
                    );
                }
                state
                    .auth
                    .store
                    .set_egress_policy_mode(row.scope, &row.scope_id, stored, crate::now_ms())
                    .await
                    .map_err(|_| "the network setting could not be saved now".to_string())?;
                Ok(json!({ "network": mode }))
            }
        }
    }

    async fn ask_first(&self, _: &ToolContext, ask: &Ask) -> Result<Option<String>, String> {
        Ok(match ask {
            Ask::Reset => {
                Some("Reset this Bot's computer? Everything on it is deleted for good.".to_string())
            }
            Ask::Update => Some(
                "Update this Bot's computer to the newest image? It is rebuilt, keeping its files."
                    .to_string(),
            ),
            Ask::SetNetwork { mode } => Some(format!(
                "Set this Bot's computer's network use to {}?",
                match mode.as_str() {
                    "always" => "always",
                    "ask" => "ask each time",
                    _ => "never",
                }
            )),
            _ => None,
        })
    }
}
