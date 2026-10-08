use super::*;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde_json::{Value, json};

impl GatewayAdmin {
    /// The method OAG does not serve. GET of the collection is 405 by design; this exists so
    /// the contract test can prove that 405 is `Refused`, never `Unreachable`.
    async fn get_principals_collection(&self) -> Result<serde_json::Value, AdminError> {
        self.send(reqwest::Method::GET, "/admin/api/principals", None)
            .await
    }
}

#[derive(Default)]
struct PrincipalLog {
    /// `(method, path, body)` for every principals call. GET of the collection must stay empty.
    calls: Vec<(String, String, Option<Value>)>,
}

type SharedLog = Arc<Mutex<PrincipalLog>>;

/// OAG's principals surface: no GET list (405), POST upsert, PATCH budget, GET usage by email.
async fn spawn_oag_principals(log: SharedLog) -> String {
    let app = Router::new()
        .route(
            "/admin/api/principals",
            post(
                |State(log): State<SharedLog>, Json(body): Json<Value>| async move {
                    log.lock().unwrap().calls.push((
                        "POST".into(),
                        "/admin/api/principals".into(),
                        Some(body.clone()),
                    ));
                    (StatusCode::OK, Json(body))
                },
            )
            .get(|State(log): State<SharedLog>| async move {
                log.lock().unwrap().calls.push((
                    "GET".into(),
                    "/admin/api/principals".into(),
                    None,
                ));
                (
                    StatusCode::METHOD_NOT_ALLOWED,
                    Json(json!({"error": "method not allowed"})),
                )
            }),
        )
        .route(
            "/admin/api/principals/{email}/budget",
            patch(
                |State(log): State<SharedLog>,
                 Path(email): Path<String>,
                 Json(body): Json<Value>| async move {
                    log.lock().unwrap().calls.push((
                        "PATCH".into(),
                        format!("/admin/api/principals/{email}/budget"),
                        Some(body.clone()),
                    ));
                    (StatusCode::OK, Json(json!({ "email": email })))
                },
            ),
        )
        .route(
            "/admin/api/principals/{email}/usage",
            get(
                |State(log): State<SharedLog>, Path(email): Path<String>| async move {
                    log.lock().unwrap().calls.push((
                        "GET".into(),
                        format!("/admin/api/principals/{email}/usage"),
                        None,
                    ));
                    (
                        StatusCode::OK,
                        Json(json!({
                            "email": email,
                            "monthly_budget_usd": "100.000000",
                            "month_to_date_usd": "1.250000",
                            "requests": 3,
                        })),
                    )
                },
            ),
        )
        .with_state(log);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://127.0.0.1:{}", addr.port())
}

#[test]
fn the_org_principal_address_is_derived_not_stored() {
    let email = GatewayAdmin::org_principal_email("org_01a05");
    assert_eq!(email, "org-org_01a05@gateway.local");
    // Deterministic: the same org resolves to the same principal across restarts, which is why
    // we keep no gateway ids of our own.
    assert_eq!(email, GatewayAdmin::org_principal_email("org_01a05"));
    assert_ne!(email, GatewayAdmin::org_principal_email("org_other"));
}

#[test]
fn debug_never_prints_the_admin_token() {
    let admin = GatewayAdmin {
        base_url: "http://gateway.local:29081".to_string(),
        token: "oag_live_supersecret".to_string(),
        http: reqwest::Client::new(),
    };
    let rendered = format!("{admin:?}");
    assert!(!rendered.contains("supersecret"), "{rendered}");
    assert!(rendered.contains("«redacted»"), "{rendered}");
}

#[test]
fn the_principal_email_is_encoded_in_the_path() {
    assert_eq!(
        encode_path_segment("org-org_01a05@gateway.local"),
        "org-org_01a05%40gateway.local"
    );
}

#[test]
fn a_405_on_the_principals_collection_is_refused_not_unreachable() {
    let error = classify_admin_reply(
        "/admin/api/principals",
        reqwest::StatusCode::METHOD_NOT_ALLOWED,
        r#"{"error":"method not allowed"}"#,
    )
    .expect_err("405 is not success");
    match error {
        AdminError::Refused(detail) => {
            assert!(detail.contains("POST /admin/api/principals"), "{detail}");
            assert!(!detail.to_lowercase().contains("unreachable"), "{detail}");
        }
        AdminError::Unreachable(detail) => {
            panic!("405 must not mean unreachable: {detail}");
        }
    }
    let rendered = format!(
        "{}",
        classify_admin_reply(
            "/admin/api/principals",
            reqwest::StatusCode::METHOD_NOT_ALLOWED,
            "",
        )
        .expect_err("empty 405")
    );
    assert!(rendered.starts_with("the gateway refused:"), "{rendered}");
    assert!(
        !rendered.contains("the gateway is unreachable"),
        "{rendered}"
    );
}

