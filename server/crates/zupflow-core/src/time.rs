//! Instants: milliseconds since the Unix epoch, UTC. Stored as that integer,
//! shown in the API as ISO 8601 with milliseconds and a `Z`, the way the
//! contract's `Instant` is written: `2026-10-05T12:34:56.789Z`.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Serialize, Serializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Timestamp(i64);

impl Timestamp {
    pub fn now() -> Self {
        Self(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX)),
        )
    }

    /// From a stored value.
    pub fn from_millis(millis: i64) -> Self {
        Self(millis)
    }

    pub fn millis(self) -> i64 {
        self.0
    }
}

/// Days since 1970-01-01 to a civil date (proleptic Gregorian), after
/// Howard Hinnant's `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    // Both are in range by construction: day 1..=31, month 1..=12.
    (year, u32::try_from(month).unwrap_or(1), u32::try_from(day).unwrap_or(1))
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let days = self.0.div_euclid(86_400_000);
        let in_day = self.0.rem_euclid(86_400_000);
        let (year, month, day) = civil_from_days(days);
        let (hour, minute) = (in_day / 3_600_000, in_day / 60_000 % 60);
        let (second, milli) = (in_day / 1_000 % 60, in_day % 1_000);
        write!(
            f,
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{milli:03}Z"
        )
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_the_contract_format() {
        assert_eq!(Timestamp::from_millis(0).to_string(), "1970-01-01T00:00:00.000Z");
        // 2026-10-05T12:34:56.789Z
        assert_eq!(
            Timestamp::from_millis(1_791_203_696_789).to_string(),
            "2026-10-05T12:34:56.789Z"
        );
        // A leap day, and the last millisecond of a year.
        assert_eq!(
            Timestamp::from_millis(951_782_400_000).to_string(),
            "2000-02-29T00:00:00.000Z"
        );
        assert_eq!(
            Timestamp::from_millis(1_767_225_599_999).to_string(),
            "2025-12-31T23:59:59.999Z"
        );
        assert_eq!(
            serde_json::to_value(Timestamp::from_millis(0)).expect("serializes"),
            serde_json::json!("1970-01-01T00:00:00.000Z")
        );
    }
}
