//! Box facts: egress tunnel readiness, the scoped box stamp, a status that waits on nobody.

use super::*;

#[tokio::test]
async fn egress_tunnel_needs_box_ready_when_info_is_present() {
    let database_url = database_or_skip!();
    let email = format!(
        "user-form-egress-box-{}@og.local",
        uuid::Uuid::now_v7().simple()
    );
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "egress-box").await;
    let named = format!("?coworker={agent}");

    let record = h
        .patch_host_settings(&token, json!({ "egressTunnelEnabled": true }))
        .await;
    assert_eq!(record["egressTunnelEnabled"], true, "{record}");
    let record = h.host_settings(&token, &named).await;
    assert_eq!(
        record["egressTunnelAvailable"], false,
        "a box that cannot report /v1/info is not available: {record}"
    );

    h.stub.set_egress(Some(EgressTunnel {
        enabled: true,
        ready: false,
    }));
    let record = h.host_settings(&token, &named).await;
    assert_eq!(
        record["egressTunnelAvailable"], false,
        "host wants the tunnel but no laptop client is attached: {record}"
    );

    h.stub.set_egress(Some(EgressTunnel {
        enabled: true,
        ready: true,
    }));
    let record = h.host_settings(&token, &named).await;
    assert_eq!(
        record["egressTunnelAvailable"], true,
        "ready box + host setting: {record}"
    );

    h.patch_host_settings(&token, json!({ "egressTunnelEnabled": false }))
        .await;
    let record = h.host_settings(&token, &named).await;
    assert_eq!(
        record["egressTunnelAvailable"], false,
        "a ready box does not override a host that opted out: {record}"
    );
}

/// NativeChat 729bdd9 reads GET `/coworkers/{id}/computer` only — not Box
/// `/v1/info` and not the gateway verb. Live ready must be on this JSON.
#[tokio::test]
async fn computer_json_stamps_the_scoped_box_live_egress() {
    let database_url = database_or_skip!();
    let email = format!(
        "user-form-computer-egress-{}@og.local",
        uuid::Uuid::now_v7().simple()
    );
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "computer-egress").await;

    let (status, body) = h.computer_json(&token, &agent).await;
    assert_eq!(status, 200, "{body}");
    let box_id = body["boxId"].as_str().expect("scoped boxId").to_string();
    assert_eq!(
        h.stub.last_egress_box().as_deref(),
        Some(box_id.as_str()),
        "probe the payload's live boxId, not a frozen coworker row: {body}"
    );
    assert_eq!(
        body["isEgressTunnelAvailable"], false,
        "missing /v1/info is not available: {body}"
    );
    assert!(
        body.get("egress_tunnel").is_none(),
        "do not invent nested ready: {body}"
    );
    assert_eq!(
        body["shareScope"], "user",
        "default per-account share places chrome in Settings→Computer: {body}"
    );
    assert!(
        body.get("groupId").is_none(),
        "user share has no groupId: {body}"
    );

    h.patch_host_settings(&token, json!({ "egressTunnelEnabled": true }))
        .await;

    h.stub.set_egress(Some(EgressTunnel {
        enabled: true,
        ready: false,
    }));
    let (status, body) = h.computer_json(&token, &agent).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["isEgressTunnelAvailable"], false, "{body}");
    assert_eq!(body["egress_tunnel"]["enabled"], true, "{body}");
    assert_eq!(body["egress_tunnel"]["ready"], false, "{body}");

    h.stub.set_egress(Some(EgressTunnel {
        enabled: true,
        ready: true,
    }));
    let (status, body) = h.computer_json(&token, &agent).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["isEgressTunnelAvailable"], true, "{body}");
    assert_eq!(body["egress_tunnel"]["enabled"], true, "{body}");
    assert_eq!(body["egress_tunnel"]["ready"], true, "{body}");
    assert_eq!(
        h.stub.last_egress_box().as_deref(),
        Some(box_id.as_str()),
        "{body}"
    );
    assert_eq!(body["shareScope"], "user", "{body}");
}

/// The pane polls `/computer`, `/screen` and the host settings every two seconds, and a guest
/// that spent the probe's whole patience saying nothing cost every one of them that wait (3 Oct
/// 2026). Once it has, a status within `SILENT_GUEST_FOR` does not ask it again, and still says
/// the tunnel is not there; a turn's own probe is not this one.
#[tokio::test]
async fn a_status_does_not_wait_on_a_guest_that_said_nothing() {
    let database_url = database_or_skip!();
    let email = format!(
        "user-form-silent-guest-{}@og.local",
        uuid::Uuid::now_v7().simple()
    );
    let h = harness(&database_url, &email).await;
    let token = h.access_token(&email);
    let agent = h.hire(&token, "silent-guest").await;
    h.patch_host_settings(&token, json!({ "egressTunnelEnabled": true }))
        .await;
    *h.stub.egress_takes.lock().expect("takes") = opengrok_box::GUEST_INFO_PATIENCE;

    let (status, body) = h.computer_json(&token, &agent).await;
    assert_eq!(status, 200, "{body}");
    let asked = *h.stub.egress_asked.lock().expect("asked");
    let began = std::time::Instant::now();
    for _ in 0..3 {
        let (status, body) = h.computer_json(&token, &agent).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["isEgressTunnelAvailable"], false, "{body}");
        assert!(body.get("egress_tunnel").is_none(), "{body}");
        let record = h.host_settings(&token, &format!("?coworker={agent}")).await;
        assert_eq!(record["egressTunnelAvailable"], false, "{record}");
    }
    let waited = began.elapsed();
    assert_eq!(*h.stub.egress_asked.lock().expect("asked"), asked);
    assert!(
        waited < 3 * opengrok_box::GUEST_INFO_PATIENCE,
        "six reads took {waited:?}: they waited on the silent guest"
    );
}
