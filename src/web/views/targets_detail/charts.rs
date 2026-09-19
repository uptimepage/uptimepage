use chrono::{DateTime, Utc};

use crate::domain::metrics::AvailabilityBucket;
use crate::domain::{Incident, confirmed_downtime_secs, uptime_pct_from_downtime};
use crate::storage::{ClampedRange, UptimeStats, rollup_bucket_secs};
use crate::web::views::dashboard::{
    Polarity, count_delta, render_spark_path_domain, ribbon_class, uptime_pp_delta,
};
use crate::web::views::fmt_ts;

use super::load::confirmed_uptime_pct;
use super::rows::KpiTrend;

// Target point count for the uptime sparkline; bucket width derives from it.
pub(super) const SPARK_SEGMENTS: i64 = 48;

/// Prefetched reads `build_kpi_trend` turns into the KPI strip's spark + deltas.
pub(super) struct KpiInputs<'a> {
    pub(super) current: &'a UptimeStats,
    pub(super) prior: &'a UptimeStats,
    pub(super) cur_incidents: &'a [Incident],
    pub(super) prior_incidents: &'a [Incident],
    pub(super) avail: &'a [AvailabilityBucket],
    pub(super) range: ClampedRange,
    pub(super) prior_range: ClampedRange,
    pub(super) spark_bucket_seconds: u32,
    pub(super) confirmed: bool,
}

/// Uptime sparkline plus Δ-vs-prior chips. Both mirror the displayed uptime %'s
/// source — confirmed downtime for the all-regions view, raw rate when
/// region-filtered — so neither the spark nor a chip can contradict the figure.
/// The prior window is the same span immediately before `range`.
pub(super) fn build_kpi_trend(inp: KpiInputs) -> KpiTrend {
    let (spark_path, spark_fill) = build_availability_spark(
        inp.avail,
        inp.cur_incidents,
        inp.range,
        inp.spark_bucket_seconds,
        inp.confirmed,
    );
    // Deltas need data in the current window and a full-length prior window: an
    // empty current window would read as a 100pp drop, and a prior truncated by
    // the retention floor would skew the raw counts against a shorter span.
    let comparable = inp.current.total > 0
        && inp.prior.total > 0
        && (inp.prior_range.to - inp.prior_range.from) >= (inp.range.to - inp.range.from);
    if !comparable {
        return KpiTrend {
            spark_path,
            spark_fill,
            ..Default::default()
        };
    }
    let prior_pct = if inp.confirmed {
        Some(confirmed_uptime_pct(inp.prior_incidents, inp.prior_range))
    } else {
        inp.prior.uptime_pct
    };
    KpiTrend {
        spark_path,
        spark_fill,
        uptime_delta: inp
            .current
            .uptime_pct
            .zip(prior_pct)
            .map(|(cur, prior)| uptime_pp_delta(cur, prior)),
        up_delta: Some(count_delta(
            inp.current.up,
            inp.prior.up,
            Polarity::HigherIsBetter,
        )),
        down_delta: Some(count_delta(
            inp.current.down,
            inp.prior.down,
            Polarity::LowerIsBetter,
        )),
        error_delta: Some(count_delta(
            inp.current.error,
            inp.prior.error,
            Polarity::LowerIsBetter,
        )),
    }
}

/// Builds the uptime sparkline over a fixed 0–100 domain (healthy sits flat at
/// the top, dips notch down). All-regions decomposes the confirmed headline:
/// every bucket is `1 − confirmed-incident-overlap`, measured the same way as
/// the figure beside it (whole window, trailing bucket up to `now`), so the line
/// can't disagree with it. Region-filtered has no confirmed model — it falls
/// back to the raw up-ratio, leaving sample gaps as `None`.
fn build_availability_spark(
    avail: &[AvailabilityBucket],
    incidents: &[Incident],
    range: ClampedRange,
    bucket_seconds: u32,
    confirmed: bool,
) -> (String, String) {
    let series = availability_series(avail, incidents, range, bucket_seconds, confirmed);
    let (path, fill, _) = render_spark_path_domain(&series, 0.0, 100.0);
    (path, fill)
}

/// Per-bucket availability over the window as a 0–100 series (`None` = no
/// samples). Shared by the KPI sparkline and the status strip so both read the
/// same model as the headline: confirmed-incident overlap for the all-regions
/// view, raw up-ratio when region-filtered.
fn availability_series(
    avail: &[AvailabilityBucket],
    incidents: &[Incident],
    range: ClampedRange,
    bucket_seconds: u32,
    confirmed: bool,
) -> Vec<Option<f32>> {
    let bucket = i64::from(rollup_bucket_secs(bucket_seconds));
    let from_grid = range.from.timestamp().div_euclid(bucket) * bucket;
    let n = (((range.to.timestamp() - from_grid) + bucket - 1) / bucket).max(1) as usize;
    let mut series = vec![None; n];
    if confirmed {
        let now = Utc::now();
        let now_ts = now.timestamp();
        for (i, slot) in series.iter_mut().enumerate() {
            let start_ts = from_grid + i as i64 * bucket;
            let end_ts = (start_ts + bucket).min(now_ts);
            let window = end_ts - start_ts;
            if window <= 0 {
                continue;
            }
            let start = DateTime::from_timestamp(start_ts, 0).unwrap_or(range.from);
            let end = DateTime::from_timestamp(end_ts, 0).unwrap_or(range.to);
            let down = confirmed_downtime_secs(incidents, start, end, now);
            *slot = Some(uptime_pct_from_downtime(down, window) as f32);
        }
    } else {
        for b in avail {
            if b.total == 0 {
                continue;
            }
            let slot = (b.bucket_ts - from_grid).div_euclid(bucket);
            if slot >= 0 && (slot as usize) < n {
                series[slot as usize] = Some((b.up as f32 / b.total as f32) * 100.0);
            }
        }
    }
    series
}

