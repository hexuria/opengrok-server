//! The coworker aggregate — who a coworker is, and which computer is theirs.
//!
//! A COMPUTER IS ASSIGNED, NOT REQUESTED. The client says "this coworker should work"; the server
//! decides what it works on. A client that could name a box id could name *somebody else's* box
//! id, and the identity rule (CLAUDE.md #7) says arguments are overwritten, not validated — so the
//! box a run uses is read from this row and never from a payload.
//!
//! DEDICATED OR SHARED IS CONFIGURATION, NOT TWO CODE PATHS. `Dedicated` means this coworker owns
//! the box and stopping it is safe; `Shared` means several coworkers use one and it must not be
//! destroyed when one of them is done. Both are the same `BoxId` behind the same trait — the mode
//! only changes who may end its life.

use serde::{Deserialize, Serialize};

use crate::id::{BoxId, CoworkerId};

/// How a coworker's computer is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BoxMode {
    /// This coworker's own machine. Stopping or destroying it affects nobody else.
    Dedicated,
    /// One machine several coworkers share. Ending it is not one coworker's decision.
    Shared,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum CoworkerEvent {
    Hired {
        name: String,
        /// A route through the gateway (`xai/grok-4.6@sub`), never a key.
        model: String,
        at_ms: i64,
    },
    Renamed {
        name: String,
        at_ms: i64,
    },
    /// The route this coworker thinks through changed. A pin is an operating fact about the
    /// coworker, not the deployment's — hiring one model and answering on another is the bug
    /// this whole aggregate's model field exists to prevent.
    Repinned {
        /// A route through the gateway (`openai/gpt-5.5`), never a key.
        model: String,
        at_ms: i64,
    },
    /// A computer became this coworker's.
    ComputerAssigned {
        box_id: BoxId,
        mode: BoxMode,
        at_ms: i64,
    },
    /// The computer went away — stopped, destroyed, or reclaimed.
    ComputerReleased {
        at_ms: i64,
    },
    Retired {
        at_ms: i64,
    },
    /// A GROUP was hired: a coworker whose members do the thinking. It has no computer and no
    /// model of its own — its transcript is the room, and each member's turn runs on that
    /// member's model, key, tools and policy (`plan-rooms.md` §2).
    GroupHired {
        name: String,
        members: Vec<CoworkerId>,
        at_ms: i64,
    },
    MembersSet {
        members: Vec<CoworkerId>,
        at_ms: i64,
    },
}

/// The most members a group holds — the client's own `GROUP_MAX_MEMBERS`
/// (`shared/agents/agents.ts:53`), transcribed, not chosen.
pub const GROUP_MAX_MEMBERS: usize = 6;

