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
