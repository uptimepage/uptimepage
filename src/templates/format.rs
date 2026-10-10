//! Timestamps and durations the way the pages print them.

use std::fmt;

use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};

use crate::duration::{DurationUnit, duration_parts};

pub fn fmt_ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Human-readable wall-clock UTC string, e.g. "2026-05-13 12:34 UTC".
/// Pair with `fmt_ts` (ISO 8601) for `<time datetime>` round-trips.
pub fn fmt_human(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d %H:%M UTC").to_string()
}

/// Two-unit duration string, e.g. `"45s"`, `"17m"`, `"2h 14m"`, `"1d 1h"`.
/// Negative durations clamp to zero.
pub fn humanize_duration(d: ChronoDuration) -> String {
    HumanDur(d.num_seconds()).to_string()
}

/// Display wrapper for [`humanize_duration`] that writes directly to a
/// `fmt::Formatter` instead of allocating an intermediate `String`. Cheap
/// to construct from the raw seconds the storage layer already returns.
pub struct HumanDur(pub i64);

impl fmt::Display for HumanDur {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ((n, unit), minor) = duration_parts(self.0);
        write!(f, "{n}{}", suffix(unit))?;
        if let Some((n, unit)) = minor {
            write!(f, " {n}{}", suffix(unit))?;
        }
        Ok(())
    }
}

fn suffix(unit: DurationUnit) -> &'static str {
    match unit {
        DurationUnit::Second => "s",
        DurationUnit::Minute => "m",
        DurationUnit::Hour => "h",
        DurationUnit::Day => "d",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanize_duration_writes_short_suffixes() {
        assert_eq!(humanize_duration(ChronoDuration::seconds(0)), "0s");
        assert_eq!(humanize_duration(ChronoDuration::seconds(45)), "45s");
        assert_eq!(humanize_duration(ChronoDuration::minutes(17)), "17m");
        assert_eq!(humanize_duration(ChronoDuration::minutes(134)), "2h 14m");
        assert_eq!(humanize_duration(ChronoDuration::hours(25)), "1d 1h");
        assert_eq!(humanize_duration(ChronoDuration::hours(48)), "2d");
        assert_eq!(humanize_duration(ChronoDuration::seconds(-5)), "0s");
    }
}