/// One cell of the uptime ribbon under the header. Mirrors the dashboard's
/// fleet ribbon so both reuse `.dashboard-ribbon__seg` and its tooltip JS.
pub struct StatusSeg {
    /// `op` | `deg` | `maj` | `none` — drives the `dashboard-ribbon__seg--*` class.
    pub class: &'static str,
    /// Bucket start (`%H:%M`) and uptime figure, shown in the hover tooltip.
    pub time: String,
    pub stat: String,
    /// ISO bounds of the bucket; a failing cell drills the drawer to this window.
    pub from_iso: String,
    pub to_iso: String,
    /// Check counts in the bucket; the drawer shows "{bad} of {total} failing"
    /// so a wide window's scale is known without fetching every row.
    pub total: u64,
    pub bad: u64,
}

/// Fixed-length `(total, up)` per bucket from the raw availability rollup, on the
/// same grid the ribbon draws. Feeds both the up-ratio tint and the drawer's
/// scale count, so one pass over `avail` serves both.
pub(super) fn bucket_counts(
    avail: &[AvailabilityBucket],
    range: ClampedRange,
    bucket_seconds: u32,
) -> Vec<(u64, u64)> {
    let bucket = i64::from(rollup_bucket_secs(bucket_seconds));
    let from_grid = range.from.timestamp().div_euclid(bucket) * bucket;
    let n = (((range.to.timestamp() - from_grid) + bucket - 1) / bucket).max(1) as usize;
    let mut counts = vec![(0u64, 0u64); n];
    for b in avail {
        let slot = (b.bucket_ts - from_grid).div_euclid(bucket);
        if slot >= 0 && (slot as usize) < n {
            let c = &mut counts[slot as usize];
            c.0 += b.total;
            c.1 += b.up;
        }
    }
    counts
}

/// Maps per-bucket `(total, up)` counts to ribbon cells, classified by the same
/// `ribbon_class` thresholds the dashboard fleet ribbon uses. A bucket with no
/// samples is `none` (hatched) so a paused or young monitor reads as a gap, not
/// green. `time`/`stat` populate the tooltip; `total`/`bad` size the drawer.
pub(super) fn status_segments(
    counts: &[(u64, u64)],
    range: ClampedRange,
    bucket_seconds: u32,
) -> Vec<StatusSeg> {
    let bucket = i64::from(rollup_bucket_secs(bucket_seconds));
    let from_grid = range.from.timestamp().div_euclid(bucket) * bucket;
    let to_ts = range.to.timestamp();
    // Multi-day ranges label buckets with the date — ten identical "12:00"
    // tooltips across a 30d strip are indistinguishable. UTC fallback only;
    // the tooltip JS re-renders it in the visitor's timezone.
    let multi_day = range.to - range.from > chrono::Duration::hours(24);
    counts
        .iter()
        .enumerate()
        .map(|(i, &(total, up))| {
            let start_ts = from_grid + i as i64 * bucket;
            let end_ts = (start_ts + bucket).min(to_ts).max(start_ts);
            let start = DateTime::from_timestamp(start_ts, 0).unwrap_or(range.from);
            let end = DateTime::from_timestamp(end_ts, 0).unwrap_or(range.to);
            let time = if multi_day {
                start.format("%b %d %H:%M").to_string()
            } else {
                start.format("%H:%M").to_string()
            };
            // The first cell's grid start precedes range.from; drill from range.from
            // so the drawer never lists rows the cell's counts never included.
            let drill_from = DateTime::from_timestamp(start_ts.max(range.from.timestamp()), 0)
                .unwrap_or(range.from);
            let from_iso = fmt_ts(drill_from);
            let to_iso = fmt_ts(end);
            let bad = total.saturating_sub(up);
            if total == 0 {
                StatusSeg {
                    class: "none",
                    time,
                    stat: "no data".into(),
                    from_iso,
                    to_iso,
                    total,
                    bad,
                }
            } else {
                let pct = (up as f64 / total as f64) * 100.0;
                StatusSeg {
                    class: ribbon_class(pct),
                    time,
                    stat: format!("{pct:.1}%"),
                    from_iso,
                    to_iso,
                    total,
                    bad,
                }
            }
        })
        .collect()
}
