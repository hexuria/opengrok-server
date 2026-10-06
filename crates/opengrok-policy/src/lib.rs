//! What a principal may make a coworker do.
//!
//! THE DANGEROUS MESSAGE IS NOT A JAILBREAK. It is *"what's the status of order 8891?"* — a
//! reasonable sentence about somebody else's order. No amount of prompt hardening answers it; the
//! answer is that the identity is overwritten before the tool runs (`opengrok-tools`) and that the
//! *permission* is checked here, on every action, every time.
//!
//! ENFORCED EVERY TURN, NOT ONCE AT THE START. A session that was allowed when it opened is not
//! evidence about now: a grant can be revoked mid-conversation, and a check that happened at
//! sign-in would keep honouring it (CLAUDE.md #6).
//!
//! TWO LAYERS, COMBINED BY INTERSECTION AND NEVER BY UNION (`docs/PLAN.md` §4.5):
//!   - the coworker's **ceiling** — what it may *ever* do, set where it is defined;
//!   - the principal's **profile** — what *this* person may make it do.
//!
//! Intersection is what makes coworker-to-coworker delegation safe later: delegation can only ever
//! narrow. Union would let a permissive profile lift a coworker above its own ceiling.
//!
//! EVERY UNKNOWN DENIES. `None` anywhere in the context means "we could not establish it", and the
//! difference between "no" and "we do not know" must never be a way in — which is the same rule as
//! CLAUDE.md #8's "a typo may only ever narrow access", seen from the lookup side.

pub mod local_exec;
pub mod shell;

use std::collections::BTreeSet;

use opengrok_core::id::{AccountId, CoworkerId};
use serde::{Deserialize, Serialize};

/// A set of tool names, or "everything".
///
/// `All` is not sugar for listing every tool: a coworker whose ceiling is `All` should still be
/// narrowed by a profile that names three tools, and a list could not express "whatever exists
/// tomorrow" without being edited every time a tool is added.
///
/// A PLUGIN IS ADMITTED WHOLE BY `"<plugin>.*"` (#268). Its tools are `<plugin>.<server>.<tool>`,
/// and not one of those names is known until its server has been dialled — which this set gates
/// ([`may_run_any_under`]) — so a list of exact names could never let a plugin in. The entry
/// admits every name under `<plugin>.` and nothing else: not `<plugin>` itself, not a plugin whose
/// name merely starts the same way. Any other `*` is part of an ordinary name no tool has, so a
/// malformed entry only ever narrows — and a replica that predates this reads the entry the same
/// way, as a name nothing matches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolSet {
    All,
    Only(BTreeSet<String>),
    None,
}

/// The plugin a `"<plugin>.*"` entry admits whole; `None` for an ordinary tool name.
pub fn plugin_of(entry: &str) -> Option<&str> {
    entry
        .strip_suffix(".*")
        .filter(|plugin| !plugin.is_empty() && !plugin.contains('*'))
}

/// The entry that admits every tool `plugin` brings, today's and tomorrow's.
pub fn every_tool_of(plugin: &str) -> String {
    format!("{plugin}.*")
}

/// Whether `tool` is named under `plugin`: `<plugin>.` and at least one more character.
fn under(tool: &str, plugin: &str) -> bool {
    tool.strip_prefix(plugin)
        .and_then(|rest| rest.strip_prefix('.'))
        .is_some_and(|rest| !rest.is_empty())
}

/// The entries of `names` that `other` admits in full: a name it allows, or a whole plugin it
/// admits whole. A plugin one side admits whole and the other only name by name comes out as
/// those names, never as the plugin — the other side said nothing about its tools to come.
fn kept_by<'a>(
    names: &'a BTreeSet<String>,
    other: &'a ToolSet,
) -> impl Iterator<Item = String> + 'a {
    names
        .iter()
        .filter(move |name| match plugin_of(name) {
            Some(plugin) => other.allows_all_of(plugin),
            None => other.allows(name),
        })
        .cloned()
}

impl ToolSet {
    /// Named so `serde(default = ...)` can reach it.
    pub fn none() -> Self {
        Self::None
    }

