//! Seam B: the ConnectRPC backend the desktop client was born dialling.
//!
//! Connect unary over HTTP/1.1 — `POST /aiserver.v1.<Service>/<Method>` with a JSON body — is
//! the one transport the client actually speaks (`cursor-inference.ts:157` builds its transport
//! with `httpVersion: "1.1"`); a bare gRPC server cannot answer it, which is why these routes
//! live on the same Axum listener as everything else.
//!
//! EVERY MESSAGE SHAPE HERE IS TRANSCRIBED, WITH PROVENANCE, FROM THE CLIENT'S OWN MOCK AND
//! GENERATED TYPES — `source/mock/*.ts` for behaviour, `dashboard_pb.ts` / `grok_bot_pb.ts` /
//! `sand_box_pb.ts` for field names — and nothing generated is vendored (docs/LEGAL.md). The
//! mock is the working minimum: two services, eighteen methods, and an app that boots against
//! them. Where the mock answers an empty message for a method it does not model, so do we —
//! that leniency is load-bearing, it is what lets the client boot without all 46 methods.
//!
//! Proto3 JSON rules honoured: 64-bit integers are emitted as strings, bytes as base64, enums
//! by their declared names; absent fields mean their defaults.

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use base64::Engine as _;
use serde_json::{Value, json};

use opengrok_core::coworker::{Coworker, CoworkerCommand};
use opengrok_core::id::CoworkerId;

use crate::gateway::GatewayState;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn router(state: GatewayState) -> Router {
    Router::new()
        .route("/aiserver.v1.DashboardService/{method}", post(dashboard))
        .route("/aiserver.v1.GrokBotService/{method}", post(grok_bot))
        .with_state(state)
}

/// A Connect unary error: HTTP status plus `{code, message}` in the body.
fn connect_error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        json!({ "code": code, "message": message }).to_string(),
    )
        .into_response()
}