impl CoworkerEvent {
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::Hired { .. } => "coworker-hired",
            Self::Renamed { .. } => "coworker-renamed",
            Self::Repinned { .. } => "coworker-repinned",
            Self::ComputerAssigned { .. } => "computer-assigned",
            Self::ComputerReleased { .. } => "computer-released",
            Self::Retired { .. } => "coworker-retired",
            Self::GroupHired { .. } => "group-hired",
            Self::MembersSet { .. } => "members-set",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Coworker {
    pub hired: bool,
    pub retired: bool,
    pub name: String,
    pub model: String,
    pub box_id: Option<BoxId>,
    pub box_mode: Option<BoxMode>,
    /// Non-empty ⇒ this coworker is a group of these. Order is the order they were named.
    pub members: Vec<CoworkerId>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CoworkerError {
    #[error("no such coworker")]
    NotHired,
    #[error("that coworker has retired")]
    Retired,
    #[error("that coworker already has a computer")]
    AlreadyHasComputer,
    #[error("that coworker has no computer")]
    NoComputer,
    #[error("a shared computer is not one coworker's to release")]
    SharedComputer,
    /// A blank model reaches the gateway as `"model": ""`, which it answers with a refusal
    /// nobody can act on. Hire accepted one until slice 18 — the 400 arms its callers already
    /// wrote were unreachable, so the invalid value simply got stored.
    #[error("a coworker needs a model to think with")]
    EmptyModel,
    #[error("a group needs at least one member")]
    NoMembers,
    #[error("a group holds at most {GROUP_MAX_MEMBERS} members")]
    TooManyMembers,
    #[error("that coworker is not a group")]
    NotAGroup,
}

#[derive(Debug, Clone)]
pub enum CoworkerCommand {
    Hire {
        name: String,
        model: String,
        at_ms: i64,
    },
    Rename {
        name: String,
        at_ms: i64,
    },
    /// Point this coworker at a different route. Deliberately its own command: renaming and
    /// repinning are different decisions, and a caller that means one must not do the other.
    Repin {
        model: String,
        at_ms: i64,
    },
    AssignComputer {
        box_id: BoxId,
        mode: BoxMode,
        at_ms: i64,
    },
    ReleaseComputer {
        at_ms: i64,
    },
    Retire {
        at_ms: i64,
    },
    /// Hire a group of existing coworkers. Whether the members EXIST, are not retired and are
    /// not groups themselves is the server's to check (it takes other rows); this aggregate
    /// keeps the list de-duplicated and bounded.
    HireGroup {
        name: String,
        members: Vec<CoworkerId>,
        at_ms: i64,
    },
    SetMembers {
        members: Vec<CoworkerId>,
        at_ms: i64,
    },
}

impl Coworker {
    pub fn replay<'a>(events: impl IntoIterator<Item = &'a CoworkerEvent>) -> Self {
        let mut state = Self::default();
        for event in events {
            state.apply(event);
        }
        state
    }

    pub fn apply(&mut self, event: &CoworkerEvent) {
        match event {
            CoworkerEvent::Hired { name, model, .. } => {
                self.hired = true;
                self.name = name.clone();
                self.model = model.clone();
            }
            CoworkerEvent::Renamed { name, .. } => self.name = name.clone(),
            CoworkerEvent::Repinned { model, .. } => self.model = model.clone(),
            CoworkerEvent::ComputerAssigned { box_id, mode, .. } => {
                self.box_id = Some(box_id.clone());
                self.box_mode = Some(*mode);
            }
            CoworkerEvent::ComputerReleased { .. } => {
                self.box_id = None;
                self.box_mode = None;
            }
            CoworkerEvent::Retired { .. } => self.retired = true,
            CoworkerEvent::GroupHired { name, members, .. } => {
                self.hired = true;
                self.name = name.clone();
                // A sentinel, never a route: a group takes no model call of its own, and a
                // caller that asked the gateway for "group" would be told so in plain words.
                self.model = "group".to_string();
                self.members = members.clone();
            }
            CoworkerEvent::MembersSet { members, .. } => self.members = members.clone(),
        }
    }

    /// A group is a coworker with members. Everything a group cannot do (take a model call,
    /// own a computer, hold a key) follows from this one predicate.
    pub fn is_group(&self) -> bool {
        !self.members.is_empty()
    }

    /// De-duplicated in the order given, one to `GROUP_MAX_MEMBERS` of them.
    fn member_list(members: Vec<CoworkerId>) -> Result<Vec<CoworkerId>, CoworkerError> {
        let mut seen: Vec<CoworkerId> = Vec::with_capacity(members.len());
        for member in members {
            if !seen.contains(&member) {
                seen.push(member);
            }
        }
        if seen.is_empty() {
            return Err(CoworkerError::NoMembers);
        }
        if seen.len() > GROUP_MAX_MEMBERS {
            return Err(CoworkerError::TooManyMembers);
        }
        Ok(seen)
    }

    /// The computer a run must use. Read from here, never from a request.
    pub fn computer(&self) -> Option<&BoxId> {
        self.box_id.as_ref()
    }

    /// The box's mode — dedicated (this coworker's own) or shared — or `None` when it has no box.
    pub fn box_mode(&self) -> Option<BoxMode> {
        self.box_mode
    }

    /// A model must be a route somebody could actually be served on. Trimmed, because a pin of
    /// spaces is the same lie as an empty one.
    fn non_blank(model: String) -> Result<String, CoworkerError> {
        let trimmed = model.trim();
        if trimmed.is_empty() {
            return Err(CoworkerError::EmptyModel);
        }
        Ok(trimmed.to_string())
    }

    fn alive(&self) -> Result<(), CoworkerError> {
        if !self.hired {
            return Err(CoworkerError::NotHired);
        }
        if self.retired {
            return Err(CoworkerError::Retired);
        }
        Ok(())
    }

    pub fn decide(&self, command: CoworkerCommand) -> Result<Vec<CoworkerEvent>, CoworkerError> {
        match command {
            CoworkerCommand::Hire { name, model, at_ms } => {
                // Validated at last. Every caller already had a 400 arm for this; none of them
                // could ever be reached, so `model: ""` was stored and later asked of the
                // gateway verbatim.
                let model = Self::non_blank(model)?;
                Ok(vec![CoworkerEvent::Hired { name, model, at_ms }])
            }

            CoworkerCommand::Repin { model, at_ms } => {
                self.alive()?;
                let model = Self::non_blank(model)?;
                Ok(vec![CoworkerEvent::Repinned { model, at_ms }])
            }

            CoworkerCommand::Rename { name, at_ms } => {
                self.alive()?;
                Ok(vec![CoworkerEvent::Renamed { name, at_ms }])
            }

            CoworkerCommand::AssignComputer {
                box_id,
                mode,
                at_ms,
            } => {
                self.alive()?;
                // Reassigning silently would strand the previous box: still billing, still holding
                // the coworker's files, and now unreachable because nothing points at it.
                if self.box_id.is_some() {
                    return Err(CoworkerError::AlreadyHasComputer);
                }
                Ok(vec![CoworkerEvent::ComputerAssigned {
                    box_id,
                    mode,
                    at_ms,
                }])
            }

            CoworkerCommand::ReleaseComputer { at_ms } => {
                self.alive()?;
                if self.box_id.is_none() {
                    return Err(CoworkerError::NoComputer);
                }
                // Releasing a shared box would take it from everyone else using it.
                if self.box_mode == Some(BoxMode::Shared) {
                    return Err(CoworkerError::SharedComputer);
                }
                Ok(vec![CoworkerEvent::ComputerReleased { at_ms }])
            }

            CoworkerCommand::Retire { at_ms } => {
                self.alive()?;
                Ok(vec![CoworkerEvent::Retired { at_ms }])
            }

            CoworkerCommand::HireGroup {
                name,
                members,
                at_ms,
            } => {
                let members = Self::member_list(members)?;
                Ok(vec![CoworkerEvent::GroupHired {
                    name,
                    members,
                    at_ms,
                }])
            }

            CoworkerCommand::SetMembers { members, at_ms } => {
                self.alive()?;
                if !self.is_group() {
                    return Err(CoworkerError::NotAGroup);
                }
                let members = Self::member_list(members)?;
                Ok(vec![CoworkerEvent::MembersSet { members, at_ms }])
            }
        }
    }
}

/// The roster row a client lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoworkerView {
    pub id: CoworkerId,
    pub name: String,
    pub model: String,
    pub box_id: Option<BoxId>,
    pub retired: bool,
    /// The client's sort key — see `research/client-grok-bot.md` §8.1.
    pub updated_at_ms: i64,
    /// A group's members; empty for an ordinary coworker. The roster's `isGroup`/`memberIds`.
    #[serde(default)]
    pub members: Vec<CoworkerId>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn hired() -> Coworker {
        let mut coworker = Coworker::default();
        for event in coworker
            .decide(CoworkerCommand::Hire {
                name: "Ada".to_string(),
                model: "xai/grok-4.6".to_string(),
                at_ms: 1,
            })
            .unwrap()
        {
            coworker.apply(&event);
        }
        coworker
    }

