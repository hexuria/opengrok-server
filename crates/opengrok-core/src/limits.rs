//! The limits a person may put on a run, and the one way two levels of them meet.
//!
//! A LEVEL MAY ONLY NARROW WHAT IS ABOVE IT. The server's own budget (`RunBudget`, in the harness)
//! bounds every run; an org's admin may set a ceiling under it, and a routine its own limits under
//! that. `and` is how any two levels meet — per limit, the tighter — so however they are combined,
//! no run can use more than any level that spoke allows (CLAUDE.md #8: a typo may only ever
//! narrow access).
//!
//! THREE AND ONLY THREE: rounds that ended in words or other work, rounds spent on the screen, and
//! the wall clock. A model call's own clocks stay the server's; they catch a door that went quiet,
//! which is not a person's choice to make per run.
//!
//! NEVER ZERO, BY TYPE. A budget of no rounds leaves the loop's `for` with nothing to run, and
//! `formal/lean/Harness.lean` (`Budget.never_falls_out`) proves the loop ends with an ending only
//! for budgets of at least one. A zero cannot be held here, so it cannot be captured on a run.

use std::num::{NonZeroU32, NonZeroU64};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The wire's names for the three, in the order a sentence lists them.
const NAMES: [&str; 3] = ["maxRounds", "maxComputerRounds", "maxWallMs"];

/// One level's limits. `None` says nothing at that level, and the levels above decide.
///
/// TWO SHAPES, ON PURPOSE. In the log it is serde's, snake_case like every event field beside it;
/// on the wire it is the clients' camelCase, written by `to_json` and read by `from_json`. A field
/// renamed for a client must never change what an old log replays as.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunLimits {
    /// Model calls that ended in words or in work other than the screen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rounds: Option<NonZeroU32>,
    /// Model calls spent looking at and acting on the box's screen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_computer_rounds: Option<NonZeroU32>,
    /// Wall clock for the whole run, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_wall_ms: Option<NonZeroU64>,
}

impl RunLimits {
    /// Both levels at once: per limit the tighter of the two, and a limit one leaves unset is the
    /// other's. The only way two levels combine, so no order of combining can widen a run.
    #[must_use]
    pub fn and(self, other: Self) -> Self {
        Self {
            max_rounds: tighter(self.max_rounds, other.max_rounds),
            max_computer_rounds: tighter(self.max_computer_rounds, other.max_computer_rounds),
            max_wall_ms: tighter(self.max_wall_ms, other.max_wall_ms),
        }
    }

