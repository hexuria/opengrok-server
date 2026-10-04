#![allow(clippy::unwrap_used)]
use super::*;

// Which ceilings switch a plugin on is `opengrok_policy::names_plugin`'s, tested there.

#[test]
fn only_minted_ids_are_read_as_plugin_skills() {
    assert!(is_plugin_skill("plugin/cw_1/demo/abc/triage"));
    assert!(!is_plugin_skill("skl_0193"));
}