    pub fn only<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::Only(names.into_iter().map(Into::into).collect())
    }

    pub fn allows(&self, tool: &str) -> bool {
        match self {
            Self::All => true,
            Self::Only(names) => {
                names.contains(tool) || self.whole_plugins().any(|plugin| under(tool, plugin))
            }
            Self::None => false,
        }
    }

    /// The plugins this set admits whole, by name. `All` names none: it admits every plugin
    /// without naming one.
    pub fn whole_plugins(&self) -> impl Iterator<Item = &str> {
        let names = match self {
            Self::Only(names) => Some(names),
            Self::All | Self::None => None,
        };
        names
            .into_iter()
            .flatten()
            .filter_map(|name| plugin_of(name))
    }

    /// Whether every tool `plugin` brings is admitted, whatever it brings next: `All`, or an entry
    /// that admits it whole (or admits a plugin its name sits under). Its tools listed by name never
    /// are — it may bring another tomorrow.
    pub fn allows_all_of(&self, plugin: &str) -> bool {
        matches!(self, Self::All)
            || self
                .whole_plugins()
                .any(|held| held == plugin || under(plugin, held))
    }

    /// INTERSECTION, NEVER UNION. The result can only be as permissive as the narrower side.
    #[must_use]
    pub fn intersect(&self, other: &Self) -> Self {
        match (self, other) {
            (Self::None, _) | (_, Self::None) => Self::None,
            (Self::All, other) | (other, Self::All) => other.clone(),
            (Self::Only(left), Self::Only(right)) => {
                let both: BTreeSet<String> =
                    kept_by(left, other).chain(kept_by(right, self)).collect();
                if both.is_empty() {
                    Self::None
                } else {
                    Self::Only(both)
                }
            }
        }
    }
}

/// What a principal is allowed to do with one coworker — layer 3.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub principal: AccountId,
    pub coworker: CoworkerId,
    pub profile: ToolSet,
    /// Tools this principal may use only with a human yes — layer 5.
    ///
    /// Checked INSIDE the profile, never beside it: a tool that is not in the profile at all is
    /// denied outright, and listing it here must not become a back door into running it. Asking
    /// for approval for something that was never permitted would train people to approve things
    /// nobody may do.
    #[serde(default = "ToolSet::none")]
    pub needs_approval: ToolSet,
    /// Set when the grant has been withdrawn. Kept rather than deleted, so the log still says a
    /// grant existed and when it stopped.
    #[serde(default)]
    pub revoked: bool,
}

/// What a coworker may *ever* do, whoever is asking — layer 2, its ceiling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ceiling {
    pub coworker: CoworkerId,
    pub tools: ToolSet,
}

/// What is being asked for.
#[derive(Debug, Clone)]
pub enum Action<'a> {
    /// May this principal talk to this coworker at all — layer 1, checked every turn.
    UseCoworker,
    /// May this principal make this coworker run this tool — layers 2 ∩ 3.
    RunTool(&'a str),
}

/// The answer. A denial always carries a reason, because a refusal the model cannot read is a
/// refusal it will retry forever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// LAYER 5: allowed in principle, but a person has to say yes first. Distinct from `Deny`
    /// because the run **suspends** rather than fails — a refusal ends a turn, an approval pauses
    /// one that can still be finished tomorrow.
    NeedsApproval(String),
    Deny(String),
}

impl Decision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }

    /// Waiting is not permission. Anything that treats `NeedsApproval` as allowed is a bug, so the
    /// only way to act on it is to ask for it by name.
    pub fn needs_approval(&self) -> bool {
        matches!(self, Self::NeedsApproval(_))
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Allow => None,
            Self::NeedsApproval(reason) | Self::Deny(reason) => Some(reason),
        }
    }
}

/// What the caller could look up. Every `None` denies — see the module note.
#[derive(Debug, Clone, Default)]
pub struct Context {
    pub grant: Option<Grant>,
    pub ceiling: Option<Ceiling>,
}

