use super::*;

fn boilerplate(summary: &str) -> bool {
    summary.starts_with("Run \"")
        || summary.starts_with("Use ")
        || summary.contains("Run a command on your local computer")
        || summary.contains("the agent's VM")
}

#[test]
fn surfaces_by_tool() {
    assert_eq!(surface_for(USER_MACHINE_SHELL), "host_shell");
    assert_eq!(surface_for("shell"), "box_shell");
    assert_eq!(surface_for("read_file"), "box_shell");
    assert_eq!(surface_for("gmail.api.send"), "mcp");
    assert_eq!(surface_for("something_else"), "computer");
}

#[test]
fn command_only_for_the_shells_and_summaries_never_boilerplate() {
    let args = json!({ "command": "brew install jq", "path": "/etc/hosts", "content": "abc" });
    for tool in [USER_MACHINE_SHELL, "shell"] {
        assert_eq!(command_for(tool, &args).as_deref(), Some("brew install jq"));
    }
    for tool in [
        "read_file",
        "write_file",
        "gmail.api.send",
        "computer",
        "open_url",
    ] {
        assert_eq!(command_for(tool, &args), None);
    }
    for tool in [
        USER_MACHINE_SHELL,
        "shell",
        "read_file",
        "write_file",
        "gmail.api.send",
        "computer",
        "open_url",
        opengrok_tools::RUN_RECIPE,
    ] {
        let summary = summary_for(tool, &args);
        assert!(!summary.is_empty());
        assert!(!boilerplate(&summary), "{tool}: {summary}");
    }
    assert!(summary_for("write_file", &args).contains("3 bytes to /etc/hosts"));
}

/// #165: a screen action read as "a plugin tool … with {raw JSON}". The issue's own
/// arguments first: every field set, only `action` meaning anything.
#[test]
fn computer_and_open_url_summaries_name_the_action() {
    let shot = json!({"to": [0, 0], "key": "", "text": "", "action": "screenshot",
                      "button": 1, "scroll": [0, 0], "coordinate": [0, 0]});
    let click = json!({"action": "click", "coordinate": [120, 40]});
    let key = json!({"action": "key", "key": "ctrl+l"});
    let typed = json!({"action": "type", "text": "x".repeat(300)});
    let page = json!({"url": "https://example.com/inbox?token=s3cret#top"});
    let recipe = json!({"recipe": "Open Gmail", "values": {"password": "s3cret"}});
    let odd = json!({"action": "zoom", "coordinate": [1, 2]});
    let cases = [
        ("computer", &shot, "screenshot"),
        ("computer", &click, "120, 40"),
        ("computer", &key, "ctrl+l"),
        ("computer", &typed, "xxxx"),
        ("computer", &odd, "zoom"),
        ("open_url", &page, "https://example.com/inbox"),
        (opengrok_tools::RUN_RECIPE, &recipe, "Open Gmail"),
    ];
    for (tool, args, names) in cases {
        let summary = summary_for(tool, args);
        assert!(summary.contains(names), "{tool}: {summary}");
        assert!(!summary.contains("plugin tool"), "{tool}: {summary}");
        assert!(!summary.contains("coordinate"), "{tool}: {summary}");
        assert!(!summary.contains("s3cret"), "{tool}: {summary}");
        assert!(!boilerplate(&summary), "{tool}: {summary}");
        assert!(summary.chars().count() < 160, "{tool}: {summary}");
        let rule = proposed_rule(tool, args);
        assert_ne!(rule, format!("Allow the {tool} tool."));
        assert!(!rule.contains("s3cret"), "{tool}: {rule}");
    }
    assert!(!summary_for("computer", &shot).contains('{'));
    let group = json!({"action": "screenshot", "machine": "group"});
    assert!(summary_for("computer", &group).contains("shared"));
}

/// The tunnel's card asks about the network, not about the judge's instructions. Its
/// "Always allow" wrote "Allow the computer tool." into the allow text, which switched the
/// judge on for every later call of that coworker. With no rule the client's Always is a
/// plain approve (`transcript-card/auto-review-actions.ts:149-150`); the standing answer
/// is the computer's egress policy.
#[test]
fn a_tunnel_card_offers_no_rule_it_cannot_honour() {
    let args = json!({"action": "click", "coordinate": [120, 40]});
    let card = auto_review_card(
        "e_t",
        "call_t",
        "pending",
        "computer",
        &args,
        Some(opengrok_tools::review::EGRESS_TUNNEL_ASK_REASON),
        9,
    );
    let approval = &card["message"]["approval"];
    assert!(approval.get("proposedRule").is_none(), "{approval}");
    assert_eq!(
        approval["reason"],
        opengrok_tools::review::EGRESS_TUNNEL_ASK_REASON
    );
    assert_eq!(approval["surface"], "computer");
    let judged = auto_review_card(
        "e_j",
        "call_j",
        "pending",
        "computer",
        &args,
        Some(opengrok_tools::review::REVIEW_ASK_REASON),
        9,
    );
    assert!(
        judged["message"]["approval"]["proposedRule"]
            .as_str()
            .is_some_and(|rule| rule.contains("click")),
        "{judged}"
    );
}

