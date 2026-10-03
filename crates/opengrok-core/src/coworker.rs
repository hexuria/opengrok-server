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
use serde_json::Value;

use crate::id::{BoxId, CoworkerId};
use crate::inference::{NewBotDefault, SourceKind};

/// How a coworker's computer is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BoxMode {
    /// This coworker's own machine. Stopping or destroying it affects nobody else.
    Dedicated,
    /// One machine several coworkers share. Ending it is not one coworker's decision.
    Shared,
}

/// Who may see and talk to a coworker. `Private` is the default and the safe one: a coworker
/// nobody chose to share stays the hirer's alone. `Org` opens it to the hirer's organisation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    #[default]
    Private,
    Org,
}

impl Visibility {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Org => "org",
        }
    }

    /// Parse what a client sent. `None` is a refusal rather than a default: a caller who wrote
    /// "public" meant something we do not offer, and quietly storing "private" would tell them
    /// they had shared a coworker they had not.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "private" => Some(Self::Private),
            "org" => Some(Self::Org),
            _ => None,
        }
    }
}

/// How hard a coworker thinks (#271): the gateway's own `reasoning_effort` words
/// (open-ai-gateway `oag-proto/src/canonical.rs`, `Effort::as_str`), plus `Inherit`, which sends
/// no word at all, and `Ultra`. THE GATEWAY'S LIST, NOT OURS: a word it cannot parse it drops
/// without a refusal, and the coworker would think at its route's default while its row said
/// otherwise. `Ultra` is the one word that is not the gateway's, so a write takes it only where
/// the model's own listing names it (`catalogue::refusal`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    /// No `reasoning_effort` on the request, so the route thinks as hard as it does by default.
    /// THE DEFAULT, because it is what every turn sent before effort existed: a coworker, a view
    /// or a run log written then reads as this and behaves as it did.
    #[default]
    Inherit,
    /// Reasoning off, spelled `none`: a word the gateway is sent like any other, which is not the
    /// same request as sending none.
    #[serde(rename = "none")]
    Off,
    Low,
    Medium,
    High,
    XHigh,
    Max,
    /// opencodex's top rung (2.75.0 lists it on gpt-6-sol and gpt-6.1-sol, not gpt-6-luna): its chat
    /// door takes `reasoning_effort: "ultra"` (`src/chat/inbound.ts`) and sends `max` upstream.
    Ultra,
}

impl Effort {
    /// Every word, in the order a refusal lists them.
    pub const ALL: [Self; 8] = [
        Self::Inherit,
        Self::Off,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
        Self::Ultra,
    ];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inherit => "inherit",
            Self::Off => "none",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
            Self::Ultra => "ultra",
        }
    }

    /// Parse what a client sent, exactly. `None` is a refusal rather than a default, for the
    /// reason `Visibility::parse` gives: storing inherit for "loud" would answer 200 about a
    /// coworker that does not think the way its caller asked. The gateway also reads `minimal`;
    /// it is not a word the client agreed, so it is refused here like any other.
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|effort| effort.as_str() == word)
    }

    /// The `reasoning_effort` a turn sends: none for `Inherit`, so the field is left off.
    #[must_use]
    pub fn reasoning_effort(self) -> Option<&'static str> {
        (self != Self::Inherit).then(|| self.as_str())
    }

    /// An effort a request names, for every door that takes one (a coworker's PATCH, a default
    /// for new bots): absent names none, null is `Inherit`, and anything that is not one of the
    /// words is refused with them listed, for the caller to put its field's name in front of.
    pub fn named(value: Option<&Value>) -> Result<Option<Self>, String> {
        match value {
            None => Ok(None),
            Some(Value::Null) => Ok(Some(Self::Inherit)),
            Some(word) => word
                .as_str()
                .and_then(Self::parse)
                .map(Some)
                .ok_or_else(|| {
                    let words = Self::ALL.map(Self::as_str).join(", ");
                    format!("must be one of {words}")
                }),
        }
    }
}

