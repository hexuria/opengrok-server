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
}

impl CoworkerEvent {
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::Hired { .. } => "coworker-hired",
            Self::Renamed { .. } => "coworker-renamed",
            Self::ComputerAssigned { .. } => "computer-assigned",
            Self::ComputerReleased { .. } => "computer-released",
            Self::Retired { .. } => "coworker-retired",
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
            CoworkerEvent::ComputerAssigned { box_id, mode, .. } => {
                self.box_id = Some(box_id.clone());
                self.box_mode = Some(*mode);
            }
            CoworkerEvent::ComputerReleased { .. } => {
                self.box_id = None;
                self.box_mode = None;
            }
            CoworkerEvent::Retired { .. } => self.retired = true,
        }
    }

    /// The computer a run must use. Read from here, never from a request.
    pub fn computer(&self) -> Option<&BoxId> {
        self.box_id.as_ref()
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
                Ok(vec![CoworkerEvent::Hired { name, model, at_ms }])
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
}
