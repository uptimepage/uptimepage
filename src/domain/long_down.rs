use std::time::Duration;

use crate::duration::{DurationUnit, duration_parts};

/// When a monitor failing from every region counts as long-down, and how often
/// it is checked while it stays that way.
pub struct LongDown {
    pub after: Duration,
    pub interval: Duration,
}

pub const LONG_DOWN: LongDown = LongDown {
    after: Duration::from_secs(3 * 86_400),
    interval: Duration::from_secs(15 * 60),
};

impl LongDown {
    /// The rate a slowed monitor runs at: never faster than its own.
    pub fn slowed_interval(&self, own: Duration) -> Duration {
        own.max(self.interval)
    }

    pub fn after_words(&self) -> String {
        words(self.after)
    }

    pub fn interval_words(&self) -> String {
        words(self.interval)
    }
}

/// Prose form for copy, such as "3 days" or "1 hour 30 minutes".
fn words(d: Duration) -> String {
    let ((n, unit), minor) = duration_parts(i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    let mut out = unit_words(n, unit);
    if let Some((n, unit)) = minor {
        out.push(' ');
        out.push_str(&unit_words(n, unit));
    }
    out
}

fn unit_words(n: i64, unit: DurationUnit) -> String {
    let name = match unit {
        DurationUnit::Second => "second",
        DurationUnit::Minute => "minute",
        DurationUnit::Hour => "hour",
        DurationUnit::Day => "day",
    };
    if n == 1 {
        format!("1 {name}")
    } else {
        format!("{n} {name}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_as_prose() {
        assert_eq!(words(Duration::from_secs(3 * 86_400)), "3 days");
        assert_eq!(words(Duration::from_secs(15 * 60)), "15 minutes");
        assert_eq!(words(Duration::from_secs(3_600)), "1 hour");
        assert_eq!(words(Duration::from_secs(5_400)), "1 hour 30 minutes");
    }
}