/// The most a standing role may be. Long enough for a paragraph of intent, short enough that it
/// cannot become a second system prompt smuggled through a text field.
pub const MAX_ROLE_CHARS: usize = 1000;

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
    /// What this coworker is FOR, in the hirer's own words, standing across every conversation.
    /// Its own event rather than a field on `Renamed`, for the reason `Repinned` is its own: a
    /// caller who meant to describe a coworker must not silently rename it. `None` clears it.
    RoleSet {
        role: Option<String>,
        at_ms: i64,
    },
    /// Who may see and talk to this coworker changed.
    VisibilitySet {
        visibility: Visibility,
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
    /// How hard this coworker thinks changed. Its own event for the reason `Repinned` is: a
    /// caller who meant to change how hard it thinks must not also rename or repin it.
    EffortSet {
        effort: Effort,
        at_ms: i64,
    },
    /// Which door this coworker's turns go through changed; `None` is the driving person's own
    /// setting. Its own event for the reason `Repinned` is.
    SourceSet {
        source: Option<SourceKind>,
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
    /// member's model, key, tools and policy (`docs/archive/plan-rooms.md` §2).
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
            Self::EffortSet { .. } => "coworker-effort-set",
            Self::SourceSet { .. } => "coworker-source-set",
            Self::RoleSet { .. } => "coworker-role-set",
            Self::VisibilitySet { .. } => "coworker-visibility-set",
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
    /// Who may see and talk to this coworker. Private unless its owner shared it.
    pub visibility: Visibility,
    /// The standing role, read on the run path on EVERY turn (`server/persona.rs`). On the
    /// aggregate rather than the seam-B profile blob because it is behavioural, not cosmetic:
    /// the blob is where the client's decoration lives, and a field the model reads every turn
    /// is not decoration.
    pub role: Option<String>,
    /// How hard it thinks: sent to the gateway on every turn, so on the aggregate for the reason
    /// the role is. A run captures it at its start (`RunEvent::Started::effort`).
    pub effort: Effort,
    /// Its own door, under the turn's word and over the driving person's setting
    /// (`opengrok_harness::local_proxy::route`). The plan behind `LocalProxy` is always the driving
    /// person's, never its owner's; set here, the pin is the model that plan is asked for, and
    /// anywhere else the person's own `localModel` is. A run captures the door its turn resolved
    /// to (`RunEvent::Started::inference_source`), never this.
    pub source: Option<SourceKind>,
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
    #[error("a role may be at most {MAX_ROLE_CHARS} characters")]
    RoleTooLong,
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
    /// Set or clear the standing role. `None` clears it.
    SetRole {
        role: Option<String>,
        at_ms: i64,
    },
    /// Share this coworker with the hirer's organisation, or take it back.
    SetVisibility {
        visibility: Visibility,
        at_ms: i64,
    },
    /// Point this coworker at a different route. Deliberately its own command: renaming and
    /// repinning are different decisions, and a caller that means one must not do the other.
    Repin {
        model: String,
        at_ms: i64,
    },
    /// Set how hard it thinks; `Inherit` goes back to the route's own default.
    SetEffort {
        effort: Effort,
        at_ms: i64,
    },
    /// Set its own door; `None` goes back to the driving person's setting.
    SetSource {
        source: Option<SourceKind>,
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
            CoworkerEvent::EffortSet { effort, .. } => self.effort = *effort,
            CoworkerEvent::SourceSet { source, .. } => self.source = *source,
            CoworkerEvent::RoleSet { role, .. } => self.role.clone_from(role),
            CoworkerEvent::VisibilitySet { visibility, .. } => self.visibility = *visibility,
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

    /// The box this coworker was FIRST given, so whether it has a computer at all: a run gates its
    /// tools on this, never on a request. Not the box it runs on. A shared box is reset, updated
    /// or taken over under the scope that shares it, and every reader finds the live one through
    /// that scope's record (`provision::scoped_box_for`, #302).
    pub fn computer(&self) -> Option<&BoxId> {
        self.box_id.as_ref()
    }

    /// The box's mode — dedicated (this coworker's own) or shared — or `None` when it has no box.
    pub fn box_mode(&self) -> Option<BoxMode> {
        self.box_mode
    }

    /// A model must be a route somebody could actually be served on. Trimmed, because a pin of
    /// spaces is the same lie as an empty one. Public for the default a hire may be born on
    /// (`NewBotDefault::named`), whose gateway model is held to exactly this.
    pub fn non_blank(model: String) -> Result<String, CoworkerError> {
        let trimmed = model.trim();
        if trimmed.is_empty() {
            return Err(CoworkerError::EmptyModel);
        }
        Ok(trimmed.to_string())
    }

    /// A just-hired coworker put on its owner's default for new bots (#318): the door and the
    /// effort it names, decided as an owner's PATCH decides them and applied here, for the
    /// hire's own append. The default's model is the hire's pin already.
    pub fn born_on(
        &mut self,
        default: &NewBotDefault,
        at_ms: i64,
    ) -> Result<Vec<CoworkerEvent>, CoworkerError> {
        let source = Some(default.source);
        let mut events = self.decide(CoworkerCommand::SetSource { source, at_ms })?;
        let effort = default.effort;
        events.extend(self.decide(CoworkerCommand::SetEffort { effort, at_ms })?);
        for event in &events {
            self.apply(event);
        }
        Ok(events)
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

            CoworkerCommand::SetEffort { effort, at_ms } => {
                self.alive()?;
                Ok(vec![CoworkerEvent::EffortSet { effort, at_ms }])
            }

            CoworkerCommand::SetSource { source, at_ms } => {
                self.alive()?;
                Ok(vec![CoworkerEvent::SourceSet { source, at_ms }])
            }

            CoworkerCommand::Rename { name, at_ms } => {
                self.alive()?;
                Ok(vec![CoworkerEvent::Renamed { name, at_ms }])
            }

            CoworkerCommand::SetRole { role, at_ms } => {
                self.alive()?;
                // Blank and whitespace clear it: the field is nullable and a person who empties
                // the box means "no role", not "a role made of spaces". The bound is enforced
                // here as well as at the door, because an aggregate that accepts a role nothing
                // can render is a broken row nobody refused.
                let role = role
                    .map(|role| role.trim().to_string())
                    .filter(|role| !role.is_empty());
                if let Some(role) = &role
                    && role.chars().count() > MAX_ROLE_CHARS
                {
                    return Err(CoworkerError::RoleTooLong);
                }
                Ok(vec![CoworkerEvent::RoleSet { role, at_ms }])
            }

            CoworkerCommand::SetVisibility { visibility, at_ms } => {
                self.alive()?;
                Ok(vec![CoworkerEvent::VisibilitySet { visibility, at_ms }])
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
    /// `Coworker::computer`: the box it was first given, not the one it runs on (#302).
    pub box_id: Option<BoxId>,
    pub retired: bool,
    /// The client's sort key — see `research/client-grok-bot.md` §8.1.
    pub updated_at_ms: i64,
    /// A group's members; empty for an ordinary coworker. The roster's `isGroup`/`memberIds`.
    #[serde(default)]
    pub members: Vec<CoworkerId>,
    /// The standing role, on the row so the roster carries it without a second read.
    #[serde(default)]
    pub role: Option<String>,
    /// Who may see and talk to it. On the row for the same reason as the role: the roster
    /// decides what to show from it, on every listing.
    #[serde(default)]
    pub visibility: Visibility,
    /// How hard it thinks, on the row so the roster carries it; inherit on a row from before.
    #[serde(default)]
    pub effort: Effort,
    /// Its own door, on the row so the roster carries it; none on a row from before.
    #[serde(default)]
    pub source: Option<SourceKind>,
}

impl CoworkerView {
    /// The row this aggregate projects to. ONE mapping for every writer, because
    /// `append_coworker` rewrites every column: a writer that spelled the row out and missed a
    /// field would reset it, and assigning a computer would quietly put the effort back to inherit.
    #[must_use]
    pub fn of(id: CoworkerId, coworker: &Coworker, updated_at_ms: i64) -> Self {
        Self {
            id,
            name: coworker.name.clone(),
            model: coworker.model.clone(),
            box_id: coworker.box_id.clone(),
            retired: coworker.retired,
            updated_at_ms,
            members: coworker.members.clone(),
            role: coworker.role.clone(),
            visibility: coworker.visibility,
            effort: coworker.effort,
            source: coworker.source,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "../tests/unit/coworker.rs"]
mod tests;
