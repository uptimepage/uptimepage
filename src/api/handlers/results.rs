use crate::api::json::Json;
use axum::extract::{Path, Query, State};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use utoipa::IntoParams;
use uuid::Uuid;

use crate::api::ApiError;
use crate::api::error::codes;
use crate::api::page::{PageEnvelope, PageOfCheckResult, PageOfIncident};
use crate::api::types::{FlowStepSeries, LatencySeries, LatencySeriesByRegion};
use crate::app::AppState;
use crate::domain::{confirmed_downtime_secs, humanize_check_error, uptime_pct_from_downtime};
use crate::error::{AppError, Result};
use crate::storage::{TimeRange, UptimeStats};
use crate::web::{Authorized, TargetsRead};

/// 404 used when a target id is absent from the caller's org. Returned only
/// after the org-scoped `get` resolves to `None`, so a foreign tenant's UUID
/// is indistinguishable from a non-existent one — no empty-vs-populated
/// oracle. The `get` is issued concurrently with the (already org-scoped)
/// results query so the cloak costs no extra round-trip on the hot path.
fn target_not_found() -> AppError {
    AppError::not_found(codes::TARGET_NOT_FOUND, "target not found")
}

const RESULTS_LIMIT_DEFAULT: usize = 1_000;
const RESULTS_LIMIT_MAX: usize = 10_000;
const INCIDENTS_LIMIT_DEFAULT: usize = 100;
const INCIDENTS_LIMIT_MAX: usize = 1_000;
// Confirmed incidents over the longest window are few; bounds the downtime read.
const UPTIME_INCIDENT_CAP: usize = 2_000;

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
pub(crate) fn resolve_range_uncapped(
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
    pub(crate) fn resolve(&self) -> Result<TimeRange> {
        resolve_range(self.from, self.to)
    }

    pub(crate) fn resolve_uncapped(&self) -> Result<TimeRange> {
        resolve_range_uncapped(self.from, self.to)
    }

    pub(crate) fn limit(&self) -> usize {
        self.limit
            .unwrap_or(RESULTS_LIMIT_DEFAULT)
            .min(RESULTS_LIMIT_MAX)
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/targets/{id}/results",
    tag = "results",
    summary = "Query check results for a target",
    params(
        ("id" = Uuid, Path, description = "Target id"),
        RangeQuery,
    ),
    responses(
        (status = 200, body = PageOfCheckResult, example = json!({
            "items": [{
                "target_id": "01h7m8z4n6v0e1m7v7y6x8x8x8",
                "timestamp": "2026-05-13T12:00:00.000Z",
                "status": "up",
                "duration_ms": 142
            }],
            "limit": 1000, "offset": 0, "has_more": false
        })),
        (status = 400, description = "Bad time range or filter", body = ApiError),
        (status = 404, description = "Target not found", body = ApiError),
    ),
)]
pub async fn list_results(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<TargetsRead>,
    Path(id): Path<Uuid>,
    Query(q): Query<RangeQuery>,
) -> Result<Json<PageOfCheckResult>> {
    let range = state.quotas.clamp_raw(org, q.resolve()?).await?;
    let limit = q.limit();
    let offset = q.offset;
    // The org-scoped `get` rides alongside the (also org-scoped) results
    // queries: a foreign/unknown id still 404s, but the cloak adds no serial
    // round-trip. Results are discarded unless the target resolves.
    let (target, peek) = tokio::try_join!(
        state.target_store.get(org, id),
        state
            .results_store
            .list_results(org, id, range, limit + 1, offset, q.region.as_deref()),
    )?;
    if target.is_none() {
        return Err(target_not_found());
    }
    Ok(Json(PageEnvelope::from_peek(
        peek,
        limit as u32,
        offset as u32,
    )))
}

/// Slices a full-width chart aims for, so 1h and 30d both return a comparably
/// dense series and switching ranges visibly re-scales the chart.
const LATENCY_TARGET_BUCKETS: i64 = 60;

/// Slices a step sparkline aims for. It renders a few hundred pixels wide, so
/// the latency grain would spend bytes and DOM on detail narrower than a
/// stroke — a 30-step flow at 60 slices is a 72 KiB page-load fetch.
const FLOW_STEP_TARGET_BUCKETS: i64 = 30;

/// Bucket width (seconds) splitting `range` into roughly `target` slices,
/// floored to a whole minute (the rollup grain) with a 60s minimum.
/// At 60: 1h→60s, 24h→1440s, 7d→10080s, 30d→43200s.
fn bucket_seconds(range: TimeRange, target: i64) -> u32 {
    let span = (range.to - range.from).num_seconds().max(60);
    let secs = (span / target / 60).max(1) * 60;
    u32::try_from(secs).unwrap_or(u32::MAX)
}

pub(crate) fn latency_bucket_seconds(range: TimeRange) -> u32 {
    bucket_seconds(range, LATENCY_TARGET_BUCKETS)
}

