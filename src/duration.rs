//! The two-unit duration split shared by the console and status page formatters.

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
}