#[test]
fn the_card_carries_the_transcribed_shape_and_settles_on_the_same_id() {
    let args = json!({ "command": "brew install jq" });
    let pending = auto_review_card(
        "e_1",
        "call_1",
        "pending",
        USER_MACHINE_SHELL,
        &args,
        Some("why"),
        7,
    );
    assert_eq!(pending["kind"], "send-message");
    assert_eq!(pending["id"], "e_1");
    assert_eq!(pending["message"]["type"], "auto-review-approval");
    let approval = &pending["message"]["approval"];
    assert_eq!(approval["requestId"], "call_1");
    assert_eq!(approval["status"], "pending");
    assert_eq!(approval["surface"], "host_shell");
    assert_eq!(approval["command"], "brew install jq");
    assert_eq!(approval["reason"], "why");
    assert!(approval["summary"].as_str().is_some_and(|s| !s.is_empty()));
    assert_eq!(
        approval["proposedRule"],
        "Allow `brew install jq` on my own computer."
    );

    let settled = auto_review_card(
        "e_1",
        "call_1",
        "approved",
        USER_MACHINE_SHELL,
        &args,
        Some("why"),
        8,
    );
    assert_eq!(settled["id"], pending["id"]);
    assert_eq!(settled["message"]["approval"]["status"], "approved");
}

#[test]
fn a_policy_ask_is_the_same_card_with_the_grants_reason_and_no_rule() {
    let args = json!({ "command": "rm -rf build" });
    let card = policy_approval_card(
        "e_2",
        "call_2",
        "pending",
        "shell",
        &args,
        Some("only the on-call may clean builds"),
        9,
    );
    assert_eq!(card["message"]["type"], "auto-review-approval");
    let approval = &card["message"]["approval"];
    assert_eq!(approval["requestId"], "call_2");
    assert_eq!(approval["surface"], "box_shell");
    assert_eq!(approval["command"], "rm -rf build");
    assert_eq!(approval["reason"], "only the on-call may clean builds");
    assert!(
        approval.get("proposedRule").is_none(),
        "a policy card offers no rule: {approval}"
    );
    let unexplained = policy_approval_card("e_3", "call_3", "pending", "shell", &args, None, 9);
    assert_eq!(
        unexplained["message"]["approval"]["reason"],
        POLICY_ASK_REASON
    );
}

#[test]
fn a_user_form_card_is_the_transcribed_shape_and_drops_secrets() {
    let raw = json!({
        "title": "Google account",
        "instruction": "Sign in.",
        "fields": [{
            "id": "password",
            "label": "Password",
            "type": "password",
            "required": true
        }],
        "values": { "password": "s3cret-pass" },
        "coworker_id": "cw_x",
        "liveHost": "accounts.google.com"
    });
    let card = user_form_card("e_form", &raw, 11, "mock-form-1");
    assert_eq!(card["kind"], "send-message");
    assert_eq!(card["id"], "e_form");
    assert_eq!(card["callId"], "mock-form-1");
    assert_eq!(card["timestampMs"], 11);
    assert_eq!(card["message"]["type"], "user-form");
    assert_eq!(card["message"]["formRequest"]["title"], "Google account");
    assert_eq!(
        card["message"]["formRequest"]["liveHost"],
        "accounts.google.com"
    );
    assert_eq!(
        card["message"]["formRequest"]["fields"][0]["id"],
        "password"
    );
    assert!(card.get("formResolution").is_none(), "{card}");
    let dumped = card.to_string();
    assert!(!dumped.contains("s3cret-pass"), "{dumped}");
    assert!(!dumped.contains("cw_x"), "{dumped}");
    assert!(card["message"]["formRequest"].get("values").is_none());
}

#[test]
fn a_computer_handoff_card_is_a_separate_sand_box_attachment() {
    let card = computer_handoff_card(
        "e_hand",
        "req_abc",
        "Google account: Enter the address and password.",
        12,
    );
    assert_eq!(card["kind"], "send-message");
    assert_eq!(card["id"], "e_hand");
    assert_eq!(card["message"]["type"], "attachment");
    assert_eq!(card["message"]["url"], "sand://box");
    assert_eq!(card["boxRequestId"], "req_abc");
    assert_eq!(
        card["boxInstruction"],
        "Google account: Enter the address and password."
    );
    assert!(
        card.get("boxResolution").is_none(),
        "boxResolution is absent while live: {card}"
    );
    assert!(
        card.get("formResolution").is_none(),
        "handoff is not a user-form: {card}"
    );
    let dumped = card.to_string();
    assert!(!dumped.to_lowercase().contains("take over"), "{dumped}");
    assert!(!dumped.to_lowercase().contains("i'm done"), "{dumped}");
    assert!(!dumped.to_lowercase().contains("skip"), "{dumped}");
}