#[utoipa::path(
    get,
    path = "/api/v1/targets/{id}/latency",
    tag = "results",
    summary = "Bucketed latency series for a target",
    description = "Returns p50/p95/p99 and mean per-phase timings, pre-bucketed \
                   server-side from the per-minute rollup into ~60 slices across \
                   the range. Switching range re-scales the buckets; cost stays \
                   O(buckets), not O(samples). Powers the monitor-detail charts.",
    params(
        ("id" = Uuid, Path, description = "Target id"),
        ("from" = Option<DateTime<Utc>>, Query, description = "Inclusive lower bound (default: now-24h)"),
        ("to" = Option<DateTime<Utc>>, Query, description = "Exclusive upper bound (default: now)"),
    ),
    responses(
        (status = 200, body = LatencySeries, example = json!({
            "bucket_seconds": 1440,
            "buckets": [{
                "t": 1747137600000_i64,
                "p50": 120, "p95": 180, "p99": 240, "avg": 130,
                "dns": 12, "connect": 20, "tls": 35, "ttfb": 60,
                "samples": 24
            }]
        })),
        (status = 400, description = "Bad time range", body = ApiError),
        (status = 404, description = "Target not found", body = ApiError),
    ),
)]
pub async fn latency(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<TargetsRead>,
    Path(id): Path<Uuid>,
    Query(q): Query<RangeQuery>,
) -> Result<Json<LatencySeries>> {
    let range = state
        .quotas
        .clamp_history(org, q.resolve_uncapped()?)
        .await?;
    let bucket_seconds = latency_bucket_seconds(range.inner());
    // Org-scoped `get` rides alongside the (also org-scoped) rollup read so a
    // foreign/unknown id still 404s without a serial round-trip.
    let (target, buckets) = tokio::try_join!(
        state.target_store.get(org, id),
        state
            .results_store
            .latency_buckets(org, id, range, bucket_seconds, q.region.as_deref()),
    )?;
    if target.is_none() {
        return Err(target_not_found());
    }
    Ok(Json(LatencySeries {
        buckets,
        bucket_seconds,
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/targets/{id}/latency/by-region",
    tag = "results",
    summary = "Per-region bucketed latency series for a target",
    description = "Like `/latency`, but split by probe region so each region can \
                   be overlaid as its own line. One entry per region with samples \
                   in the range; same server-side bucketing and O(buckets) cost.",
    params(
        ("id" = Uuid, Path, description = "Target id"),
        ("from" = Option<DateTime<Utc>>, Query, description = "Inclusive lower bound (default: now-24h)"),
        ("to" = Option<DateTime<Utc>>, Query, description = "Exclusive upper bound (default: now)"),
    ),
    responses(
        (status = 200, body = LatencySeriesByRegion),
        (status = 400, description = "Bad time range", body = ApiError),
        (status = 404, description = "Target not found", body = ApiError),
    ),
)]
pub async fn latency_by_region(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<TargetsRead>,
    Path(id): Path<Uuid>,
    Query(q): Query<RangeQuery>,
) -> Result<Json<LatencySeriesByRegion>> {
    let range = state
        .quotas
        .clamp_history(org, q.resolve_uncapped()?)
        .await?;
    let bucket_seconds = latency_bucket_seconds(range.inner());
    let (target, mut regions, catalog) = tokio::try_join!(
        state.target_store.get(org, id),
        state
            .results_store
            .latency_buckets_by_region(org, id, range, bucket_seconds),
        state.regions_detailed(),
    )?;
    if target.is_none() {
        return Err(target_not_found());
    }
    for series in &mut regions {
        series.label = catalog
            .iter()
            .find(|r| r.id == series.region)
            .map(|r| r.display_name().to_string())
            .unwrap_or_else(|| series.region.clone());
    }
    Ok(Json(LatencySeriesByRegion {
        regions,
        bucket_seconds,
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/targets/{id}/flow-steps",
    tag = "results",
    summary = "Per-step duration series for a browser-flow monitor",
    description = "One series per declared step: the mean duration among the \
                   runs that passed it, plus how many failed. Steps a run never \
                   reached are excluded, so a journey that stopped early does \
                   not average zeros into the steps behind it. Failures are \
                   counted but kept out of the mean — a failed step sat in its \
                   whole step timeout and would bury the runs around it. `avg` \
                   is null for a bucket nothing passed. Bucketed like `/latency` \
                   but coarser: these render as sparklines, and a 30-step flow \
                   at the latency grain is a 72 KiB response. Empty for every \
                   check kind but flow.",
    params(
        ("id" = Uuid, Path, description = "Target id"),
        ("from" = Option<DateTime<Utc>>, Query, description = "Inclusive lower bound (default: now-24h)"),
        ("to" = Option<DateTime<Utc>>, Query, description = "Exclusive upper bound (default: now)"),
    ),
    responses(
        (status = 200, body = FlowStepSeries, example = json!({
            "bucket_seconds": 1440,
            "steps": [{
                "step": 3,
                "op": "assert_url",
                "buckets": [{ "t": 1747137600000_i64, "avg": 1840, "samples": 5, "failed": 0 }]
            }]
        })),
        (status = 400, description = "Bad time range", body = ApiError),
        (status = 404, description = "Target not found", body = ApiError),
    ),
)]
pub async fn flow_steps(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<TargetsRead>,
    Path(id): Path<Uuid>,
    Query(q): Query<RangeQuery>,
) -> Result<Json<FlowStepSeries>> {
    // Raw-table read, so the raw window and the span cap both apply: the runs
    // it aggregates are gone on the same day the run panel's are, and one
    // request fans every run out per declared step before grouping.
    let range = state.quotas.clamp_raw(org, q.resolve()?).await?;
    let bucket_seconds = bucket_seconds(range.inner(), FLOW_STEP_TARGET_BUCKETS);
    let (target, steps) = tokio::try_join!(
        state.target_store.get(org, id),
        state
            .results_store
            .flow_step_buckets(org, id, range, bucket_seconds, q.region.as_deref()),
    )?;
    if target.is_none() {
        return Err(target_not_found());
    }
    Ok(Json(FlowStepSeries {
        steps,
        bucket_seconds,
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/targets/{id}/uptime",
    tag = "results",
    summary = "Uptime stats for a target over a time range",
    description = "Counts per-status check totals and computes uptime percentage. \
                   `uptime_pct` is null when the window holds no checks, which is \
                   unknown rather than zero.",
    params(
        ("id" = Uuid, Path),
        ("from" = Option<DateTime<Utc>>, Query, description = "Inclusive lower bound (default: now-24h)"),
        ("to" = Option<DateTime<Utc>>, Query, description = "Exclusive upper bound (default: now)"),
    ),
    responses(
        (status = 200, body = UptimeStats, example = json!({
            "total": 1440, "up": 1437, "down": 2, "degraded": 1, "error": 0, "uptime_pct": 99.79
        })),
        (status = 400, body = ApiError),
        (status = 404, body = ApiError),
    ),
)]
pub async fn uptime(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<TargetsRead>,
    Path(id): Path<Uuid>,
    Query(q): Query<RangeQuery>,
) -> Result<Json<UptimeStats>> {
    let range = state.quotas.clamp_raw(org, q.resolve()?).await?;
    let (target, mut stats) = tokio::try_join!(
        state.target_store.get(org, id),
        state
            .results_store
            .uptime(org, id, range, q.region.as_deref()),
    )?;
    if target.is_none() {
        return Err(target_not_found());
    }
    // All-regions uptime is reported as confirmed downtime over the window; a
    // region filter keeps the raw per-region sample rate.
    if q.region.is_none() && stats.total > 0 {
        let incidents = state
            .incident_narration_store
            .list_for_target(org, id, range.inner(), UPTIME_INCIDENT_CAP, 0, false)
            .await?;
        let down = confirmed_downtime_secs(&incidents, range.from, range.to, Utc::now());
        stats.uptime_pct = Some(uptime_pct_from_downtime(
            down,
            (range.to - range.from).num_seconds(),
        ));
    }
    Ok(Json(stats))
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct IncidentsQuery {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: usize,
    /// If true, return only currently-active incidents.
    #[serde(default)]
    pub ongoing_only: bool,
}

#[utoipa::path(
    get,
    path = "/api/v1/targets/{id}/incidents",
    tag = "results",
    summary = "List incidents (coalesced down/error periods) for a target",
    description = "An incident is a contiguous period of `down` or `error` status. Two consecutive bad checks separated by one `up` count as separate incidents. Ongoing incidents have `ended_at: null`.",
    params(
        ("id" = Uuid, Path),
        IncidentsQuery,
    ),
    responses(
        (status = 200, body = PageOfIncident, example = json!({
            "items": [{
                "id": "01h7m8z4n6v0e1m7v7y6x8x8x8",
                "target_id": "01h7m8z4n6v0e1m7v7y6x8x8x8",
                "started_at": "2026-05-13T11:30:00.000Z",
                "ended_at": "2026-05-13T11:35:00.000Z",
                "status": "down",
                "duration_secs": 300,
                "check_count": 5,
                "error_sample": "connection refused"
            }],
            "limit": 100, "offset": 0, "has_more": false
        })),
        (status = 400, body = ApiError),
        (status = 404, body = ApiError),
    ),
)]
pub async fn list_incidents(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<TargetsRead>,
    Path(id): Path<Uuid>,
    Query(q): Query<IncidentsQuery>,
) -> Result<Json<PageOfIncident>> {
    let range = state
        .quotas
        .clamp_raw(org, resolve_range(q.from, q.to)?)
        .await?;
    let limit = q
        .limit
        .unwrap_or(INCIDENTS_LIMIT_DEFAULT)
        .min(INCIDENTS_LIMIT_MAX);
    // Existence + tenant check; 404 separates an unknown monitor from empty history.
    state
        .target_store
        .get(org, id)
        .await?
        .ok_or_else(target_not_found)?;
    let mut peek = state
        .incident_narration_store
        .list_for_target(org, id, range.inner(), limit + 1, q.offset, q.ongoing_only)
        .await?;
    for inc in &mut peek {
        inc.error_sample = inc.error_sample.as_deref().map(humanize_check_error);
    }
    Ok(Json(PageEnvelope::from_peek(
        peek,
        limit as u32,
        q.offset as u32,
    )))
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
