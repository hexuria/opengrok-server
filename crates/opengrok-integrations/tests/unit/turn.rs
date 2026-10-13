#![allow(clippy::unwrap_used)]
use super::*;

// Which ceilings switch a plugin on is `opengrok_policy::names_plugin`'s, tested there.

#[test]
fn only_minted_ids_are_read_as_plugin_skills() {
    assert!(is_plugin_skill("plugin/cw_1/demo/abc/triage"));
    assert!(!is_plugin_skill("skl_0193"));
}

#[test]
fn a_plugin_skill_id_names_its_bot_only_when_shaped_as_one_is_minted() {
    let bot = CoworkerId::from_stored("cw_1");
    let minted = plugin_skill_id(
        &bot,
        "demo",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "triage",
    );
    assert_eq!(plugin_skill_bot(&minted), Some("cw_1"));
    assert_eq!(
        plugin_skill_bot("plugin/cw_1/bir/server/file-tax"),
        Some("cw_1")
    );
    for id in [
        "plugin/../x",
        "plugin/cw_1/../server/file-tax",
        "plugin/cw_1//server/file-tax",
        "plugin/cw_1/bir/server/",
        "plugin/cw_1/bir/server/file tax",
        "plugin/cw_1/bir/server/file-tax\nWARN forged",
        "plugin/cw_1/bir/server/file-tax/extra",
        "skl_1/x/y/z/w",
    ] {
        assert_eq!(
            plugin_skill_bot(id),
            None,
            "{id:?} is not a plugin skill id"
        );
    }
}
