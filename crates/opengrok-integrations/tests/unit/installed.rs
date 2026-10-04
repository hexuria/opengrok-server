#![allow(clippy::unwrap_used)]
use super::*;
use serde_json::json;

#[test]
fn switching_a_plugin_off_takes_only_its_own_entries() {
    let stored = json!({"only": ["demo.*", "demo.hosted.search", "demolition.*", "shell"]});
    assert_eq!(
        without(stored, "demo").unwrap(),
        Some(json!({"only": ["demolition.*", "shell"]}))
    );
    assert_eq!(
        without(json!({"only": ["demo.*"]}), "demo").unwrap(),
        Some(json!("none"))
    );
}

#[test]
fn a_set_without_the_plugin_is_not_rewritten() {
    for stored in [
        json!({"only": ["shell"]}),
        json!("all"),
        json!("none"),
        json!(42),
    ] {
        assert_eq!(without(stored, "demo").unwrap(), None);
    }
}
