//! UTC timestamps with a stable RFC3339 string representation.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// A UTC instant, serialized as RFC3339 with second-or-finer precision.
///
/// Stored internally as whole nanoseconds since the Unix epoch so that ordering, arithmetic and
/// equality are exact and platform independent.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Timestamp(i128);

impl Timestamp {
    /// The Unix epoch.
    pub const EPOCH: Timestamp = Timestamp(0);

    /// Construct from whole nanoseconds since the Unix epoch.
    pub const fn from_unix_nanos(nanos: i128) -> Self {
        Timestamp(nanos)
    }

    /// Construct from whole seconds since the Unix epoch.
    pub const fn from_unix_seconds(secs: i64) -> Self {
        Timestamp(secs as i128 * 1_000_000_000)
    }

    /// Whole nanoseconds since the Unix epoch.
    pub const fn unix_nanos(self) -> i128 {
        self.0
    }

    /// Whole seconds since the Unix epoch, truncated toward negative infinity.
    pub fn unix_seconds(self) -> i64 {
        self.0.div_euclid(1_000_000_000) as i64
    }

    /// Add whole seconds.
    pub const fn plus_seconds(self, secs: i64) -> Self {
        Timestamp(self.0 + secs as i128 * 1_000_000_000)
    }

    /// Add whole milliseconds.
    pub const fn plus_millis(self, millis: i64) -> Self {
        Timestamp(self.0 + millis as i128 * 1_000_000)
    }

    /// Whole seconds elapsed from `earlier` to `self`; negative if `self` precedes `earlier`.
    pub fn seconds_since(self, earlier: Timestamp) -> i64 {
        ((self.0 - earlier.0) / 1_000_000_000) as i64
    }

    /// Whole milliseconds elapsed from `earlier` to `self`.
    pub fn millis_since(self, earlier: Timestamp) -> i64 {
        ((self.0 - earlier.0) / 1_000_000) as i64
    }

    /// Render as RFC3339 in UTC.
    pub fn to_rfc3339(self) -> String {
        let dt = OffsetDateTime::from_unix_timestamp_nanos(self.0)
            .unwrap_or(OffsetDateTime::UNIX_EPOCH)
            .to_offset(time::UtcOffset::UTC);
        dt.format(&Rfc3339).unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
    }

    /// Parse an RFC3339 timestamp.
    pub fn parse_rfc3339(s: &str) -> Result<Self, ParseTimestampError> {
        let dt = OffsetDateTime::parse(s, &Rfc3339).map_err(|_| ParseTimestampError(s.to_string()))?;
        Ok(Timestamp(dt.unix_timestamp_nanos()))
    }
}

/// Failure to parse an RFC3339 timestamp.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a valid RFC3339 timestamp: {0}")]
pub struct ParseTimestampError(pub String);

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_rfc3339())
    }
}

impl fmt::Debug for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Timestamp({})", self.to_rfc3339())
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_rfc3339())
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Timestamp::parse_rfc3339(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_roundtrip() {
        let t = Timestamp::from_unix_seconds(1_700_000_000);
        let s = t.to_rfc3339();
        assert_eq!(s, "2023-11-14T22:13:20Z");
        assert_eq!(Timestamp::parse_rfc3339(&s).unwrap(), t);
    }

    #[test]
    fn arithmetic_is_exact() {
        let t = Timestamp::from_unix_seconds(100);
        assert_eq!(t.plus_seconds(23).seconds_since(t), 23);
        assert_eq!(t.plus_millis(2500).millis_since(t), 2500);
        assert_eq!(t.plus_seconds(-10).seconds_since(t), -10);
    }

    #[test]
    fn serde_is_string_shaped() {
        let t = Timestamp::from_unix_seconds(0);
        assert_eq!(serde_json::to_string(&t).unwrap(), "\"1970-01-01T00:00:00Z\"");
        let back: Timestamp = serde_json::from_str("\"1970-01-01T00:00:00Z\"").unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn ordering_matches_time() {
        assert!(Timestamp::from_unix_seconds(1) < Timestamp::from_unix_seconds(2));
    }
}