    /// Nothing set: this level leaves every limit to the levels above it.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values().iter().all(Option::is_none)
    }

    fn values(&self) -> [Option<u64>; 3] {
        [
            self.max_rounds.map(|rounds| u64::from(rounds.get())),
            self.max_computer_rounds
                .map(|rounds| u64::from(rounds.get())),
            self.max_wall_ms.map(NonZeroU64::get),
        ]
    }

    /// As the wire carries them: every limit named, `null` where this level sets none. Never a
    /// missing key, which a client cannot tell from a server too old to say.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let [rounds, computer, wall] = self.values();
        json!({ "maxRounds": rounds, "maxComputerRounds": computer, "maxWallMs": wall })
    }

    /// What a person sent, or the sentence that says why it cannot be a run's limits. Each limit
    /// is a whole number from 1 to what `most` (the server's own budget) allows, or `null` or
    /// absent for none; `null` for the whole object is no limits at all.
    ///
    /// A NAME THAT IS NOT A LIMIT IS REFUSED, NOT SKIPPED. `maxRound` skipped would leave the run
    /// with no limit where its sender meant one, the one direction a typo must never go. Above
    /// the server's own budget is refused too: accepted, it would be a limit that never binds.
    pub fn from_json(sent: &Value, most: &Self) -> Result<Self, String> {
        let fields = match sent {
            Value::Null => return Ok(Self::default()),
            Value::Object(fields) => fields,
            _ => {
                return Err(
                    "run limits are an object of maxRounds, maxComputerRounds and maxWallMs"
                        .to_string(),
                );
            }
        };
        if let Some(other) = fields.keys().find(|key| !NAMES.contains(&key.as_str())) {
            return Err(format!(
                "{other} is not a run limit; the limits are maxRounds, maxComputerRounds and \
                 maxWallMs"
            ));
        }
        let rounds_most = u64::from(u32::MAX);
        let [rounds, computer, wall] = most.values();
        let read = |name: &str, top: Option<u64>, type_most: u64| {
            let top = top.unwrap_or(type_most).min(type_most);
            match fields.get(name) {
                None | Some(Value::Null) => Ok(None),
                Some(value) => value
                    .as_u64()
                    .filter(|asked| (1..=top).contains(asked))
                    .map(Some)
                    .ok_or_else(|| {
                        format!(
                            "{name} must be a whole number from 1 to {top}, the most this server \
                             lets a run use"
                        )
                    }),
            }
        };
        let as_rounds = |asked: Option<u64>| {
            asked
                .and_then(|asked| u32::try_from(asked).ok())
                .and_then(NonZeroU32::new)
        };
        Ok(Self {
            max_rounds: as_rounds(read("maxRounds", rounds, rounds_most)?),
            max_computer_rounds: as_rounds(read("maxComputerRounds", computer, rounds_most)?),
            max_wall_ms: read("maxWallMs", wall, u64::MAX)?.and_then(NonZeroU64::new),
        })
    }

    /// The first limit here above the org's `ceiling`, as the sentence that refuses it — naming
    /// the ceiling, so the person knows what they may ask for instead. `None` when every limit
    /// here fits under it, as each does where the ceiling sets nothing.
    #[must_use]
    pub fn over_ceiling(&self, ceiling: &Self) -> Option<String> {
        NAMES
            .iter()
            .zip(self.values())
            .zip(ceiling.values())
            .find_map(|((name, asked), most)| {
                let (asked, most) = (asked?, most?);
                (asked > most).then(|| {
                    format!(
                        "{name} {asked} is above your organization's ceiling of {most}; set \
                         {most} or less, or ask its admin to raise the ceiling"
                    )
                })
            })
    }
}

