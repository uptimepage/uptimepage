use chrono::{DateTime, DurationRound, TimeDelta, Utc};
use clickhouse::query::Query;

use crate::domain::CheckStatus;
use crate::storage::traits::rollup_bucket_secs;

use super::{TABLE, to_unix_secs};

/// Raw results answer ranges at most this old, so a long range costs O(hours)
/// on the hour rollup instead of O(checks).
const RAW_READ_DAYS: u16 = 30;

/// Where a range's per-check aggregates come from. Both sources yield the same
/// aggregates, so a reader swaps only these SQL fragments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Source {
    Raw,
    Hour,
}

impl Source {
    /// `raw_days` is the org's raw retention: raw rows older than it are gone
    /// while the hour rollup still holds them.
    pub(super) fn for_range(from: DateTime<Utc>, raw_days: u16) -> Self {
        let days = raw_days.min(RAW_READ_DAYS);
        if from >= Utc::now() - chrono::Duration::days(i64::from(days)) {
            Self::Raw
        } else {
            Self::Hour
        }
    }

    pub(super) fn table(self) -> &'static str {
        match self {
            Self::Raw => TABLE,
            Self::Hour => "check_results_1h",
        }
    }

    pub(super) fn time(self) -> &'static str {
        match self {
            Self::Raw => "timestamp",
            Self::Hour => "hour",
        }
    }

    /// `[from, to)` over [`Self::time`]; bind with [`Self::bind_window`].
    pub(super) fn window(self) -> String {
        let t = self.time();
        format!("{t} >= fromUnixTimestamp(?) AND {t} < fromUnixTimestamp(?)")
    }

    /// The two binds [`Self::window`] expects; call after the leading ones.
    pub(super) fn bind_window(self, q: Query, from: DateTime<Utc>, to: DateTime<Utc>) -> Query {
        q.bind(to_unix_secs(self.window_start(from, to)))
            .bind(to_unix_secs(to))
    }

    /// An hour row counts in the range its start falls in, so adjacent ranges
    /// never share one. A range that holds no hour start reads the hour it
    /// falls in instead of nothing; an empty range stays empty.
    fn window_start(self, from: DateTime<Utc>, to: DateTime<Utc>) -> DateTime<Utc> {
        let hour = from.duration_trunc(TimeDelta::hours(1)).unwrap_or(from);
        match self {
            Self::Hour if from < to && hour + TimeDelta::hours(1) >= to => hour,
            _ => from,
        }
    }

    /// `requested` rounded up to whole minutes, or to whole hours on the hour
    /// rollup, where a narrower bucket would mostly hold no row.
    pub(super) fn bucket_width(self, requested: u32) -> u32 {
        match self {
            Self::Raw => rollup_bucket_secs(requested),
            Self::Hour => requested.clamp(3600, u32::MAX / 3600 * 3600).div_ceil(3600) * 3600,
        }
    }

    /// Start of the `secs`-wide bucket each row falls in.
    pub(super) fn bucket(self, secs: u32) -> String {
        format!(
            "toUInt32(toStartOfInterval({}, INTERVAL {secs} SECOND))",
            self.time()
        )
    }

    pub(super) fn total(self) -> &'static str {
        match self {
            Self::Raw => "count()",
            Self::Hour => "countMerge(total_checks)",
        }
    }

    pub(super) fn checks(self, status: CheckStatus) -> String {
        let s = status.as_str();
        match self {
            Self::Raw => format!("countIf(status = '{s}')"),
            Self::Hour => format!("countIfMerge({s}_checks)"),
        }
    }

    /// Mean of a `{metric}_ms` column: `duration`, `dns`, `connect`, `tls` or
    /// `ttfb`.
    pub(super) fn avg_ms(self, metric: &str) -> String {
        match self {
            Self::Raw => format!("avg({metric}_ms)"),
            Self::Hour => format!("avgMerge(avg_{metric}_ms)"),
        }
    }

    /// `levels` must be a prefix of the rollup's `0.5, 0.95, 0.99`.
    pub(super) fn duration_quantiles(self, levels: &str) -> String {
        match self {
            Self::Raw => format!("quantiles({levels})(duration_ms)"),
            Self::Hour => format!("quantilesMerge({levels})(duration_quantiles)"),
        }
    }

    pub(super) fn last_status(self) -> &'static str {
        match self {
            Self::Raw => "argMax(status, timestamp)",
            Self::Hour => "argMaxMerge(last_status_state)",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn days_ago(d: i64) -> DateTime<Utc> {
        Utc::now() - chrono::Duration::days(d)
    }

    #[test]
    fn recent_ranges_read_raw() {
        assert_eq!(Source::for_range(days_ago(7), 30), Source::Raw);
        assert_eq!(Source::for_range(days_ago(29), 30), Source::Raw);
    }

    #[test]
    fn ranges_past_the_read_cap_read_the_hour_rollup() {
        assert_eq!(Source::for_range(days_ago(31), 30), Source::Hour);
        assert_eq!(Source::for_range(days_ago(31), 90), Source::Hour);
    }

    #[test]
    fn ranges_past_a_short_raw_window_read_the_hour_rollup() {
        assert_eq!(Source::for_range(days_ago(6), 7), Source::Raw);
        assert_eq!(Source::for_range(days_ago(8), 7), Source::Hour);
    }

    #[test]
    fn an_hour_counts_in_the_range_its_start_falls_in() {
        let t = |h: u32, m: u32| {
            DateTime::parse_from_rfc3339(&format!("2026-09-01T{h:02}:{m:02}:00Z"))
                .unwrap()
                .with_timezone(&Utc)
        };
        assert_eq!(Source::Hour.window_start(t(10, 15), t(10, 45)), t(10, 0));
        assert_eq!(Source::Hour.window_start(t(10, 15), t(11, 0)), t(10, 0));
        assert_eq!(Source::Hour.window_start(t(10, 15), t(11, 10)), t(10, 15));
        assert_eq!(Source::Raw.window_start(t(10, 15), t(10, 45)), t(10, 15));
        assert_eq!(Source::Hour.window_start(t(10, 45), t(10, 30)), t(10, 45));
        assert_eq!(Source::Hour.window_start(t(10, 30), t(10, 30)), t(10, 30));
    }

    #[test]
    fn hour_rollup_buckets_are_whole_hours() {
        assert_eq!(Source::Raw.bucket_width(1440), 1440);
        assert_eq!(Source::Raw.bucket_width(90), 120);
        assert_eq!(Source::Hour.bucket_width(1440), 3600);
        assert_eq!(Source::Hour.bucket_width(5400), 7200);
        assert_eq!(Source::Hour.bucket_width(43_200), 43_200);
        assert_eq!(Source::Hour.bucket_width(u32::MAX), u32::MAX / 3600 * 3600);
        assert_eq!(Source::Raw.bucket_width(u32::MAX), u32::MAX / 60 * 60);
    }

    #[test]
    fn both_sources_name_the_same_status_counts() {
        assert_eq!(
            Source::Raw.checks(CheckStatus::Degraded),
            "countIf(status = 'degraded')"
        );
        assert_eq!(
            Source::Hour.checks(CheckStatus::Degraded),
            "countIfMerge(degraded_checks)"
        );
    }
}
