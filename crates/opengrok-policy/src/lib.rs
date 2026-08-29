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

use std::collections::BTreeSet;

use opengrok_core::id::{AccountId, CoworkerId};
use serde::{Deserialize, Serialize};

/// A set of tool names, or "everything".
///
/// `All` is not sugar for listing every tool: a coworker whose ceiling is `All` should still be
/// narrowed by a profile that names three tools, and a list could not express "whatever exists
/// tomorrow" without being edited every time a tool is added.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolSet {
    All,
    Only(BTreeSet<String>),
    None,
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
            Self::Only(names) => names.contains(tool),
            Self::None => false,
        }
    }

    /// INTERSECTION, NEVER UNION. The result can only be as permissive as the narrower side.
    #[must_use]
    pub fn intersect(&self, other: &Self) -> Self {
        match (self, other) {
            (Self::None, _) | (_, Self::None) => Self::None,
            (Self::All, other) | (other, Self::All) => other.clone(),
            (Self::Only(left), Self::Only(right)) => {
                let both: BTreeSet<String> = left.intersection(right).cloned().collect();
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
}