fn tighter<T: Ord>(one: Option<T>, other: Option<T>) -> Option<T> {
    match (one, other) {
        (Some(one), Some(other)) => Some(one.min(other)),
        (one, other) => one.or(other),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    fn limits(rounds: Option<u32>, computer: Option<u32>, wall: Option<u64>) -> RunLimits {
        RunLimits {
            max_rounds: rounds.and_then(NonZeroU32::new),
            max_computer_rounds: computer.and_then(NonZeroU32::new),
            max_wall_ms: wall.and_then(NonZeroU64::new),
        }
    }

    fn server() -> RunLimits {
        limits(Some(8), Some(24), Some(900_000))
    }

    /// Per limit the tighter, and a limit left unset is the other level's: the server's default,
    /// an org's ceiling and a routine's own meet in any order and never widen a run.
    #[test]
    fn two_levels_meet_at_the_tighter_of_each_limit() {
        let org = limits(Some(6), None, Some(600_000));
        let routine = limits(Some(10), Some(12), None);
        let run = server().and(org).and(routine);
        assert_eq!(run, limits(Some(6), Some(12), Some(600_000)));
        assert_eq!(
            run,
            routine.and(org).and(server()),
            "the order does not matter"
        );
        assert_eq!(
            org.and(RunLimits::default()),
            org,
            "an empty level says nothing"
        );
        assert_eq!(
            RunLimits::default().and(RunLimits::default()),
            RunLimits::default()
        );
        assert!(
            RunLimits::default().is_empty() && !org.is_empty(),
            "only a level that sets nothing is empty"
        );
    }

    #[test]
    fn what_a_person_sends_is_read_with_nulls_as_none() {
        let sent = json!({ "maxRounds": 3, "maxComputerRounds": null, "maxWallMs": 60000 });
        assert_eq!(
            RunLimits::from_json(&sent, &server()),
            Ok(limits(Some(3), None, Some(60_000)))
        );
        assert_eq!(
            RunLimits::from_json(&Value::Null, &server()),
            Ok(RunLimits::default())
        );
        assert_eq!(
            RunLimits::from_json(&json!({}), &server()),
            Ok(RunLimits::default())
        );
        assert_eq!(
            RunLimits::from_json(&json!({ "maxRounds": 8 }), &server()),
            Ok(limits(Some(8), None, None)),
            "the server's own budget is itself allowed"
        );
    }

    /// Zero and nonsense are refused where they are written, with a sentence that names the limit
    /// and what it may be; a misspelt name is refused rather than dropped.
    #[test]
    fn zero_nonsense_and_unknown_names_are_refused_with_a_sentence() {
        for (sent, says) in [
            (
                json!({ "maxRounds": 0 }),
                "maxRounds must be a whole number from 1 to 8",
            ),
            (json!({ "maxRounds": -1 }), "maxRounds must be"),
            (json!({ "maxRounds": 2.5 }), "maxRounds must be"),
            (
                json!({ "maxComputerRounds": "3" }),
                "maxComputerRounds must be",
            ),
            (json!({ "maxComputerRounds": 25 }), "from 1 to 24"),
            (json!({ "maxWallMs": 900_001 }), "from 1 to 900000"),
            (json!({ "maxRound": 3 }), "maxRound is not a run limit"),
            (
                json!({ "callTimeoutMs": 1000 }),
                "callTimeoutMs is not a run limit",
            ),
            (json!([3]), "an object of maxRounds"),
            (json!(3), "an object of maxRounds"),
        ] {
            let refused = RunLimits::from_json(&sent, &server()).expect_err(&sent.to_string());
            assert!(refused.contains(says), "{sent}: {refused}");
        }
    }

    #[test]
    fn the_wire_names_every_limit_and_nulls_what_is_unset() {
        assert_eq!(
            limits(Some(3), None, Some(60_000)).to_json(),
            json!({ "maxRounds": 3, "maxComputerRounds": null, "maxWallMs": 60000 })
        );
        assert_eq!(
            RunLimits::default().to_json(),
            json!({ "maxRounds": null, "maxComputerRounds": null, "maxWallMs": null })
        );
    }

    #[test]
    fn a_limit_above_the_ceiling_is_refused_naming_the_ceiling() {
        let ceiling = limits(Some(6), None, Some(600_000));
        let refused = limits(Some(7), Some(50), None)
            .over_ceiling(&ceiling)
            .expect("7 is above 6");
        assert!(
            refused.contains("maxRounds 7") && refused.contains("ceiling of 6"),
            "{refused}"
        );
        assert_eq!(limits(Some(6), Some(50), None).over_ceiling(&ceiling), None);
        assert_eq!(
            limits(None, None, Some(600_001))
                .over_ceiling(&ceiling)
                .map(|why| why.contains("maxWallMs 600001")),
            Some(true)
        );
        assert_eq!(
            server().over_ceiling(&RunLimits::default()),
            None,
            "a ceiling that sets nothing refuses nothing"
        );
    }

    /// The log's shape: snake_case, unset limits left out, and a zero no log can hold.
    #[test]
    fn the_log_shape_round_trips_and_cannot_hold_a_zero() {
        let set = limits(Some(3), None, Some(60_000));
        let stored = serde_json::to_value(set).unwrap();
        assert_eq!(stored, json!({ "max_rounds": 3, "max_wall_ms": 60000 }));
        assert_eq!(serde_json::from_value::<RunLimits>(stored).unwrap(), set);
        assert_eq!(
            serde_json::from_value::<RunLimits>(json!({})).unwrap(),
            RunLimits::default()
        );
        assert!(serde_json::from_value::<RunLimits>(json!({ "max_rounds": 0 })).is_err());
    }
}
