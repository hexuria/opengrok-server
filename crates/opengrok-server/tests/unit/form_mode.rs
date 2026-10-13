use super::*;

fn card() -> FormRequest {
    form_request_from(&json!({
        "formMode": true,
        "plugin": "bir",
        "fingerprint": "sha256:seen",
        "form": {"form": "0619E", "tin": "123456789000", "period": "2026-09"},
        "fields": [
            {"id": "tax14", "label": "Amount"},
            {"id": "tax16", "label": "Net", "readOnly": true}
        ]
    }))
}

fn key() -> form_mode::FormKey {
    card().form.unwrap()
}

/// A save carries the card's identity and only the boxes the person may type in: a key the
/// card does not have and a box the plugin computes never reach the plugin.
#[test]
fn a_save_sends_only_the_cards_own_boxes() {
    let press = json!({"values": {"tax14": "12,500.00", "tax16": "1", "other": "x"}});
    let sent = arguments_for("save", &card(), &key(), &press).unwrap();
    assert_eq!(
        sent,
        json!({"form": key().to_value(), "values": {"tax14": "12,500.00"}})
    );
    let nothing = json!({"values": {"tax16": "1"}});
    assert!(arguments_for("save", &card(), &key(), &nothing).is_err());
}

/// Submit sends the fingerprint the person saw, falling back to the card's own, and never an
/// identity the press names: the card's is the only one.
#[test]
fn a_submit_sends_the_fingerprint_the_person_saw() {
    let press = json!({"fingerprint": "sha256:pressed", "form": {"form": "1601C"}});
    let sent = arguments_for("submit", &card(), &key(), &press).unwrap();
    assert_eq!(
        sent,
        json!({"form": key().to_value(), "fingerprint": "sha256:pressed"})
    );
    let fallback = arguments_for("submit", &card(), &key(), &json!({})).unwrap();
    assert_eq!(fallback["fingerprint"], json!("sha256:seen"));
}

#[test]
fn focus_view_and_refresh_take_what_their_tools_take() {
    let focus = arguments_for("focus", &card(), &key(), &json!({"key": "tax14"})).unwrap();
    assert_eq!(focus["key"], json!("tax14"));
    assert!(arguments_for("focus", &card(), &key(), &json!({"key": "nope"})).is_err());
    let view = arguments_for("view", &card(), &key(), &json!({})).unwrap();
    assert_eq!(view, json!({"form": key().to_value(), "inline": true}));
    let page = arguments_for("refresh", &card(), &key(), &json!({"page": 2})).unwrap();
    assert_eq!(page["mode"], json!("page"));
    let index = arguments_for("refresh", &card(), &key(), &json!({})).unwrap();
    assert_eq!(index["mode"], json!("index"));
}

fn entry() -> Value {
    json!({"kind": "send-message", "id": "e_1", "threadId": "t",
           "message": {"type": "user-form", "formRequest": serde_json::to_value(card()).unwrap()}})
}

/// A dismissed card is closed, not dead: Resume (a refresh) opens it again with the plugin's
/// card, and nothing of its dismissal is left to read it as closed.
#[test]
fn a_dismissed_card_reopens_on_refresh() {
    let dismissed = settle_dismissed(entry(), Some("tax14"));
    assert_eq!(dismissed["formResolution"], json!("dismissed"));
    assert_eq!(dismissed["widgetDismissed"], json!(true));
    assert_eq!(dismissed["formOutcome"]["lastFocus"], json!("tax14"));
    let mut fresh = card();
    fresh.fingerprint = Some("sha256:fresh".to_string());
    let opened = reopened(dismissed, &fresh);
    for key in ["formResolution", "widgetDismissed", "formOutcome"] {
        assert!(opened.get(key).is_none(), "{key}: {opened}");
    }
    assert_eq!(
        opened["message"]["formRequest"]["fingerprint"],
        json!("sha256:fresh")
    );
}

/// A submitted card is drawn from what was queued: the plugin's values, every box read only, no
/// error left, and the view answers from that snapshot.
#[test]
fn a_submitted_card_keeps_its_snapshot_and_shows_it() {
    let values = json!({"tax14": "12,500.00"});
    let values = values.as_object().unwrap();
    let settled = settle_submitted(entry(), Some("q_1"), Some("sha256:seen"), values);
    assert_eq!(settled["formResolution"], json!("submitted"));
    let fields = settled["message"]["formRequest"]["fields"]
        .as_array()
        .unwrap();
    assert_eq!(fields[0]["value"], json!("12,500.00"));
    assert!(fields.iter().all(|field| field["readOnly"] == json!(true)));
    assert_eq!(settled["message"]["formRequest"]["status"], json!("queued"));
    let view = shown(&settled);
    assert_eq!(view["values"]["tax14"], json!("12,500.00"));
    assert_eq!(view["queueRef"], json!("q_1"));
}