#[tokio::test]
async fn ensure_org_principal_posts_upsert_and_never_gets_the_collection() {
    let log: SharedLog = Arc::new(Mutex::new(PrincipalLog::default()));
    let base = spawn_oag_principals(log.clone()).await;
    let admin = GatewayAdmin::new(&base, "admin-token");

    admin
        .ensure_org_principal("org_01a05", None)
        .await
        .expect("upsert");

    let calls = log.lock().unwrap().calls.clone();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].0, "POST");
    assert_eq!(calls[0].1, "/admin/api/principals");
    let body = calls[0].2.as_ref().expect("json body");
    assert_eq!(body["email"], json!("org-org_01a05@gateway.local"));
    assert_eq!(body["role"], json!("member"));
    assert!(
        body.get("monthly_budget_usd").is_none(),
        "omit the budget to leave it (COALESCE): {body}"
    );
    assert!(
        calls.iter().all(|(method, path, _)| {
            !(method == "GET" && path.trim_end_matches('/') == "/admin/api/principals")
        }),
        "never GET the collection: {calls:?}"
    );
}

#[tokio::test]
async fn ensure_org_principal_sends_the_budget_when_set() {
    let log: SharedLog = Arc::new(Mutex::new(PrincipalLog::default()));
    let base = spawn_oag_principals(log.clone()).await;
    let admin = GatewayAdmin::new(&base, "admin-token");

    admin
        .ensure_org_principal("org_01a05", Some("100"))
        .await
        .expect("upsert");

    let body = log.lock().unwrap().calls[0].2.clone().expect("body");
    assert_eq!(body["monthly_budget_usd"], json!("100"));
    assert_eq!(body["role"], json!("member"));
}

#[tokio::test]
async fn org_usage_and_budget_are_by_email_not_the_collection() {
    let log: SharedLog = Arc::new(Mutex::new(PrincipalLog::default()));
    let base = spawn_oag_principals(log.clone()).await;
    let admin = GatewayAdmin::new(&base, "admin-token");

    let usage = admin.org_usage("org_01a05").await.expect("usage");
    let usage = usage.expect("provisioned");
    assert_eq!(usage.email, "org-org_01a05@gateway.local");
    assert_eq!(usage.month_to_date_usd, "1.250000");

    admin
        .set_org_budget("org_01a05", Some("50"))
        .await
        .expect("budget");

    let calls = log.lock().unwrap().calls.clone();
    assert_eq!(calls[0].0, "GET");
    assert_eq!(
        calls[0].1,
        "/admin/api/principals/org-org_01a05@gateway.local/usage"
    );
    assert_eq!(calls[1].0, "PATCH");
    assert_eq!(
        calls[1].1,
        "/admin/api/principals/org-org_01a05@gateway.local/budget"
    );
    assert!(
        calls.iter().all(|(method, path, _)| {
            !(method == "GET" && path.trim_end_matches('/') == "/admin/api/principals")
        }),
        "never GET the collection: {calls:?}"
    );
}

#[tokio::test]
async fn a_get_of_the_principals_collection_is_refused_not_unreachable() {
    let log: SharedLog = Arc::new(Mutex::new(PrincipalLog::default()));
    let base = spawn_oag_principals(log.clone()).await;
    let admin = GatewayAdmin::new(&base, "admin-token");

    let error = admin
        .get_principals_collection()
        .await
        .expect_err("GET collection is 405");
    let rendered = format!("{error}");
    match error {
        AdminError::Refused(detail) => {
            assert!(detail.contains("POST /admin/api/principals"), "{detail}");
        }
        AdminError::Unreachable(detail) => {
            panic!("405 must not mean unreachable: {detail}");
        }
    }
    assert!(rendered.starts_with("the gateway refused:"), "{rendered}");
    assert!(
        !rendered.contains("the gateway is unreachable"),
        "{rendered}"
    );
    assert_eq!(log.lock().unwrap().calls[0].0, "GET");
}

#[tokio::test]
async fn a_closed_port_is_unreachable() {
    let admin = GatewayAdmin::new("http://127.0.0.1:1", "admin-token");
    let error = admin
        .ensure_org_principal("org_01a05", None)
        .await
        .expect_err("nothing listens on :1");
    assert!(matches!(error, AdminError::Unreachable(_)), "{error:?}");
    let rendered = format!("{error}");
    assert!(
        rendered.starts_with("the gateway is unreachable:"),
        "{rendered}"
    );
}