/// Decide. Pure and total: no clock, no I/O, no database — so every rule below is testable, and
/// the rules are the part that must never be wrong.
pub fn decide(
    principal: &AccountId,
    coworker: &CoworkerId,
    action: Action<'_>,
    context: &Context,
) -> Decision {
    // Layer 1: is there a grant at all?
    let Some(grant) = &context.grant else {
        // An absent grant is a denial, never a default-allow. This is the single most important
        // line in the file: a lookup that failed must not read as permission.
        return Decision::Deny(format!("no grant lets {principal} use coworker {coworker}"));
    };

    if grant.revoked {
        return Decision::Deny(format!("the grant for coworker {coworker} was revoked"));
    }

    // A grant for somebody else, or for another coworker, is not a grant. Re-checked here rather
    // than trusted, because the caller looked it up and lookups can be wrong.
    if &grant.principal != principal {
        return Decision::Deny(format!(
            "that grant belongs to {}, not to {principal}",
            grant.principal
        ));
    }
    if &grant.coworker != coworker {
        return Decision::Deny(format!(
            "that grant is for coworker {}, not {coworker}",
            grant.coworker
        ));
    }

    match action {
        Action::UseCoworker => Decision::Allow,

        Action::RunTool(tool) => {
            // Layer 2: the coworker's own ceiling. Unknown means denied — a coworker whose limits
            // we cannot read is not one we can let run anything.
            let Some(ceiling) = &context.ceiling else {
                return Decision::Deny(format!(
                    "coworker {coworker} has no tool ceiling on record, so nothing may run"
                ));
            };
            if &ceiling.coworker != coworker {
                return Decision::Deny(format!(
                    "that ceiling is for coworker {}, not {coworker}",
                    ceiling.coworker
                ));
            }

            // Layers 2 ∩ 3.
            if ceiling.tools.intersect(&grant.profile).allows(tool) {
                // Permitted — but does a person have to say yes first? Checked only after the tool
                // is known to be allowed, so approval can never widen access.
                if grant.needs_approval.allows(tool) {
                    return Decision::NeedsApproval(format!(
                        "running {tool} on coworker {coworker} needs a human yes"
                    ));
                }
                Decision::Allow
            } else if !ceiling.tools.allows(tool) {
                // Which layer refused matters to whoever has to fix it: one is the coworker's
                // definition, the other is this person's grant.
                Decision::Deny(format!("coworker {coworker} may never run {tool}"))
            } else {
                Decision::Deny(format!(
                    "{principal} may not make coworker {coworker} run {tool}"
                ))
            }
        }
    }
}

/// Whether an account-installed plugin is switched on: what the ceiling and grant BOTH admit names
/// it whole (`"<plugin>.*"`), on a live grant for this coworker. Every missing piece answers no.
///
/// NAMED, NOT MERELY ADMITTED. `All` admits every plugin without naming one, and an installed
/// plugin is code an account chose from a registry, not the server's own: on a Bot whose tools
/// were "all", an uninstall-then-install at a new pin went live without anyone choosing it again.
/// So such a plugin is on only once its owner switches it on by name, which a ceiling save does.
/// Its skills and its servers both answer to this; its tools still meet [`decide`] one by one.
pub fn names_plugin(
    principal: &AccountId,
    coworker: &CoworkerId,
    plugin: &str,
    context: &Context,
) -> bool {
    if !decide(principal, coworker, Action::UseCoworker, context).is_allowed() {
        return false;
    }
    let (Some(grant), Some(ceiling)) = (&context.grant, &context.ceiling) else {
        return false;
    };
    let both = ceiling.tools.intersect(&grant.profile);
    &ceiling.coworker == coworker && both.whole_plugins().any(|named| named == plugin)
}

