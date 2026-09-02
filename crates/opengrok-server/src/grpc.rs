//! The tonic listener — seam B's services as real gRPC, for internal callers.
//!
//! THE CLIENT EDGE IS NOT SERVED HERE. The desktop client speaks Connect unary over HTTP/1.1,
//! which `seamb.rs` answers on the Axum listener; a bare tonic server cannot answer it. This
//! listener exists for what tonic is actually good at — service-to-service gRPC on our own
//! network — and it serves the SAME transcribed contract (`opengrok-proto`), so an internal
//! caller and the desktop client can never disagree about what a message means.
//!
//! Opt-in by construction: no `OG_GRPC_BIND`, no listener. Auth is the same signed access token
//! as everywhere else, carried in the `authorization` metadata.

use tonic::{Request, Response, Status};

use opengrok_core::id::AccountId;
use opengrok_proto::aiserver::v1 as pb;

use crate::gateway::GatewayState;

#[derive(Clone)]
pub struct SeamBGrpc {
    pub state: GatewayState,
}

impl SeamBGrpc {
    fn account_from<T>(&self, request: &Request<T>) -> Result<AccountId, Status> {
        let token = request
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or_else(|| Status::unauthenticated("a signed access token is required"))?;
        let claims = self
            .state
            .agui
            .auth
            .minter
            .verify_access(token)
            .map_err(|_| Status::unauthenticated("bad token"))?;
        Ok(AccountId::from_stored(claims.sub))
    }
}

#[tonic::async_trait]
impl pb::dashboard_service_server::DashboardService for SeamBGrpc {
    async fn get_me(
        &self,
        request: Request<pb::GetMeRequest>,
    ) -> Result<Response<pb::GetMeResponse>, Status> {
        let account_id = self.account_from(&request)?;
        let email = self
            .state
            .agui
            .auth
            .store
            .load_account(&account_id)
            .await
            .map(|(account, _)| account.email.clone())
            .unwrap_or_default();
        let first_name = email.split('@').next().unwrap_or("Open Grok").to_string();
        Ok(Response::new(pb::GetMeResponse {
            auth_id: account_id.as_str().to_string(),
            user_id: 1,
            email: Some(email),
            first_name: Some(first_name),
            last_name: Some(String::new()),
        }))
    }

    async fn get_teams(
        &self,
        request: Request<pb::GetTeamsRequest>,
    ) -> Result<Response<pb::GetTeamsResponse>, Status> {
        self.account_from(&request)?;
        Ok(Response::new(pb::GetTeamsResponse {
            teams: vec![pb::Team {
                name: "Open Grok".to_string(),
                id: 1,
                seats: 1,
                has_billing: true,
                is_enterprise: false,
                team_slug: "opengrok".to_string(),
            }],
        }))
    }

    async fn get_user_privacy_mode(
        &self,
        request: Request<pb::GetUserPrivacyModeRequest>,
    ) -> Result<Response<pb::GetUserPrivacyModeResponse>, Status> {
        self.account_from(&request)?;
        Ok(Response::new(pb::GetUserPrivacyModeResponse {
            privacy_mode: pb::PrivacyMode::NoTraining.into(),
        }))
    }

    async fn get_team_admin_settings(
        &self,
        request: Request<pb::GetTeamAdminSettingsRequest>,
    ) -> Result<Response<pb::GetTeamAdminSettingsResponse>, Status> {
        self.account_from(&request)?;
        Ok(Response::new(pb::GetTeamAdminSettingsResponse {
            local_tool_controls: Some(pb::LocalToolControls {
                permission_ceiling: pb::LocalToolPermissionCeiling::Always.into(),
            }),
        }))
    }

    async fn get_team_admin_settings_or_empty_if_not_in_team(
        &self,
        request: Request<pb::GetTeamAdminSettingsRequest>,
    ) -> Result<Response<pb::GetTeamAdminSettingsResponse>, Status> {
        self.get_team_admin_settings(request).await
    }

    async fn update_user_name(
        &self,
        request: Request<pb::UpdateUserNameRequest>,
    ) -> Result<Response<pb::UpdateUserNameResponse>, Status> {
        self.account_from(&request)?;
        Ok(Response::new(pb::UpdateUserNameResponse {}))
    }
}

#[tonic::async_trait]
impl pb::grok_bot_service_server::GrokBotService for SeamBGrpc {
    async fn list_grok_bot_agents(
        &self,
        request: Request<pb::ListGrokBotAgentsRequest>,
    ) -> Result<Response<pb::ListGrokBotAgentsResponse>, Status> {
        let account_id = self.account_from(&request)?;
        let coworkers = self
            .state
            .agui
            .auth
            .store
            .coworkers_for(&account_id)
            .await
            .map_err(|_| Status::internal("roster unavailable"))?;
        let mut agents = Vec::new();
        for view in coworkers.iter().filter(|view| !view.retired) {
            let profile = self
                .state
                .agui
                .auth
                .store
                .seamb_profile(&view.id)
                .await
                .ok()
                .flatten();
            let field = |key: &str| -> String {
                profile
                    .as_ref()
                    .and_then(|profile| profile.get(key))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            agents.push(pb::GrokBotAgent {
                id: view.id.as_str().to_string(),
                legacy_agent_id: view.id.as_str().to_string(),
                agent_id: view.id.as_str().to_string(),
                name: view.name.clone(),
                description: field("description"),
                title: field("title"),
                avatar_shape: field("avatarShape"),
                avatar_color: field("avatarColor"),
                created_at_ms: view.updated_at_ms,
                updated_at_ms: view.updated_at_ms,
                harness: "box".to_string(),
                role: field("role"),
            });
        }
        Ok(Response::new(pb::ListGrokBotAgentsResponse { agents }))
    }

    async fn ensure_sand_box(
        &self,
        request: Request<pb::EnsureSandBoxRequest>,
    ) -> Result<Response<pb::EnsureSandBoxResponse>, Status> {
        let account_id = self.account_from(&request)?;
        let gateway_url = self.state.public_gateway_url.clone().unwrap_or_default();
        if gateway_url.is_empty() {
            return Err(Status::failed_precondition(
                "OG_PUBLIC_GATEWAY_URL is not configured; the mint has no address to hand out",
            ));
        }
        Ok(Response::new(pb::EnsureSandBoxResponse {
            cluster: "opengrok".to_string(),
            tenant_id: account_id.as_str().to_string(),
            pod_id: "opengrok-1".to_string(),
            network_token: String::new(),
            vnc_url: String::new(),
            fork_vnc_base_url: String::new(),
            gateway_url,
            gateway_token: self.state.bearer.clone().unwrap_or_default(),
        }))
    }
}

/// Serve both services until the process ends. Called only when `OG_GRPC_BIND` is set.
pub async fn serve(state: GatewayState, bind: std::net::SocketAddr) {
    let service = SeamBGrpc { state };
    tracing::info!(%bind, "tonic listening (internal gRPC, transcribed seam B)");
    let result = tonic::transport::Server::builder()
        .add_service(pb::dashboard_service_server::DashboardServiceServer::new(
            service.clone(),
        ))
        .add_service(pb::grok_bot_service_server::GrokBotServiceServer::new(
            service,
        ))
        .serve(bind)
        .await;
    if let Err(error) = result {
        tracing::error!(%error, "the tonic listener stopped");
    }
}
