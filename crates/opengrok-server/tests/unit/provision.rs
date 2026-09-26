use super::*;

#[test]
fn a_group_is_its_own_scope_whatever_the_sharing_mode() {
    for mode in ["per-org", "per-account", "per-bot"] {
        let (scope, id, box_mode) = scope_for(mode, "acct_1", Some("org_1"), "cw_room", true);
        assert_eq!(
            (scope, id.as_str(), box_mode),
            ("group", "cw_room", BoxMode::Shared),
            "{mode}"
        );
    }
}

#[test]
fn a_plain_coworker_follows_the_mode() {
    assert_eq!(
        scope_for("per-bot", "acct_1", Some("org_1"), "cw_1", false),
        ("bot", "cw_1".to_string(), BoxMode::Dedicated)
    );
    assert_eq!(
        scope_for("per-account", "acct_1", Some("org_1"), "cw_1", false),
        ("account", "acct_1".to_string(), BoxMode::Shared)
    );
    assert_eq!(
        scope_for("per-org", "acct_1", Some("org_1"), "cw_1", false),
        ("org", "org_1".to_string(), BoxMode::Shared)
    );
    // No org to share: the member's own box stands in.
    assert_eq!(
        scope_for("per-org", "acct_1", None, "cw_1", false),
        ("account", "acct_1".to_string(), BoxMode::Shared)
    );
}

#[test]
fn computer_json_stamps_live_egress_when_the_guest_is_ready() {
    let mut screen = json!({ "boxId": "bx_live" });
    stamp_egress_fields(
        &mut screen,
        true,
        Some(EgressTunnel {
            enabled: true,
            ready: true,
        }),
    );
    assert_eq!(screen["isEgressTunnelAvailable"], true);
    assert_eq!(screen["egress_tunnel"]["enabled"], true);
    assert_eq!(screen["egress_tunnel"]["ready"], true);
}

#[test]
fn computer_json_available_is_false_when_the_guest_is_not_ready() {
    let mut screen = json!({ "boxId": "bx_live" });
    stamp_egress_fields(
        &mut screen,
        true,
        Some(EgressTunnel {
            enabled: true,
            ready: false,
        }),
    );
    assert_eq!(screen["isEgressTunnelAvailable"], false);
    assert_eq!(screen["egress_tunnel"]["ready"], false);
    assert_eq!(screen["egress_tunnel"]["enabled"], true);
}

#[test]
fn computer_json_does_not_invent_ready_when_info_is_missing() {
    let mut screen = json!({ "boxId": "bx_live" });
    stamp_egress_fields(&mut screen, true, None);
    assert_eq!(screen["isEgressTunnelAvailable"], false);
    assert!(
        screen.get("egress_tunnel").is_none(),
        "missing /v1/info must not invent a nested capability: {screen}"
    );
}

#[test]
fn share_scope_maps_internal_scope_to_client_placement() {
    assert_eq!(share_scope_of("bot"), "dedicated");
    assert_eq!(share_scope_of("account"), "user");
    assert_eq!(share_scope_of("group"), "group");
    assert_eq!(share_scope_of("org"), "org");
    let (scope, id, _) = scope_for("per-bot", "acct_1", Some("org_1"), "cw_1", false);
    let mut screen = json!({ "boxId": "bx_live" });
    stamp_share_scope(&mut screen, scope, &id);
    assert_eq!(screen["shareScope"], "dedicated");
    assert!(screen.get("groupId").is_none(), "{screen}");

    let (scope, id, _) = scope_for("per-account", "acct_1", Some("org_1"), "cw_1", false);
    let mut screen = json!({ "boxId": "bx_live" });
    stamp_share_scope(&mut screen, scope, &id);
    assert_eq!(screen["shareScope"], "user");
    assert!(screen.get("groupId").is_none(), "{screen}");

    let (scope, id, _) = scope_for("per-org", "acct_1", Some("org_1"), "cw_1", false);
    let mut screen = json!({ "boxId": "bx_live" });
    stamp_share_scope(&mut screen, scope, &id);
    assert_eq!(screen["shareScope"], "org");
    assert!(screen.get("groupId").is_none(), "{screen}");

    let (scope, id, _) = scope_for("per-org", "acct_1", None, "cw_1", false);
    let mut screen = json!({ "boxId": "bx_live" });
    stamp_share_scope(&mut screen, scope, &id);
    assert_eq!(
        screen["shareScope"], "user",
        "per-org with no org falls back to account/user, not org: {screen}"
    );

    let (scope, id, _) = scope_for("per-account", "acct_1", Some("org_1"), "cw_room", true);
    let mut screen = json!({ "boxId": "bx_live" });
    stamp_share_scope(&mut screen, scope, &id);
    assert_eq!(screen["shareScope"], "group");
    assert_eq!(screen["groupId"], "cw_room");
}

#[test]
fn absent_computer_json_omits_share_scope_and_does_not_fake_ready() {
    let mut absent = json!({
        "agentId": "cw_1",
        "state": "absent",
        "vncUrl": Value::Null,
    });
    stamp_egress_fields(&mut absent, true, None);
    assert_eq!(absent["isEgressTunnelAvailable"], false);
    assert!(absent.get("egress_tunnel").is_none(), "{absent}");
    assert!(
        absent.get("shareScope").is_none(),
        "unprovisioned omits shareScope; client hides the control: {absent}"
    );
    assert!(absent.get("groupId").is_none(), "{absent}");
}

#[test]
fn share_scope_stamp_leaves_egress_fields_in_place() {
    let mut screen = json!({ "boxId": "bx_live" });
    stamp_egress_fields(
        &mut screen,
        true,
        Some(EgressTunnel {
            enabled: true,
            ready: true,
        }),
    );
    stamp_share_scope(&mut screen, "account", "acct_1");
    assert_eq!(screen["isEgressTunnelAvailable"], true);
    assert_eq!(screen["egress_tunnel"]["ready"], true);
    assert_eq!(screen["shareScope"], "user");
}