    fn with_computer(mode: BoxMode) -> Coworker {
        let mut coworker = hired();
        for event in coworker
            .decide(CoworkerCommand::AssignComputer {
                box_id: BoxId::from_stored("box_1"),
                mode,
                at_ms: 2,
            })
            .unwrap()
        {
            coworker.apply(&event);
        }
        coworker
    }

    #[test]
    fn a_hired_coworker_has_a_name_and_a_route() {
        let coworker = hired();
        assert!(coworker.hired);
        assert_eq!(coworker.name, "Ada");
        assert_eq!(coworker.model, "xai/grok-4.6");
        assert_eq!(coworker.computer(), None, "and no computer yet");
    }

    #[test]
    fn assigning_a_computer_makes_it_the_one_a_run_uses() {
        let coworker = with_computer(BoxMode::Dedicated);
        assert_eq!(coworker.computer(), Some(&BoxId::from_stored("box_1")));
    }

    /// Silently reassigning would strand the previous box: still billing, still holding the
    /// coworker's files, and unreachable because nothing points at it.
    #[test]
    fn a_second_computer_is_refused_rather_than_stranding_the_first() {
        let coworker = with_computer(BoxMode::Dedicated);
        assert_eq!(
            coworker.decide(CoworkerCommand::AssignComputer {
                box_id: BoxId::from_stored("box_2"),
                mode: BoxMode::Dedicated,
                at_ms: 3,
            }),
            Err(CoworkerError::AlreadyHasComputer)
        );
    }