/// `context` with each of `plugins` admitted whole, for ONE turn whose driving account owns the
/// coworker and named those plugins in its message (`@cloudflare`).
///
/// A WIDENING, AND THE ONLY ONE. It is sound because both layers it touches are the owner's own
/// word: the ceiling is the owner's setting, and on their own Bot the grant is theirs too, which is
/// why switching a plugin on saves it into both. A mention is that same person switching it on
/// for one message, and nothing is stored. The caller proves ownership and that each plugin is
/// installed by that account (`turn::mentioned`); a member of an org typing a name on a shared
/// Bot must never reach here, since for them both layers are somebody else's word. `All` stays
/// `All`: it already admits every tool, and the caller dials a mentioned plugin by name
/// (`names_plugin` answers no for `All`).
#[must_use]
pub fn with_mentioned(mut context: Context, plugins: &[String]) -> Context {
    fn widened(tools: ToolSet, plugins: &[String]) -> ToolSet {
        let named: BTreeSet<String> = plugins.iter().map(|p| every_tool_of(p)).collect();
        match tools {
            ToolSet::All => ToolSet::All,
            _ if named.is_empty() => tools,
            ToolSet::None => ToolSet::Only(named),
            ToolSet::Only(mut names) => {
                names.extend(named);
                ToolSet::Only(names)
            }
        }
    }
    if let Some(ceiling) = context.ceiling.as_mut() {
        ceiling.tools = widened(
            std::mem::replace(&mut ceiling.tools, ToolSet::None),
            plugins,
        );
    }
    if let Some(grant) = context.grant.as_mut() {
        grant.profile = widened(
            std::mem::replace(&mut grant.profile, ToolSet::None),
            plugins,
        );
    }
    context
}

