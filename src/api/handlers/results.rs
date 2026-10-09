use crate::request::json::Json;
use axum::extract::{Path, Query, State};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use utoipa::IntoParams;
use uuid::Uuid;

use crate::api::types::{FlowStepSeries, LatencySeries, LatencySeriesByRegion};
use crate::app::AppState;
use crate::domain::{confirmed_downtime_secs, humanize_check_error, uptime_pct_from_downtime};
use crate::error::ApiError;
use crate::error::codes;
use crate::error::{AppError, Result};
use crate::pagination::page::{PageEnvelope, PageOfCheckResult, PageOfIncident};
use crate::request::range::{
    FLOW_STEP_TARGET_BUCKETS, RangeQuery, bucket_seconds, latency_bucket_seconds, resolve_range,
};
use crate::request::{Authorized, TargetsRead};
use crate::storage::UptimeStats;

/// 404 used when a target id is absent from the caller's org. Returned only
/// after the org-scoped `get` resolves to `None`, so a foreign tenant's UUID
/// is indistinguishable from a non-existent one — no empty-vs-populated
/// oracle. The `get` is issued concurrently with the (already org-scoped)
/// results query so the cloak costs no extra round-trip on the hot path.
fn target_not_found() -> AppError {
    AppError::not_found(codes::TARGET_NOT_FOUND, "target not found")
}

const INCIDENTS_LIMIT_DEFAULT: usize = 100;
const INCIDENTS_LIMIT_MAX: usize = 1_000;
// Confirmed incidents over the longest window are few; bounds the downtime read.
const UPTIME_INCIDENT_CAP: usize = 2_000;

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
    description = "Counts per-status check totals and computes uptime percentage: \
                   confirmed incident downtime over the window, a partial outage \
                   counted at 30% and degraded performance not at all, or with \
                   `region` set that region's share of passing checks. `uptime_pct` \
                   is null when the window holds no checks, which is unknown rather \
                   than zero.",
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