    #[test]
    fn a_dedicated_computer_can_be_released() {
        let mut coworker = with_computer(BoxMode::Dedicated);
        for event in coworker
            .decide(CoworkerCommand::ReleaseComputer { at_ms: 4 })
            .unwrap()
        {
            coworker.apply(&event);
        }
        assert_eq!(coworker.computer(), None);
    }

    /// Releasing a shared box would take it from everyone else using it.
    #[test]
    fn a_shared_computer_is_not_one_coworkers_to_release() {
        let coworker = with_computer(BoxMode::Shared);
        assert_eq!(
            coworker.decide(CoworkerCommand::ReleaseComputer { at_ms: 4 }),
            Err(CoworkerError::SharedComputer)
        );
    }

    #[test]
    fn releasing_a_computer_that_does_not_exist_is_refused() {
        assert_eq!(
            hired().decide(CoworkerCommand::ReleaseComputer { at_ms: 4 }),
            Err(CoworkerError::NoComputer)
        );
    }

    #[test]
    fn a_retired_coworker_accepts_no_further_commands() {
        let mut coworker = with_computer(BoxMode::Dedicated);
        for event in coworker
            .decide(CoworkerCommand::Retire { at_ms: 5 })
            .unwrap()
        {
            coworker.apply(&event);
        }
        assert_eq!(
            coworker.decide(CoworkerCommand::Rename {
                name: "Grace".to_string(),
                at_ms: 6
            }),
            Err(CoworkerError::Retired)
        );
        assert_eq!(
            coworker.decide(CoworkerCommand::ReleaseComputer { at_ms: 6 }),
            Err(CoworkerError::Retired)
        );
    }

    #[test]
    fn commands_against_a_coworker_who_does_not_exist_are_refused() {
        assert_eq!(
            Coworker::default().decide(CoworkerCommand::Rename {
                name: "x".to_string(),
                at_ms: 1
            }),
            Err(CoworkerError::NotHired)
        );
    }

    #[test]
    fn replay_reconstructs_the_same_state() {
        let log = vec![
            CoworkerEvent::Hired {
                name: "Ada".to_string(),
                model: "xai/grok-4.6".to_string(),
                at_ms: 1,
            },
            CoworkerEvent::ComputerAssigned {
                box_id: BoxId::from_stored("box_1"),
                mode: BoxMode::Dedicated,
                at_ms: 2,
            },
            CoworkerEvent::Renamed {
                name: "Grace".to_string(),
                at_ms: 3,
            },
        ];
        let coworker = Coworker::replay(&log);
        assert_eq!(coworker.name, "Grace");
        assert_eq!(coworker.computer(), Some(&BoxId::from_stored("box_1")));
        assert_eq!(coworker.box_mode, Some(BoxMode::Dedicated));
    }

    fn repin(coworker: &mut Coworker, model: &str) -> Result<(), CoworkerError> {
        for event in coworker.decide(CoworkerCommand::Repin {
            model: model.to_string(),
            at_ms: 9,
        })? {
            coworker.apply(&event);
        }
        Ok(())
    }

    /// The point of the slice: a pin is changeable, and changing it changes nothing else.
    #[test]
    fn repinning_changes_the_model_and_only_the_model() {
        let mut coworker = with_computer(BoxMode::Dedicated);
        repin(&mut coworker, "openai/gpt-5.5").unwrap();
        assert_eq!(coworker.model, "openai/gpt-5.5");
        assert_eq!(coworker.name, "Ada", "repinning is not renaming");
        assert_eq!(
            coworker.computer(),
            Some(&BoxId::from_stored("box_1")),
            "repinning does not disturb the computer"
        );
        assert!(!coworker.retired);
    }

    /// Renaming and repinning are different decisions; neither may do the other's work.
    #[test]
    fn renaming_does_not_disturb_the_pin() {
        let mut coworker = hired();
        for event in coworker
            .decide(CoworkerCommand::Rename {
                name: "Grace".to_string(),
                at_ms: 3,
            })
            .unwrap()
        {
            coworker.apply(&event);
        }
        assert_eq!(coworker.model, "xai/grok-4.6");
    }