/// Whether ANY tool whose name starts with `prefix` could be run (or held for a yes) — so a
/// caller can skip reaching a plugin server the coworker could use nothing from.
///
/// AN OPTIMISATION THAT CAN ONLY NARROW. It answers "is there any point asking", never "may this
/// run": every tool is still put through [`decide`] by name. It shares `decide`'s refusals — no
/// grant, a revoked one, one for somebody else, no ceiling — so an unknown answers no, and `All`
/// answers yes because a set that admits whatever exists tomorrow cannot be ruled out by name.
pub fn may_run_any_under(
    principal: &AccountId,
    coworker: &CoworkerId,
    prefix: &str,
    context: &Context,
) -> bool {
    if !decide(principal, coworker, Action::UseCoworker, context).is_allowed() {
        return false;
    }
    let (Some(grant), Some(ceiling)) = (&context.grant, &context.ceiling) else {
        return false;
    };
    if &ceiling.coworker != coworker {
        return false;
    }
    match ceiling.tools.intersect(&grant.profile) {
        ToolSet::All => true,
        ToolSet::Only(names) => names.iter().any(|name| match plugin_of(name) {
            // A whole plugin reaches every server under it; an entry narrower than the server
            // still reaches that server, for the tools it names.
            Some(plugin) => {
                let whole = format!("{plugin}.");
                whole.starts_with(prefix) || prefix.starts_with(&whole)
            }
            None => name.starts_with(prefix),
        }),
        ToolSet::None => false,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn principal() -> AccountId {
        AccountId::from_stored("acct_1")
    }

    fn coworker() -> CoworkerId {
        CoworkerId::from_stored("cw_1")
    }

    fn granted(profile: ToolSet, ceiling: ToolSet) -> Context {
        Context {
            grant: Some(Grant {
                principal: principal(),
                coworker: coworker(),
                profile,
                needs_approval: ToolSet::None,
                revoked: false,
            }),
            ceiling: Some(Ceiling {
                coworker: coworker(),
                tools: ceiling,
            }),
        }
    }

    #[test]
    fn an_installed_plugin_is_on_only_where_both_layers_name_it() {
        let named = ToolSet::only(["demo.*", "shell"]);
        let on = |ceiling: ToolSet, profile: ToolSet| {
            names_plugin(
                &principal(),
                &coworker(),
                "demo",
                &granted(profile, ceiling),
            )
        };
        assert!(on(named.clone(), named.clone()));
        assert!(on(ToolSet::All, named.clone()));
        assert!(
            !on(ToolSet::All, ToolSet::All),
            "all tools never names a plugin"
        );
        assert!(!on(named.clone(), ToolSet::only(["shell"])));
        assert!(!on(ToolSet::only(["demo.hosted.search"]), ToolSet::All));
        assert!(!on(ToolSet::only(["demolition.*"]), ToolSet::All));
        assert!(!names_plugin(
            &principal(),
            &coworker(),
            "demo",
            &Context::default()
        ));
    }

    /// The single most important rule: a missing grant is a denial, never a default-allow.
    #[test]
    fn without_a_grant_nothing_is_allowed() {
        let decision = decide(
            &principal(),
            &coworker(),
            Action::UseCoworker,
            &Context::default(),
        );
        assert!(!decision.is_allowed());
        assert!(decision.reason().unwrap().contains("no grant"));
    }

    /// Checked every turn, so revoking a grant stops the next turn rather than the next sign-in.
    #[test]
    fn a_revoked_grant_stops_working_immediately() {
        let mut context = granted(ToolSet::All, ToolSet::All);
        if let Some(grant) = context.grant.as_mut() {
            grant.revoked = true;
        }
        let decision = decide(&principal(), &coworker(), Action::UseCoworker, &context);
        assert!(!decision.is_allowed());
        assert!(decision.reason().unwrap().contains("revoked"));
    }

    /// Somebody else's grant is not a grant, however it was retrieved.
    #[test]
    fn another_principals_grant_does_not_admit_this_one() {
        let context = granted(ToolSet::All, ToolSet::All);
        let decision = decide(
            &AccountId::from_stored("acct_someone_else"),
            &coworker(),
            Action::UseCoworker,
            &context,
        );
        assert!(!decision.is_allowed());
    }

    /// A grant for a different coworker must not admit this one — the mistake a bad join makes.
    #[test]
    fn a_grant_for_another_coworker_does_not_admit_this_one() {
        let context = granted(ToolSet::All, ToolSet::All);
        let decision = decide(
            &principal(),
            &CoworkerId::from_stored("cw_someone_else"),
            Action::UseCoworker,
            &context,
        );
        assert!(!decision.is_allowed());
    }

    #[test]
    fn a_granted_principal_may_use_its_coworker() {
        let context = granted(ToolSet::All, ToolSet::All);
        assert!(decide(&principal(), &coworker(), Action::UseCoworker, &context).is_allowed());
    }

    /// INTERSECTION, NOT UNION. A permissive profile must not lift a coworker above its ceiling.
    #[test]
    fn a_profile_cannot_grant_more_than_the_coworkers_ceiling() {
        let context = granted(ToolSet::All, ToolSet::only(["read_file"]));
        assert!(
            decide(
                &principal(),
                &coworker(),
                Action::RunTool("read_file"),
                &context
            )
            .is_allowed()
        );
        let denied = decide(
            &principal(),
            &coworker(),
            Action::RunTool("shell"),
            &context,
        );
        assert!(!denied.is_allowed());
        // The reason names the ceiling, because that is what a person would have to change.
        assert!(
            denied.reason().unwrap().contains("may never run"),
            "{denied:?}"
        );
    }

    /// And the other direction: a generous ceiling does not widen a narrow profile.
    #[test]
    fn a_ceiling_cannot_grant_more_than_the_principals_profile() {
        let context = granted(ToolSet::only(["read_file"]), ToolSet::All);
        assert!(
            decide(
                &principal(),
                &coworker(),
                Action::RunTool("read_file"),
                &context
            )
            .is_allowed()
        );
        let denied = decide(
            &principal(),
            &coworker(),
            Action::RunTool("shell"),
            &context,
        );
        assert!(!denied.is_allowed());
        // This one names the principal, because the grant is what would have to change.
        assert!(
            denied.reason().unwrap().contains("may not make"),
            "{denied:?}"
        );
    }

    #[test]
    fn the_intersection_of_two_lists_is_what_both_allow() {
        let context = granted(
            ToolSet::only(["shell", "read_file"]),
            ToolSet::only(["read_file", "write_file"]),
        );
        assert!(
            decide(
                &principal(),
                &coworker(),
                Action::RunTool("read_file"),
                &context
            )
            .is_allowed()
        );
        for denied in ["shell", "write_file"] {
            assert!(
                !decide(&principal(), &coworker(), Action::RunTool(denied), &context).is_allowed(),
                "{denied} is in only one of the two sets and must be refused"
            );
        }
    }

    /// Disjoint sets mean nothing runs, rather than everything.
    #[test]
    fn disjoint_sets_intersect_to_nothing() {
        assert_eq!(
            ToolSet::only(["a"]).intersect(&ToolSet::only(["b"])),
            ToolSet::None
        );
    }

    #[test]
    fn none_beats_all_in_either_order() {
        assert_eq!(ToolSet::None.intersect(&ToolSet::All), ToolSet::None);
        assert_eq!(ToolSet::All.intersect(&ToolSet::None), ToolSet::None);
    }

    /// A coworker whose limits cannot be read is not one we let run anything — "we do not know"
    /// must never be a way in.
    #[test]
    fn a_missing_ceiling_denies_rather_than_defaults_open() {
        let mut context = granted(ToolSet::All, ToolSet::All);
        context.ceiling = None;
        let decision = decide(
            &principal(),
            &coworker(),
            Action::RunTool("shell"),
            &context,
        );
        assert!(!decision.is_allowed());
        assert!(decision.reason().unwrap().contains("no tool ceiling"));
    }

    /// A ceiling belonging to a different coworker is a lookup that went wrong, and a lookup that
    /// went wrong must not widen anything.
    #[test]
    fn a_ceiling_for_the_wrong_coworker_denies() {
        let mut context = granted(ToolSet::All, ToolSet::All);
        context.ceiling = Some(Ceiling {
            coworker: CoworkerId::from_stored("cw_other"),
            tools: ToolSet::All,
        });
        assert!(
            !decide(
                &principal(),
                &coworker(),
                Action::RunTool("shell"),
                &context
            )
            .is_allowed()
        );
    }

    /// Every denial can be read and acted on. A refusal the model cannot understand is one it
    /// retries forever.
    #[test]
    fn every_denial_says_why() {
        let contexts = [
            Context::default(),
            granted(ToolSet::None, ToolSet::All),
            granted(ToolSet::All, ToolSet::None),
        ];
        for context in contexts {
            let decision = decide(
                &principal(),
                &coworker(),
                Action::RunTool("shell"),
                &context,
            );
            let reason = decision.reason().unwrap_or_default();
            assert!(!reason.is_empty(), "a denial must carry a reason");
        }
    }

    /// Layer 5: a tool inside the profile but marked for approval suspends rather than runs.
    #[test]
    fn a_tool_marked_for_approval_is_neither_allowed_nor_denied() {
        let mut context = granted(ToolSet::All, ToolSet::All);
        if let Some(grant) = context.grant.as_mut() {
            grant.needs_approval = ToolSet::only(["shell"]);
        }
        let decision = decide(
            &principal(),
            &coworker(),
            Action::RunTool("shell"),
            &context,
        );
        assert!(decision.needs_approval(), "{decision:?}");
        // Waiting is not permission.
        assert!(
            !decision.is_allowed(),
            "approval pending must not read as allowed"
        );
        assert!(decision.reason().unwrap().contains("human yes"));
    }

    /// Other tools are unaffected: marking one for approval must not gate the rest.
    #[test]
    fn tools_not_marked_for_approval_still_run() {
        let mut context = granted(ToolSet::All, ToolSet::All);
        if let Some(grant) = context.grant.as_mut() {
            grant.needs_approval = ToolSet::only(["shell"]);
        }
        assert!(
            decide(
                &principal(),
                &coworker(),
                Action::RunTool("read_file"),
                &context
            )
            .is_allowed()
        );
    }

    /// APPROVAL MUST NOT BE A BACK DOOR. A tool outside the profile stays denied even when it is
    /// listed for approval — asking a person to approve something nobody may do would train them
    /// to approve anything.
    #[test]
    fn approval_cannot_widen_a_profile_that_never_allowed_the_tool() {
        let mut context = granted(ToolSet::only(["read_file"]), ToolSet::All);
        if let Some(grant) = context.grant.as_mut() {
            grant.needs_approval = ToolSet::only(["shell"]);
        }
        let decision = decide(
            &principal(),
            &coworker(),
            Action::RunTool("shell"),
            &context,
        );
        assert!(!decision.needs_approval(), "{decision:?}");
        assert!(!decision.is_allowed());
    }

    /// Nor past a ceiling: the coworker's own limits still win.
    #[test]
    fn approval_cannot_widen_past_the_ceiling() {
        let mut context = granted(ToolSet::All, ToolSet::only(["read_file"]));
        if let Some(grant) = context.grant.as_mut() {
            grant.needs_approval = ToolSet::only(["shell"]);
        }
        let decision = decide(
            &principal(),
            &coworker(),
            Action::RunTool("shell"),
            &context,
        );
        assert!(!decision.needs_approval());
        assert!(decision.reason().unwrap().contains("may never run"));
    }

    /// The invariant as a property: no action on an EMPTY context is ever allowed. If a future
    /// edit adds a default-allow path anywhere, this is what fails.
    #[test]
    fn an_empty_context_allows_nothing_at_all() {
        let actions = [
            Action::UseCoworker,
            Action::RunTool("shell"),
            Action::RunTool("read_file"),
            Action::RunTool("anything_at_all"),
        ];
        for action in actions {
            assert!(
                !decide(
                    &principal(),
                    &coworker(),
                    action.clone(),
                    &Context::default()
                )
                .is_allowed(),
                "an empty context must never allow anything"
            );
            // Nor may it ever ask for approval: approval is for things already permitted.
            assert!(
                !decide(&principal(), &coworker(), action, &Context::default()).needs_approval(),
                "an empty context must never ask for approval either"
            );
        }
    }

    /// #199: a plugin server the coworker could use nothing from is not worth reaching — and the
    /// answer to "is it worth it" can only ever be no where `decide` would say no.
    #[test]
    fn only_a_server_with_a_runnable_tool_is_worth_reaching() {
        let only_gmail = granted(ToolSet::All, ToolSet::only(["shell", "gmail.api.send"]));
        assert!(may_run_any_under(
            &principal(),
            &coworker(),
            "gmail.api.",
            &only_gmail
        ));
        assert!(!may_run_any_under(
            &principal(),
            &coworker(),
            "github.api.",
            &only_gmail
        ));
        // A server name that merely starts the same way is a different server.
        assert!(!may_run_any_under(
            &principal(),
            &coworker(),
            "gmail.apix.",
            &only_gmail
        ));

        // The PROFILE narrows too: intersection, never union.
        let profile_without = granted(ToolSet::only(["shell"]), ToolSet::All);
        assert!(!may_run_any_under(
            &principal(),
            &coworker(),
            "gmail.api.",
            &profile_without
        ));

        // `All` cannot be ruled out by name.
        let everything = granted(ToolSet::All, ToolSet::All);
        assert!(may_run_any_under(
            &principal(),
            &coworker(),
            "gmail.api.",
            &everything
        ));

        // Every unknown is a no, exactly as it is for `decide`.
        assert!(!may_run_any_under(
            &principal(),
            &coworker(),
            "gmail.api.",
            &Context::default()
        ));
        let mut revoked = everything.clone();
        if let Some(grant) = revoked.grant.as_mut() {
            grant.revoked = true;
        }
        assert!(!may_run_any_under(
            &principal(),
            &coworker(),
            "gmail.api.",
            &revoked
        ));
        let mut ceilingless = everything.clone();
        ceilingless.ceiling = None;
        assert!(!may_run_any_under(
            &principal(),
            &coworker(),
            "gmail.api.",
            &ceilingless
        ));
        let someone_else = AccountId::from_stored("acct_2");
        assert!(!may_run_any_under(
            &someone_else,
            &coworker(),
            "gmail.api.",
            &everything
        ));
    }

    /// #268: a plugin is switched on whole, before anybody knows its tools' names — and the entry
    /// admits exactly the tools under that plugin.
    #[test]
    fn a_whole_plugin_admits_its_own_tools_and_nothing_else() {
        let gmail = ToolSet::only([every_tool_of("gmail")]);
        assert!(gmail.allows("gmail.api.send"));
        assert!(gmail.allows("gmail.other.read"));
        // A plugin whose name only starts the same way is a different plugin.
        assert!(!gmail.allows("gmailx.a.b"));
        // The plugin's own name is not one of its tools, and neither is the bare prefix.
        assert!(!gmail.allows("gmail"));
        assert!(!gmail.allows("gmail."));
        assert!(!gmail.allows("shell"));
        assert!(gmail.allows_all_of("gmail"));
        assert!(!gmail.allows_all_of("gmailx"));
        // Its tools named one by one are not the plugin: it may bring another tomorrow.
        assert!(!ToolSet::only(["gmail.api.send"]).allows_all_of("gmail"));
        // PINNED: by name, a tool is its first segment's, where a call is routed too
        // (`split_qualified`), so `gmail.*` admits `gmail.api.x.send` and anything under
        // `gmail.api`. A plugin whose own name has a dot is never dialled (server
        // `connect_plugins`), so no tool of a plugin called `gmail.api` is ever offered this way.
        assert!(gmail.allows("gmail.api.x.send"));
        assert!(gmail.allows_all_of("gmail.api"));
        assert!(ToolSet::All.allows_all_of("gmail"));
        assert!(!ToolSet::None.allows_all_of("gmail"));
    }

    /// A typo may only ever narrow: an entry that is not `<plugin>.*` is a name no tool has.
    #[test]
    fn a_malformed_whole_plugin_entry_admits_nothing() {
        for entry in ["*", ".*", "gmail*", "gmail.**", "*.*"] {
            let set = ToolSet::only([entry]);
            for tool in ["gmail.api.send", "shell", "gmail", "x.y.z"] {
                assert!(!set.allows(tool), "{entry} must not admit {tool}");
            }
            assert!(!set.allows_all_of("gmail"), "{entry}");
            assert_eq!(plugin_of(entry), None, "{entry}");
        }
    }

    /// INTERSECTION, NEVER UNION, with whole plugins in it: a side that names one tool of a plugin
    /// narrows the other side's whole plugin to that tool, and `All`/`None` behave as they did.
    #[test]
    fn a_whole_plugin_intersects_down_to_what_both_sides_admit() {
        let gmail = ToolSet::only([every_tool_of("gmail")]);
        let send = ToolSet::only(["gmail.api.send"]);
        assert_eq!(gmail.intersect(&send), send);
        assert_eq!(send.intersect(&gmail), send);
        assert_eq!(gmail.intersect(&gmail), gmail);
        assert_eq!(gmail.intersect(&ToolSet::All), gmail);
        assert_eq!(ToolSet::All.intersect(&gmail), gmail);
        assert_eq!(gmail.intersect(&ToolSet::None), ToolSet::None);
        assert_eq!(ToolSet::None.intersect(&gmail), ToolSet::None);
        assert_eq!(
            gmail.intersect(&ToolSet::only([every_tool_of("gmailx")])),
            ToolSet::None
        );
        assert_eq!(
            gmail.intersect(&ToolSet::only(["shell"])),
            ToolSet::None,
            "a whole plugin and a built-in share nothing"
        );
        // A plugin under another's name is narrower, so it is what survives.
        let nested = ToolSet::only([every_tool_of("gmail.api")]);
        assert_eq!(gmail.intersect(&nested), nested);
        assert_eq!(nested.intersect(&gmail), nested);
        let mixed = ToolSet::only(["shell".to_string(), every_tool_of("gmail")]);
        assert_eq!(
            mixed.intersect(&ToolSet::only(["shell", "gmail.api.send", "read_file"])),
            ToolSet::only(["shell", "gmail.api.send"])
        );
    }

    /// The ceiling gate a turn runs: a plugin switched on in the ceiling runs only where the
    /// profile admits it too, and its server is worth reaching only then.
    #[test]
    fn a_whole_plugin_runs_only_where_both_layers_admit_it() {
        let gmail = || ToolSet::only(["shell".to_string(), every_tool_of("gmail")]);
        let both = granted(gmail(), gmail());
        let send = Action::RunTool("gmail.api.send");
        assert!(decide(&principal(), &coworker(), send.clone(), &both).is_allowed());
        assert!(may_run_any_under(
            &principal(),
            &coworker(),
            "gmail.api.",
            &both
        ));
        assert!(!may_run_any_under(
            &principal(),
            &coworker(),
            "gmailx.api.",
            &both
        ));
        assert!(
            !decide(
                &principal(),
                &coworker(),
                Action::RunTool("gmailx.a.b"),
                &both
            )
            .is_allowed()
        );

        let profile_without = granted(ToolSet::only(["shell"]), gmail());
        assert!(!decide(&principal(), &coworker(), send, &profile_without).is_allowed());
        assert!(!may_run_any_under(
            &principal(),
            &coworker(),
            "gmail.api.",
            &profile_without
        ));
    }
}
