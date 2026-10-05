//! Float → [`Confidence`] conversion (PUA spec §5 rule 2). The only module in PUA allowed float
//! arithmetic; the workspace denies `clippy::float_arithmetic` everywhere else.
#![allow(clippy::float_arithmetic)]

use pua_core::Confidence;

/// Above this a noul answer reads as yes. Exactly a half is a coin; calling it yes is arbitrary,
/// which is why the confidence beside it is the number a caller should act on
/// (`opengrok-server src/jev/routes.rs`).
pub const YES_ABOVE: f64 = 0.5;

/// Why a float could not become a [`Confidence`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ConvertError {
    /// NaN or ±∞.
    NonFinite,
    /// Outside the unit interval `[0, 1]`.
    OutOfRange,
}

impl core::fmt::Display for ConvertError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NonFinite => f.write_str("probability is not finite"),
            Self::OutOfRange => f.write_str("probability is outside [0, 1]"),
        }
    }
}

impl std::error::Error for ConvertError {}

/// Converts a unit-interval probability/confidence to millis. Rounds half away from zero.
///
/// # Errors
/// [`ConvertError::NonFinite`] or [`ConvertError::OutOfRange`].
pub fn confidence_from_unit(p: f64) -> Result<Confidence, ConvertError> {
    if !p.is_finite() {
        return Err(ConvertError::NonFinite);
    }
    if !(0.0..=1.0).contains(&p) {
        return Err(ConvertError::OutOfRange);
    }
    // Round half away from zero into 0..=1000.
    let millis = (p * 1000.0).round();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let v = millis as i16;
    Confidence::new(v).map_err(|_| ConvertError::OutOfRange)
}

/// The noul reading and its confidence: yes when `noul >= YES_ABOVE`, confidence is the
/// probability of the chosen side (`noul` if yes, `1 − noul` if no).
///
/// # Errors
/// Same as [`confidence_from_unit`] for the derived confidence.
pub fn noul_reading(noul: f64) -> Result<(bool, Confidence), ConvertError> {
    if !noul.is_finite() {
        return Err(ConvertError::NonFinite);
    }
    if !(0.0..=1.0).contains(&noul) {
        return Err(ConvertError::OutOfRange);
    }
    let yes = noul >= YES_ABOVE;
    let conf = if yes { noul } else { 1.0 - noul };
    Ok((yes, confidence_from_unit(conf)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundaries_and_rejects() {
        assert_eq!(confidence_from_unit(0.0).unwrap().get(), 0);
        assert_eq!(confidence_from_unit(1.0).unwrap().get(), 1000);
        assert_eq!(confidence_from_unit(0.5).unwrap().get(), 500);
        assert_eq!(confidence_from_unit(0.8204).unwrap().get(), 820);
        assert_eq!(confidence_from_unit(0.8205).unwrap().get(), 821);
        assert_eq!(confidence_from_unit(f64::NAN), Err(ConvertError::NonFinite));
        assert_eq!(
            confidence_from_unit(f64::INFINITY),
            Err(ConvertError::NonFinite)
        );
        assert_eq!(confidence_from_unit(-0.01), Err(ConvertError::OutOfRange));
        assert_eq!(confidence_from_unit(1.01), Err(ConvertError::OutOfRange));
    }

    #[test]
    fn noul_half_is_yes_with_half_confidence() {
        let (yes, c) = noul_reading(0.5).unwrap();
        assert!(yes);
        assert_eq!(c.get(), 500);
        let (yes, c) = noul_reading(0.2).unwrap();
        assert!(!yes);
        assert_eq!(c.get(), 800);
        let (yes, c) = noul_reading(0.9).unwrap();
        assert!(yes);
        assert_eq!(c.get(), 900);
    }
}