    /// A retired coworker takes no more decisions — the same guard every other command uses.
    #[test]
    fn a_retired_coworker_cannot_be_repinned() {
        let mut coworker = hired();
        for event in coworker
            .decide(CoworkerCommand::Retire { at_ms: 4 })
            .unwrap()
        {
            coworker.apply(&event);
        }
        assert_eq!(
            repin(&mut coworker, "openai/gpt-5.5"),
            Err(CoworkerError::Retired)
        );
    }

    /// A blank pin reaches the gateway as `"model": ""`. Refused at the only place that can
    /// refuse it once, for every caller — this was storable until slice 18.
    #[test]
    fn a_blank_model_is_refused_on_hire_and_on_repin() {
        for blank in ["", "   ", "\t"] {
            assert_eq!(
                Coworker::default().decide(CoworkerCommand::Hire {
                    name: "Ada".to_string(),
                    model: blank.to_string(),
                    at_ms: 1,
                }),
                Err(CoworkerError::EmptyModel),
                "hire accepted a blank pin: {blank:?}"
            );
            let mut coworker = hired();
            assert_eq!(
                repin(&mut coworker, blank),
                Err(CoworkerError::EmptyModel),
                "repin accepted a blank pin: {blank:?}"
            );
            assert_eq!(coworker.model, "xai/grok-4.6", "the old pin survived");
        }
    }

    /// Surrounding space is not part of a route name; storing it would ask the gateway for a
    /// model that differs from the one a person typed.
    #[test]
    fn a_pin_is_stored_trimmed() {
        let mut coworker = hired();
        repin(&mut coworker, "  openai/gpt-5.5  ").unwrap();
        assert_eq!(coworker.model, "openai/gpt-5.5");
    }

    /// The computer survives a rename, because it is a different fact about the same coworker.
    #[test]
    fn renaming_does_not_disturb_the_computer() {
        let mut coworker = with_computer(BoxMode::Dedicated);
        for event in coworker
            .decide(CoworkerCommand::Rename {
                name: "Grace".to_string(),
                at_ms: 3,
            })
            .unwrap()
        {
            coworker.apply(&event);
        }
        assert_eq!(coworker.computer(), Some(&BoxId::from_stored("box_1")));
    }

    #[test]
    fn a_group_is_a_coworker_with_members_bounded_and_deduplicated() {
        let a = CoworkerId::from_stored("cw_a");
        let b = CoworkerId::from_stored("cw_b");
        let events = Coworker::default()
            .decide(CoworkerCommand::HireGroup {
                name: "Pair".to_string(),
                members: vec![a.clone(), b.clone(), a.clone()],
                at_ms: 1,
            })
            .unwrap_or_default();
        let group = Coworker::replay(&events);
        assert!(group.is_group());
        assert_eq!(
            group.members,
            vec![a.clone(), b.clone()],
            "de-duplicated, in order"
        );
        assert_eq!(group.model, "group", "a sentinel, never a route");
        assert!(matches!(
            Coworker::default().decide(CoworkerCommand::HireGroup {
                name: "Empty".to_string(),
                members: Vec::new(),
                at_ms: 1,
            }),
            Err(CoworkerError::NoMembers)
        ));
        let many: Vec<CoworkerId> = (0..=GROUP_MAX_MEMBERS)
            .map(|n| CoworkerId::from_stored(format!("cw_{n}")))
            .collect();
        assert!(matches!(
            Coworker::default().decide(CoworkerCommand::HireGroup {
                name: "Crowd".to_string(),
                members: many,
                at_ms: 1,
            }),
            Err(CoworkerError::TooManyMembers)
        ));
        // Members can be reset; an ordinary coworker cannot be given members after the fact.
        let reset = group
            .decide(CoworkerCommand::SetMembers {
                members: vec![b.clone()],
                at_ms: 2,
            })
            .unwrap_or_default();
        let mut group = group;
        for event in &reset {
            group.apply(event);
        }
        assert_eq!(group.members, vec![b]);
        let solo = Coworker::replay(
            &Coworker::default()
                .decide(CoworkerCommand::Hire {
                    name: "Solo".to_string(),
                    model: "oag/cheap".to_string(),
                    at_ms: 1,
                })
                .unwrap_or_default(),
        );
        assert!(!solo.is_group());
        assert!(matches!(
            solo.decide(CoworkerCommand::SetMembers {
                members: vec![a],
                at_ms: 2,
            }),
            Err(CoworkerError::NotAGroup)
        ));
    }
}
