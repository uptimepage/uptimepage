//! The `from`/`to`/`limit` query every time-series read takes, and how a window
//! becomes a bucket width, so the REST reads and the share-link pages cut the
//! same range the same way.

use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use utoipa::IntoParams;

use crate::error::codes;
use crate::error::{AppError, Result};
use crate::storage::TimeRange;

const RESULTS_LIMIT_DEFAULT: usize = 1_000;
const RESULTS_LIMIT_MAX: usize = 10_000;

/// Hard cap on the requested `(to - from)` span, bounding the worst-case
/// result read per request regardless of any plan's retention window.
/// Callers needing deeper history page backwards via `to`.
const MAX_RANGE_DAYS: i64 = 90;

/// Resolves optional `from`/`to` query params to a validated `TimeRange`,
/// defaulting to a 24-hour window ending at `now`. Returns `BAD_TIME_RANGE`
/// when `to <= from` or when the requested span exceeds [`MAX_RANGE_DAYS`].
pub(crate) fn resolve_range(
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
) -> Result<TimeRange> {
    let range = resolve_range_uncapped(from, to)?;
    let max = Duration::try_days(MAX_RANGE_DAYS).unwrap_or_default();
    if range.to - range.from > max {
        return Err(AppError::bad_request(
            codes::BAD_TIME_RANGE,
            "time window exceeds maximum of 90 days; page backwards via 'to'",
        ));
    }
    Ok(range)
}

/// Like [`resolve_range`] without the 90-day span cap. For rollup reads, where
/// the per-plan `clamp_history` bounds the lookback (up to 13 months) and the
/// pre-aggregated source has no per-request memory blow-up to guard against.
fn resolve_range_uncapped(
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
) -> Result<TimeRange> {
    let to = to.unwrap_or_else(Utc::now);
    let from = from.unwrap_or_else(|| to - Duration::try_hours(24).unwrap_or_default());
    if to <= from {
        return Err(AppError::bad_request(
            codes::BAD_TIME_RANGE,
            "'to' must be strictly greater than 'from'",
        ));
    }
    Ok(TimeRange { from, to })
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RangeQuery {
    /// Inclusive lower bound (default: now-24h).
    pub from: Option<DateTime<Utc>>,
    /// Exclusive upper bound (default: now).
    pub to: Option<DateTime<Utc>>,
    /// Page size (default 1000, max 10000).
    pub limit: Option<usize>,
    /// Page offset (default 0).
    #[serde(default)]
    pub offset: usize,
    /// Restrict to one probe region; omit for all regions.
    pub region: Option<String>,
}

impl RangeQuery {
    pub fn resolve(&self) -> Result<TimeRange> {
        resolve_range(self.from, self.to)
    }

    pub fn resolve_uncapped(&self) -> Result<TimeRange> {
        resolve_range_uncapped(self.from, self.to)
    }

    pub fn limit(&self) -> usize {
        self.limit
            .unwrap_or(RESULTS_LIMIT_DEFAULT)
            .min(RESULTS_LIMIT_MAX)
    }
}

/// Slices a full-width chart aims for, so 1h and 30d both return a comparably
/// dense series and switching ranges visibly re-scales the chart.
const LATENCY_TARGET_BUCKETS: i64 = 60;

/// Slices a step sparkline aims for. It renders a few hundred pixels wide, so
/// the latency grain would spend bytes and DOM on detail narrower than a
/// stroke — a 30-step flow at 60 slices is a 72 KiB page-load fetch.
pub(crate) const FLOW_STEP_TARGET_BUCKETS: i64 = 30;

/// Bucket width (seconds) splitting `range` into roughly `target` slices,
/// floored to a whole minute (the rollup grain) with a 60s minimum.
/// At 60: 1h→60s, 24h→1440s, 7d→10080s, 30d→43200s.
pub(crate) fn bucket_seconds(range: TimeRange, target: i64) -> u32 {
    let span = (range.to - range.from).num_seconds().max(60);
    let secs = (span / target / 60).max(1) * 60;
    u32::try_from(secs).unwrap_or(u32::MAX)
}

pub(crate) fn latency_bucket_seconds(range: TimeRange) -> u32 {
    bucket_seconds(range, LATENCY_TARGET_BUCKETS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, 0, 0, 0).unwrap()
    }

    fn assert_bad_range(err: AppError) -> String {
        match err {
            AppError::BadRequest { code, message, .. } => {
                assert_eq!(code, codes::BAD_TIME_RANGE);
                message
            }
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn rejects_equal_from_and_to() {
        let t = ts(2026, 5, 1);
        let err = resolve_range(Some(t), Some(t)).expect_err("equal must reject");
        assert_bad_range(err);
    }

    #[test]
    fn rejects_from_after_to() {
        let from = ts(2026, 5, 2);
        let to = ts(2026, 5, 1);
        let err = resolve_range(Some(from), Some(to)).expect_err("inverted must reject");
        assert_bad_range(err);
    }

    #[test]
    fn rejects_window_exceeding_max() {
        let to = ts(2026, 5, 1);
        let from = to - Duration::try_days(MAX_RANGE_DAYS + 1).unwrap();
        let err = resolve_range(Some(from), Some(to)).expect_err("over-max must reject");
        let msg = assert_bad_range(err);
        assert!(
            msg.contains("90 days"),
            "message should name the limit, got: {msg}"
        );
    }

    #[test]
    fn accepts_exactly_max_window() {
        let to = ts(2026, 5, 1);
        let from = to - Duration::try_days(MAX_RANGE_DAYS).unwrap();
        let range = resolve_range(Some(from), Some(to)).expect("boundary must pass");
        assert_eq!(range.from, from);
        assert_eq!(range.to, to);
    }

    #[test]
    fn latency_bucket_seconds_scales_with_range() {
        let span = |d: Duration| {
            let to = ts(2026, 5, 1);
            latency_bucket_seconds(TimeRange { from: to - d, to })
        };
        assert_eq!(span(Duration::try_hours(1).unwrap()), 60);
        assert_eq!(span(Duration::try_hours(24).unwrap()), 1440);
        assert_eq!(span(Duration::try_days(7).unwrap()), 10080);
        assert_eq!(span(Duration::try_days(30).unwrap()), 43200);
    }

    // Sparklines are a few hundred pixels wide, so the latency grain would spend
    // payload on detail thinner than the stroke drawing it.
    #[test]
    fn step_buckets_are_coarser_than_latency_buckets() {
        let to = ts(2026, 5, 1);
        for days in [1, 7, 30] {
            let range = TimeRange {
                from: to - Duration::try_days(days).unwrap(),
                to,
            };
            let steps = bucket_seconds(range, FLOW_STEP_TARGET_BUCKETS);
            assert!(
                steps >= latency_bucket_seconds(range) * 2,
                "{days}d: step bucket {steps}s is not coarser"
            );
        }
    }

    #[test]
    fn latency_bucket_seconds_floors_to_one_minute() {
        // A sub-hour span must never produce a bucket below the 60s rollup grain.
        let to = ts(2026, 5, 1);
        let from = to - Duration::try_minutes(10).unwrap();
        assert_eq!(latency_bucket_seconds(TimeRange { from, to }), 60);
    }

    #[test]
    fn defaults_to_last_24h_when_omitted() {
        let range = resolve_range(None, None).expect("defaults must validate");
        assert_eq!(range.to - range.from, Duration::try_hours(24).unwrap());
    }
}
