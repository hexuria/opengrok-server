//! Giving a freshly-hired coworker its own computer — shared by every create path.
//!
//! Factored out of the REST hire so the gateway `createAgent` and seam-B `CreateGrokBotAgent`
//! provision boxes identically: create a dedicated box (or reference a shared one), assign it on
//! the coworker, and report a failure WITHOUT failing the hire — a boxless coworker is still a
//! coworker, and the person can be told why the box is missing rather than losing the coworker.
//!
//! The box is created here at hire time; it is torn down by the retire path (see the coworker
//! release flow). No create path may ship without its teardown — a leaked box is a bill (ascii) or
//! a dangling container (docker).

use std::sync::Arc;

use opengrok_box::Computer;
use opengrok_core::coworker::{BoxMode, Coworker, CoworkerCommand, CoworkerEvent};
use opengrok_core::id::BoxId;

/// What a create path asked for regarding a computer.
#[derive(Debug, Default, Clone)]
pub struct ComputerWish {
    /// Provision a dedicated box of this coworker's own.
    pub with_computer: bool,
    /// Attach to a named existing box instead of creating one ("shared" — not created per coworker).
    pub shared_box_id: Option<String>,
}

impl ComputerWish {
    fn asked(&self) -> bool {
        self.with_computer || self.shared_box_id.is_some()
    }
}

/// The outcome of a provisioning attempt.
pub struct Provisioned {
    /// The `ComputerAssigned` events to persist alongside the hire (empty when none/failed).
    pub events: Vec<CoworkerEvent>,
    /// The assigned box id, for the coworker view (`None` when none/failed).
    pub box_id: Option<BoxId>,
    /// A client-readable reason the box could not be given — never fatal to the hire.
    pub error: Option<String>,
}

/// Give `coworker` a computer if `wish` asked for one. Applies the assignment events to `coworker`
/// so its state reflects the box; returns the events (to persist), the box id (for the view), and
/// any error. Never returns `Err`: a provisioning failure is reported, not raised, so the hire
/// stands.
pub async fn provision_computer(
    computer: Option<&Arc<dyn Computer>>,
    coworker: &mut Coworker,
    wish: &ComputerWish,
    at_ms: i64,
) -> Provisioned {
    if !wish.asked() {
        return Provisioned {
            events: Vec::new(),
            box_id: None,
            error: None,
        };
    }

    // A shared box is named, not created — making one per coworker is exactly what "shared" is not.
    let assignment = match (&wish.shared_box_id, computer) {
        (Some(box_id), _) => Ok((BoxId::from_stored(box_id.clone()), BoxMode::Shared)),
        (None, Some(computer)) => computer
            .create(None)
            .await
            .map(|id| (BoxId::from_stored(id), BoxMode::Dedicated))
            .map_err(|error| error.to_string()),
        (None, None) => Err("this server has no computer provider configured".to_string()),
    };

    match assignment {
        Ok((box_id, mode)) => match coworker.decide(CoworkerCommand::AssignComputer {
            box_id: box_id.clone(),
            mode,
            at_ms,
        }) {
            Ok(events) => {
                for event in &events {
                    coworker.apply(event);
                }
                Provisioned {
                    events,
                    box_id: Some(box_id),
                    error: None,
                }
            }
            Err(error) => Provisioned {
                events: Vec::new(),
                box_id: None,
                error: Some(error.to_string()),
            },
        },
        Err(error) => Provisioned {
            events: Vec::new(),
            box_id: None,
            error: Some(error),
        },
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use opengrok_core::coworker::CoworkerCommand;

    fn hired() -> Coworker {
        let events = Coworker::default()
            .decide(CoworkerCommand::Hire {
                name: "Bot".to_string(),
                model: "oag/cheap".to_string(),
                at_ms: 1,
            })
            .expect("hire");
        let mut coworker = Coworker::default();
        for event in &events {
            coworker.apply(event);
        }
        coworker
    }

    #[tokio::test]
    async fn no_wish_is_a_no_op() {
        let mut coworker = hired();
        let out = provision_computer(None, &mut coworker, &ComputerWish::default(), 2).await;
        assert!(out.events.is_empty());
        assert!(out.box_id.is_none());
        assert!(out.error.is_none());
        assert!(coworker.computer().is_none());
    }

    #[tokio::test]
    async fn dedicated_without_a_provider_reports_an_error_not_a_box() {
        let mut coworker = hired();
        let wish = ComputerWish {
            with_computer: true,
            shared_box_id: None,
        };
        let out = provision_computer(None, &mut coworker, &wish, 2).await;
        assert!(out.box_id.is_none());
        assert!(coworker.computer().is_none());
        assert!(
            out.error
                .as_deref()
                .unwrap_or_default()
                .contains("no computer provider")
        );
    }

    #[tokio::test]
    async fn a_shared_box_is_referenced_without_creating_one() {
        let mut coworker = hired();
        let wish = ComputerWish {
            with_computer: false,
            shared_box_id: Some("box_shared".to_string()),
        };
        let out = provision_computer(None, &mut coworker, &wish, 2).await;
        assert!(out.error.is_none());
        assert_eq!(out.box_id.as_ref().map(BoxId::as_str), Some("box_shared"));
        assert_eq!(coworker.computer().map(BoxId::as_str), Some("box_shared"));
        assert!(!out.events.is_empty());
    }
}