fn connect_ok(body: Value) -> Response {
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

/// Whose backend call this is. Seam B is account-scoped — unlike the gateway, every request
/// carries the signed access token slice 1 mints.
// The boxed refusal keeps the Ok path's return size small; clippy's large-Err lint is right that
// a 128-byte Err on every call would be paid even on success.
#[allow(clippy::result_large_err)]
fn account_from(
    state: &GatewayState,
    headers: &HeaderMap,
) -> Result<opengrok_core::id::AccountId, Response> {
    crate::agui::routes::account_from_bearer(&state.agui, headers).ok_or_else(|| {
        connect_error(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "a signed access token is required",
        )
    })
}

/// An empty body is an empty message; a NON-EMPTY body that is not JSON is a client error and
/// must be refused — absorbing it as `{}` once minted a blank agent named "Grok" from a
/// malformed create, which is precisely the silent failure proto parsing exists to prevent.
#[allow(clippy::result_large_err)] // same bargain as account_from: the Err is a whole Response
fn parse_args(body: &str) -> Result<Value, Response> {
    if body.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(body).map_err(|_| {
        connect_error(
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "the request body is not JSON",
        )
    })
}

/// Accepts proto3 JSON's both spellings of an int64: number or string.
fn as_ms(value: Option<&Value>) -> Option<i64> {
    match value {
        Some(Value::Number(number)) => number.as_i64(),
        Some(Value::String(text)) => text.parse().ok(),
        _ => None,
    }
}

/// `aiserver.v1.DashboardService` — six methods carry the boot (`source/mock/dashboard-handlers.ts`).
async fn dashboard(
    State(state): State<GatewayState>,
    Path(method): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let account_id = match account_from(&state, &headers) {
        Ok(account_id) => account_id,
        Err(refusal) => return refusal,
    };
    let args = match parse_args(&body) {
        Ok(args) => args,
        Err(refusal) => return refusal,
    };

    let email = state
        .agui
        .auth
        .store
        .load_account(&account_id)
        .await
        .map(|(account, _)| account.email.clone())
        .unwrap_or_default();
    let first_name = email.split('@').next().unwrap_or("OpenGrok").to_string();

    match method.as_str() {
        // Field names from `dashboard_pb.ts` GetMeResponse.
        "GetMe" => connect_ok(json!({
            "authId": account_id.as_str(),
            "userId": 1,
            "email": email,
            "firstName": first_name,
            "lastName": "",
        })),
        // One team, the mock's shape (`Team` in dashboard_pb.ts); ids are ours.
        "GetTeams" => connect_ok(json!({
            "teams": [{
                "id": 1,
                "name": "OpenGrok",
                "role": "TEAM_ROLE_OWNER",
                "seats": 1,
                "hasBilling": true,
                "isEnterprise": false,
                "teamSlug": "opengrok",
            }]
        })),
        "GetUserPrivacyMode" => connect_ok(json!({
            "privacyMode": "PRIVACY_MODE_NO_TRAINING",
        })),
        "GetTeamAdminSettings" | "GetTeamAdminSettingsOrEmptyIfNotInTeam" => connect_ok(json!({
            "localToolControls": {
                "permissionCeiling": "LOCAL_TOOL_PERMISSION_CEILING_ALWAYS",
            }
        })),
        "UpdateUserName" => {
            // The mock stores the name and answers an empty message; we have nowhere better for
            // it yet either. Accepting it is the contract; the empty reply is proto3 defaults.
            let _ = args;
            connect_ok(json!({}))
        }
        other => {
            // The mock answers an empty message for anything it does not model, and the client
            // boots BECAUSE of that leniency. Mirror it.
            tracing::debug!(method = other, "DashboardService default empty reply");
            connect_ok(json!({}))
        }
    }
}

/// A `GrokBotAgent` (grok_bot_pb.ts) from a coworker plus its stored profile extras.
fn agent_json(view: &opengrok_core::coworker::CoworkerView, profile: Option<&Value>) -> Value {
    let field = |key: &str| -> String {
        profile
            .and_then(|profile| profile.get(key))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    json!({
        "id": view.id.as_str(),
        "legacyAgentId": view.id.as_str(),
        "agentId": view.id.as_str(),
        "name": view.name,
        "description": field("description"),
        "title": field("title"),
        "avatarShape": field("avatarShape"),
        "avatarColor": field("avatarColor"),
        // int64s travel as strings in proto3 JSON.
        "createdAtMs": view.updated_at_ms.to_string(),
        "updatedAtMs": view.updated_at_ms.to_string(),
        "harness": "box",
        "role": field("role"),
    })
}

/// One seam-B transcript entry from a stored gateway entry: the body is the client-shaped JSON,
/// as bytes, base64 — exactly how the mock's store ships `LocalTranscriptBody`.
fn transcript_entry_json(seq: i64, entry: &Value) -> Value {
    let body = base64::engine::general_purpose::STANDARD.encode(entry.to_string());
    json!({
        "seq": seq.to_string(),
        "entryKind": entry.get("kind").and_then(Value::as_str).unwrap_or("message"),
        "body": body,
        "updatedSeq": seq.to_string(),
        "entryId": entry.get("id").and_then(Value::as_str).unwrap_or_default(),
        "bodyOmitted": false,
    })
}

/// `aiserver.v1.GrokBotService` — the mock's twelve, plus `EnsureSandBox` (P1, the mint).
async fn grok_bot(
    State(state): State<GatewayState>,
    Path(method): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let account_id = match account_from(&state, &headers) {
        Ok(account_id) => account_id,
        Err(refusal) => return refusal,
    };
    let args = match parse_args(&body) {
        Ok(args) => args,
        Err(refusal) => return refusal,
    };
    let store = state.agui.auth.store.clone();

    match method.as_str() {
        "ListGrokBotAgents" => {
            let Ok(coworkers) = store.coworkers_for(&account_id).await else {
                return connect_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "roster unavailable",
                );
            };
            let mut agents = Vec::new();
            for view in coworkers.iter().filter(|view| !view.retired) {
                let profile = store.seamb_profile(&view.id).await.ok().flatten();
                agents.push(agent_json(view, profile.as_ref()));
            }
            connect_ok(json!({ "agents": agents }))
        }

        "CreateGrokBotAgent" => {
            let name = args.get("name").and_then(Value::as_str).unwrap_or("Grok");
            // A caller-minted id is honoured (the client sends one); otherwise we mint.
            let id = args
                .get("agentId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(|id| CoworkerId::from_stored(id.to_string()))
                .unwrap_or_else(CoworkerId::new);
            let at_ms = now_ms();
            let mut events = match Coworker::default().decide(CoworkerCommand::Hire {
                name: name.to_string(),
                model: state.agui.model.clone(),
                at_ms,
            }) {
                Ok(events) => events,
                Err(reason) => {
                    return connect_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_argument",
                        &reason.to_string(),
                    );
                }
            };
            let mut coworker = Coworker::default();
            for event in &events {
                coworker.apply(event);
            }

            // 1 account = 1 computer: first agent creates it, later agents share it.
            let provisioned = crate::agui::provision::ensure_account_computer(
                &state.agui,
                &account_id,
                &mut coworker,
                at_ms,
            )
            .await;
            events.extend(provisioned.events);

            let view = opengrok_core::coworker::CoworkerView {
                id: id.clone(),
                name: coworker.name.clone(),
                model: coworker.model.clone(),
                box_id: coworker.computer().cloned(),
                retired: false,
                updated_at_ms: at_ms,
            };
            if let Err(error) = store
                .append_coworker(&id, &account_id, 0, &events, &view)
                .await
            {
                tracing::error!(%error, "could not hire over seam B");
                return connect_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", "hire failed");
            }

            // Grant the builtin tools so a boxed coworker can actually use its computer.
            let tools = opengrok_policy::ToolSet::only(
                opengrok_tools::Executor::builtin_tool_names().to_vec(),
            );
            if let Err(error) = store
                .grant_access(
                    &account_id,
                    &id,
                    &tools,
                    &tools,
                    &opengrok_policy::ToolSet::None,
                    at_ms,
                )
                .await
            {
                tracing::error!(%error, "could not grant access over seam B");
                return connect_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "the coworker was created but could not be granted",
                );
            }

            let profile = json!({
                "description": args.get("description").and_then(Value::as_str).unwrap_or(""),
                "title": args.get("title").and_then(Value::as_str).unwrap_or(""),
                "avatarShape": args.get("avatarShape").and_then(Value::as_str).unwrap_or(""),
                "avatarColor": args.get("avatarColor").and_then(Value::as_str).unwrap_or(""),
                "role": args.get("role").and_then(Value::as_str).unwrap_or(""),
            });
            let _ = store.put_seamb_profile(&id, &profile, now_ms()).await;
            let mut reply = json!({
                "agent": agent_json(&view, Some(&profile)),
                "harness": "GROK_BOT_AGENT_HARNESS_KIND_BOX",
            });
            if provisioned.error.is_some()
                && let Some(object) = reply.as_object_mut()
            {
                object.insert(
                    "computerError".to_string(),
                    crate::agui::provision::error_json(&provisioned.error),
                );
            }
            connect_ok(reply)
        }

        "UpdateGrokBotAgent" => {
            let Some(id) = args.get("id").and_then(Value::as_str) else {
                return connect_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_argument",
                    "id is required",
                );
            };
            let coworker_id = CoworkerId::from_stored(id.to_string());
            let Ok((loaded, seq)) = store.load_coworker(&coworker_id).await else {
                return connect_error(StatusCode::NOT_FOUND, "not_found", "no such agent");
            };
            let name = args
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(&loaded.name)
                .to_string();
            if name != loaded.name
                && let Ok(events) = loaded.decide(CoworkerCommand::Rename {
                    name: name.clone(),
                    at_ms: now_ms(),
                })
            {
                let mut after = loaded.clone();
                for event in &events {
                    after.apply(event);
                }
                let view = opengrok_core::coworker::CoworkerView {
                    id: coworker_id.clone(),
                    name: after.name.clone(),
                    model: after.model.clone(),
                    box_id: after.box_id.clone(),
                    retired: after.retired,
                    updated_at_ms: now_ms(),
                };
                let _ = store
                    .append_coworker(&coworker_id, &account_id, seq, &events, &view)
                    .await;
            }
            let mut profile = store
                .seamb_profile(&coworker_id)
                .await
                .ok()
                .flatten()
                .unwrap_or_else(|| json!({}));
            if let Some(map) = profile.as_object_mut() {
                for key in ["description", "title", "avatarShape", "avatarColor"] {
                    if let Some(value) = args.get(key).and_then(Value::as_str) {
                        map.insert(key.to_string(), json!(value));
                    }
                }
            }
            let _ = store
                .put_seamb_profile(&coworker_id, &profile, now_ms())
                .await;
            let Ok(coworkers) = store.coworkers_for(&account_id).await else {
                return connect_ok(json!({}));
            };
            let Some(view) = coworkers.into_iter().find(|view| view.id == coworker_id) else {
                return connect_error(StatusCode::NOT_FOUND, "not_found", "no such agent");
            };
            connect_ok(json!({ "agent": agent_json(&view, Some(&profile)) }))
        }

        "DeleteGrokBotAgent" => {
            let Some(id) = args.get("id").and_then(Value::as_str) else {
                return connect_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_argument",
                    "id is required",
                );
            };
            let coworker_id = CoworkerId::from_stored(id.to_string());
            if let Ok((loaded, seq)) = store.load_coworker(&coworker_id).await {
                let at_ms = now_ms();
                let mut after = loaded;
                // Retire the agent; its box is the account's shared computer, torn down below only
                // when the account's last agent is gone.
                if let Ok(events) = after.decide(CoworkerCommand::Retire { at_ms }) {
                    for event in &events {
                        after.apply(event);
                    }
                    let view = opengrok_core::coworker::CoworkerView {
                        id: coworker_id.clone(),
                        name: after.name.clone(),
                        model: after.model.clone(),
                        box_id: after.box_id.clone(),
                        retired: after.retired,
                        updated_at_ms: at_ms,
                    };
                    let _ = store
                        .append_coworker(&coworker_id, &account_id, seq, &events, &view)
                        .await;
                }
            }
            crate::agui::provision::teardown_account_computer_if_last(&state.agui, &account_id)
                .await;
            connect_ok(json!({}))
        }

        "ListGrokBotTranscriptEntries" => {
            let Some(agent) = args.get("agentId").and_then(Value::as_str) else {
                return connect_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_argument",
                    "agentId is required",
                );
            };
            let coworker = CoworkerId::from_stored(agent.to_string());
            let before = as_ms(args.get("beforeSeq"));
            let limit = args
                .get("limit")
                .and_then(Value::as_i64)
                .filter(|limit| *limit > 0)
                .unwrap_or(200)
                .clamp(1, 1000);
            match store.gateway_page(&coworker, before, limit).await {
                Ok(page) => connect_ok(json!({
                    "entries": page
                        .iter()
                        .map(|(seq, entry)| transcript_entry_json(*seq, entry))
                        .collect::<Vec<_>>(),
                    "generation": 1,
                })),
                Err(error) => {
                    tracing::error!(%error, "seam B transcript read failed");
                    connect_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "internal",
                        "transcript unavailable",
                    )
                }
            }
        }

        "CommitGrokBotTranscriptEntries" => {
            let Some(agent) = args.get("agentId").and_then(Value::as_str) else {
                return connect_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_argument",
                    "agentId is required",
                );
            };
            let coworker = CoworkerId::from_stored(agent.to_string());
            let mut committed = 0;
            for entry in args
                .get("entries")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
            {
                let Some(body) = entry.get("body").and_then(Value::as_str) else {
                    continue;
                };
                let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(body) else {
                    continue;
                };
                let Ok(parsed) = serde_json::from_slice::<Value>(&bytes) else {
                    continue;
                };
                if store
                    .append_gateway_entry(&coworker, &parsed, now_ms())
                    .await
                    .is_ok()
                {
                    committed += 1;
                }
            }
            connect_ok(json!({ "committedCount": committed, "deletedCount": 0 }))
        }

        "SendGrokBotUserMessage" => crate::seamb_send::send(&state, &account_id, &args).await,

        "GetGrokBotSendStatus" => {
            let agent = args
                .get("agentId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let message = args
                .get("messageId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let slot = format!("seamb:{}", account_id.as_str());
            match store
                .nonce_record(&slot, &format!("{agent}:{message}"))
                .await
            {
                Ok(Some(record)) => connect_ok(json!({
                    "status": "GROK_BOT_SEND_STATUS_ACCEPTED",
                    "echoEntryId": record.get("echoEntryId").and_then(Value::as_str).unwrap_or(""),
                    "acceptedAtMs": as_ms(record.get("acceptedAtMs")).unwrap_or(0).to_string(),
                })),
                Ok(None) => connect_ok(json!({ "status": "GROK_BOT_SEND_STATUS_NOT_FOUND" })),
                Err(_) => connect_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "ledger unavailable",
                ),
            }
        }

        "ListGrokBotUserComputers" => {
            // Advertise the computer OPTIONS this caller can put a bot on. Local VM (a VM on the
            // server host) needs no credential and is always offered; box.ascii.dev is offered as
            // configured only when this server actually has an ascii provider (per-org credentials
            // land next); Windows 365 is a placeholder until it is built. The client renders a
            // `configured:false` option greyed with "set up by your org admin" rather than hiding it.
            // box.ascii.dev shows configured when THIS caller's org has set a key on the admin
            // dashboard. Resolve the caller's org, then check its configured kinds.
            let ascii_ready = match store.load_account(&account_id).await {
                Ok((account, _)) => match account.org_id {
                    Some(org) => store
                        .org_computer_kinds(&org)
                        .await
                        .map(|kinds| kinds.iter().any(|kind| kind == "ascii"))
                        .unwrap_or(false),
                    None => false,
                },
                Err(_) => false,
            };
            let mut computers = Vec::new();
            // Local VM only where it is allowed (self-host / dev) — a hosted deploy hides it.
            if crate::agui::provision::local_docker_allowed() {
                computers.push(json!({
                    "id": "local-docker",
                    "label": "Local VM (on the server)",
                    "kind": "local-docker",
                    "state": "available",
                    "configured": true,
                }));
            }
            computers.push(json!({
                "id": "ascii",
                "label": "box.ascii.dev",
                "kind": "ascii",
                "state": if ascii_ready { "available" } else { "not-configured" },
                "configured": ascii_ready,
            }));
            computers.push(json!({
                "id": "windows365",
                "label": "Windows 365",
                "kind": "windows365",
                "state": "not-configured",
                "configured": false,
            }));
            let account_error = store
                .account_computer_error(account_id.as_str())
                .await
                .ok()
                .flatten();
            connect_ok(json!({
                "computers": computers,
                "computerError": crate::agui::provision::error_json(&account_error),
            }))
        }

        "SetGrokBotAgentClientState" => {
            let agent = args
                .get("agentId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            connect_ok(json!({
                "state": {
                    "agentId": agent,
                    "unreadCount": 0,
                    "notificationsEnabled": false,
                    "notifyOnUpdatesEnabled": true,
                    "hiddenFromSidebar": false,
                    "updatedAtMs": now_ms().to_string(),
                }
            }))
        }

        "ReadGrokBotAgentAttachmentChunk" => connect_ok(json!({
            "data": "",
            "totalSize": "0",
        })),

        // P1 — THE MINT, where the two seams meet: the backend hands the client the address and
        // bearer of a gateway. Ours. The address must not be loopback (the client refuses it,
        // `local-docker-host-connector.ts:465`), so it comes from OG_PUBLIC_GATEWAY_URL.
        "EnsureSandBox" => {
            let gateway_url = state.public_gateway_url.clone().unwrap_or_default();
            if gateway_url.is_empty() {
                return connect_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "failed_precondition",
                    "OG_PUBLIC_GATEWAY_URL is not configured; the mint has no address to hand out",
                );
            }
            connect_ok(json!({
                "cluster": "opengrok",
                "tenantId": account_id.as_str(),
                "podId": "opengrok-1",
                "networkToken": "",
                "execDaemonAuthToken": "",
                "execDaemonUrl": "",
                "vncUrl": "",
                "terminalsFolder": "",
                "forkVncBaseUrl": "",
                "gatewayUrl": gateway_url,
                "gatewayToken": state.bearer.clone().unwrap_or_default(),
            }))
        }

        other => {
            // The mock's default impl: an empty message for every unmodelled method, and the app
            // boots because of it.
            tracing::debug!(method = other, "GrokBotService default empty reply");
            connect_ok(json!({}))
        }
    }
}
