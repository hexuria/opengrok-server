#![allow(clippy::unwrap_used)]
use super::*;
use opengrok_policy::{Ceiling, Grant, ToolSet};

fn policy(ceiling: ToolSet, profile: ToolSet) -> (AccountId, CoworkerId, Context) {
    let (account, bot) = (
        AccountId::from_stored("acct_1"),
        CoworkerId::from_stored("cw_1"),
    );
    let grant = Grant {
        principal: account.clone(),
        coworker: bot.clone(),
        profile,
        needs_approval: ToolSet::None,
        revoked: false,
    };
    let ceiling = Ceiling {
        coworker: bot.clone(),
        tools: ceiling,
    };
    let context = Context {
        grant: Some(grant),
        ceiling: Some(ceiling),
    };
    (account, bot, context)
}

#[test]
fn a_plugin_is_on_only_where_ceiling_and_grant_both_admit_it_whole() {
    let on = ToolSet::only(["demo.*", "shell"]);
    let (account, bot, both) = policy(on.clone(), on.clone());
    assert!(switched_on(&account, &bot, "demo", &both));
    assert!(!switched_on(&account, &bot, "other", &both));
    let (_, _, ceiling_only) = policy(on.clone(), ToolSet::only(["shell"]));
    assert!(!switched_on(&account, &bot, "demo", &ceiling_only));
    let (_, _, one_tool) = policy(ToolSet::only(["demo.hosted.search"]), ToolSet::All);
    assert!(!switched_on(&account, &bot, "demo", &one_tool));
    assert!(!switched_on(&account, &bot, "demo", &Context::default()));
}

#[test]
fn only_minted_ids_are_read_as_plugin_skills() {
    assert!(is_plugin_skill("plugin/cw_1/demo/abc/triage"));
    assert!(!is_plugin_skill("skl_0193"));
}
