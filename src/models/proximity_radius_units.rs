//! Proximity radius unit enum.
//!
//! Maps directly to the Google Ads API `ProximityInfo.radius_units` enum
//! strings. The field is **required** on every proximity criterion — omitting
//! it fails with `fieldError: REQUIRED`, verified live against v25.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Unit for a proximity criterion's radius.
///
/// Google enforces a different range per unit, verified live against v25 with
/// `validateOnly`: anything outside it fails with `INVALID_PROXIMITY_RADIUS`.
///
/// - `KILOMETERS`: 1 to 800.
/// - `MILES`: 1 to 500.
///
/// Note the minimum is a whole 1 in both units — 0.99 km is rejected, so the
/// tight "walking distance" radius some advertisers reach for does not exist.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ProximityRadiusUnits {
    /// Radius expressed in kilometres (1–800).
    #[default]
    Kilometers,
    /// Radius expressed in miles (1–500).
    Miles,
}

impl ProximityRadiusUnits {
    /// String representation matching the Google Ads REST API enum value.
    pub fn as_api_str(&self) -> &'static str {
        match self {
            ProximityRadiusUnits::Kilometers => "KILOMETERS",
            ProximityRadiusUnits::Miles => "MILES",
        }
    }

    /// Inclusive `(min, max)` radius Google accepts for this unit.
    pub fn radius_bounds(&self) -> (f64, f64) {
        match self {
            ProximityRadiusUnits::Kilometers => (1.0, 800.0),
            ProximityRadiusUnits::Miles => (1.0, 500.0),
        }
    }
}

impl std::fmt::Display for ProximityRadiusUnits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_api_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_screaming_snake_case() {
        assert_eq!(
            serde_json::to_string(&ProximityRadiusUnits::Kilometers).unwrap(),
            "\"KILOMETERS\""
        );
        assert_eq!(
            serde_json::to_string(&ProximityRadiusUnits::Miles).unwrap(),
            "\"MILES\""
        );
    }

    #[test]
    fn deserializes_screaming_snake_case() {
        let km: ProximityRadiusUnits = serde_json::from_str("\"KILOMETERS\"").unwrap();
        assert_eq!(km, ProximityRadiusUnits::Kilometers);
        let mi: ProximityRadiusUnits = serde_json::from_str("\"MILES\"").unwrap();
        assert_eq!(mi, ProximityRadiusUnits::Miles);
    }

    #[test]
    fn rejects_unknown_variant() {
        let r: Result<ProximityRadiusUnits, _> = serde_json::from_str("\"METERS\"");
        assert!(r.is_err());
    }

    #[test]
    fn bounds_match_the_live_api() {
        assert_eq!(
            ProximityRadiusUnits::Kilometers.radius_bounds(),
            (1.0, 800.0)
        );
        assert_eq!(ProximityRadiusUnits::Miles.radius_bounds(), (1.0, 500.0));
    }

    #[test]
    fn defaults_to_kilometers() {
        assert_eq!(
            ProximityRadiusUnits::default(),
            ProximityRadiusUnits::Kilometers
        );
    }
}
