//! How a duration breaks into units: the two-unit split the console and status
//! page formatters share, and the exact single-unit form a config value
//! round-trips through.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DurationUnit {
    Second,
    Minute,
    Hour,
    Day,
}

/// The largest unit of a duration, plus the next one down when non-zero.
/// Negative durations clamp to zero.
pub(crate) fn duration_parts(secs: i64) -> ((i64, DurationUnit), Option<(i64, DurationUnit)>) {
    let total = secs.max(0);
    if total < 60 {
        return ((total, DurationUnit::Second), None);
    }
    let mins = total / 60;
    if mins < 60 {
        return ((mins, DurationUnit::Minute), None);
    }
    let (hours, rem_mins) = (mins / 60, mins % 60);
    if hours < 24 {
        return (
            (hours, DurationUnit::Hour),
            (rem_mins != 0).then_some((rem_mins, DurationUnit::Minute)),
        );
    }
    let (days, rem_hours) = (hours / 24, hours % 24);
    (
        (days, DurationUnit::Day),
        (rem_hours != 0).then_some((rem_hours, DurationUnit::Hour)),
    )
}

/// Exact single-unit duration (`45s`, `5m`, `24h`, `30d`) for config values
/// that must round-trip; the lossy two-unit display is `HumanDur`.
/// Days start at two, so a 24h interval stays hours.
pub(crate) fn exact_duration(secs: u64) -> String {
    if secs == 0 {
        "0s".to_owned()
    } else if secs >= 172_800 && secs.is_multiple_of(86_400) {
        format!("{}d", secs / 86_400)
    } else if secs.is_multiple_of(3_600) {
        format!("{}h", secs / 3_600)
    } else if secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_parts_switch_units_at_each_boundary() {
        use DurationUnit::{Day, Hour, Minute, Second};
        let cases = [
            (-5, (0, Second), None),
            (59, (59, Second), None),
            (60, (1, Minute), None),
            (3_599, (59, Minute), None),
            (3_600, (1, Hour), None),
            (3_660, (1, Hour), Some((1, Minute))),
            (86_399, (23, Hour), Some((59, Minute))),
            (86_400, (1, Day), None),
            (90_000, (1, Day), Some((1, Hour))),
        ];
        for (secs, major, minor) in cases {
            assert_eq!(duration_parts(secs), (major, minor), "{secs}s");
        }
    }

    #[test]
    fn exact_duration_reaches_days_without_moving_a_daily_interval() {
        assert_eq!(exact_duration(0), "0s");
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
