//! Timestamps and durations the way the pages print them.

use std::fmt;

use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};

pub fn fmt_ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Human-readable wall-clock UTC string, e.g. "2026-05-13 12:34 UTC".
/// Pair with `fmt_ts` (ISO 8601) for `<time datetime>` round-trips.
pub fn fmt_human(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d %H:%M UTC").to_string()
}

/// Exact single-unit duration (`45s`, `5m`, `24h`, `30d`) for config values
/// that must round-trip — the lossy two-unit display lives in [`HumanDur`].
/// Days start at two, so a 24h interval stays hours.
pub fn exact_duration(secs: u64) -> String {
    if secs >= 172_800 && secs.is_multiple_of(86_400) {
        format!("{}d", secs / 86_400)
    } else if secs.is_multiple_of(3_600) {
        format!("{}h", secs / 3_600)
    } else if secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
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
        let total = self.0.max(0);
        if total < 60 {
            return write!(f, "{total}s");
        }
        let mins = total / 60;
        if mins < 60 {
            return write!(f, "{mins}m");
        }
        let hours = mins / 60;
        let rem_mins = mins % 60;
        if hours < 24 {
            if rem_mins == 0 {
                return write!(f, "{hours}h");
            }
            return write!(f, "{hours}h {rem_mins}m");
        }
        let days = hours / 24;
        let rem_hours = hours % 24;
        if rem_hours == 0 {
            write!(f, "{days}d")
        } else {
            write!(f, "{days}d {rem_hours}h")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanize_duration_picks_largest_unit() {
        assert_eq!(humanize_duration(ChronoDuration::seconds(0)), "0s");
        assert_eq!(humanize_duration(ChronoDuration::seconds(45)), "45s");
        assert_eq!(humanize_duration(ChronoDuration::minutes(17)), "17m");
        assert_eq!(humanize_duration(ChronoDuration::minutes(134)), "2h 14m");
        assert_eq!(humanize_duration(ChronoDuration::hours(25)), "1d 1h");
        assert_eq!(humanize_duration(ChronoDuration::hours(48)), "2d");
        assert_eq!(humanize_duration(ChronoDuration::seconds(-5)), "0s");
    }

    #[test]
    fn exact_duration_reaches_days_without_moving_a_daily_interval() {
        assert_eq!(exact_duration(45), "45s");
        assert_eq!(exact_duration(300), "5m");
        assert_eq!(exact_duration(4_980), "83m");
        assert_eq!(exact_duration(86_400), "24h");
        assert_eq!(exact_duration(172_800), "2d");
        assert_eq!(exact_duration(2_592_000), "30d");
    }

    /// The form renders a stored duration with this, and `parseDuration` in
    /// check_form.js reads it back on submit. Anything outside the grammar that
    /// regex accepts would come back as a validation error on a field the
    /// customer never touched.
    #[test]
    fn every_rendered_duration_matches_the_grammar_the_form_parses() {
        for secs in [
            0, 1, 45, 59, 60, 90, 300, 4_980, 3_600, 43_200, 86_400, 172_800, 2_592_000,
        ] {
            let rendered = exact_duration(secs);
            let (digits, unit) = rendered.split_at(rendered.len() - 1);
            assert!(
                digits.chars().all(|c| c.is_ascii_digit()) && !digits.is_empty(),
                "{rendered} is not digits followed by a unit"
            );
            assert!(
                ["s", "m", "h", "d"].contains(&unit),
                "{rendered} ends in an unparseable unit"
            );
        }
    }
}
