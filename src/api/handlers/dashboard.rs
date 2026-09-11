use std::collections::HashMap;
use std::sync::Arc;

use crate::api::json::Json;
use axum::extract::State;
use chrono::{Duration, Utc};
use uuid::Uuid;

use crate::api::ApiError;
use crate::api::types::{
    DashboardMetrics, DashboardSummary, Last24hSummary, StatusBreakdown, SystemSummary,
};
use crate::app::AppState;
use crate::domain::{CheckStatus, OrgId, Target};
use crate::error::Result;
use crate::storage::{TargetFilter, TimeRange};
use crate::web::CurrentOrg;

const MAX_ORG_MONITORS: usize = 10_000;

#[utoipa::path(
    get,
    path = "/api/v1/dashboard/summary",
    tag = "dashboard",
    summary = "Fleet-wide aggregated metrics for the operator dashboard",
    description = "Composed from ClickHouse rollups + Postgres counts + in-process gauges. Cached in-process for 5 seconds.",
    responses(
        (status = 200, body = DashboardSummary, example = json!({
            "targets": {"total": 42, "enabled": 40, "disabled": 2},
            "current_status": {"up": 38, "down": 1, "degraded": 1, "error": 0, "unknown": 2},
            "last_24h": {"checks_total": 50400, "checks_up": 50360, "uptime_pct": 99.92, "incidents": 3},
            "system": {"in_flight_checks": 5, "result_queue_depth": 12, "dropped_results_last_5m": 0, "circuit_breakers_open": 0}
        })),
        (status = 503, body = ApiError, description = "One or more data sources unavailable"),
    ),
)]
pub async fn dashboard_summary(
    State(state): State<AppState>,
    CurrentOrg(org_id): CurrentOrg,
) -> Result<Json<DashboardSummary>> {
    if let Some(snapshot) = state.dashboard_cache.get(&org_id) {
        return Ok(Json((*snapshot).clone()));
    }

    let now = Utc::now();
    let range = TimeRange {
        from: now - Duration::try_hours(24).unwrap_or_default(),
        to: now,
    };

    let (targets, monitors, rollup, (checks_total, checks_up, _avg_ms, incidents)) = tokio::try_join!(
        state.target_store.summary(org_id),
        state.target_store.list(
            org_id,
            TargetFilter {
                // Default limit is a page of 100; the breakdown counts the org.
                limit: Some(MAX_ORG_MONITORS),
                ..TargetFilter::default()
            },
        ),
        state.results_store.dashboard_rollup(org_id, range, None),
        state.results_store.last_n_summary(org_id, range, None),
    )?;
    let current_status = status_breakdown(&state, org_id, range, &monitors, rollup).await;

    let uptime_pct = if checks_total > 0 {
        (checks_up as f64 / checks_total as f64) * 100.0
    } else {
        0.0
    };

    let summary = DashboardSummary {
        targets,
        current_status,
        last_24h: Last24hSummary {
            checks_total,
            checks_up,
            uptime_pct,
            incidents,
        },
        system: SystemSummary {
            in_flight_checks: u32::try_from(state.worker_pool.in_flight()).unwrap_or(u32::MAX),
            result_queue_depth: u32::try_from(state.worker_pool.result_queue_depth())
                .unwrap_or(u32::MAX),
            // Cumulative since process start; the field name keeps `_last_5m`
            // for response-shape stability.
            dropped_results_last_5m: state.worker_pool.dropped_results(),
            circuit_breakers_open: u32::try_from(state.worker_pool.open_breakers())
                .unwrap_or(u32::MAX),
        },
    };

    state
        .dashboard_cache
        .insert(org_id, Arc::new(summary.clone()));
    Ok(Json(summary))
}

/// Counts by folded status, not by whichever region reported last.
async fn status_breakdown(
    state: &AppState,
    org: OrgId,
    range: TimeRange,
    monitors: &[Target],
    rollup: Vec<DashboardMetrics>,
) -> StatusBreakdown {
    let metrics: HashMap<Uuid, DashboardMetrics> =
        rollup.into_iter().map(|m| (m.target_id, m)).collect();
    let folded = state
        .folded_status(org, range, crate::app::folded_status_policies(monitors))
        .await;
    let mut out = StatusBreakdown::default();
    for t in monitors {
        let status = metrics.get(&t.id).filter(|m| m.samples > 0).and_then(|m| {
            folded
                .get(&t.id)
                .copied()
                .or_else(|| CheckStatus::from_label(&m.last_status))
        });
        match status {
            Some(CheckStatus::Up) => out.up += 1,
            Some(CheckStatus::Down) => out.down += 1,
            Some(CheckStatus::Degraded) => out.degraded += 1,
            Some(CheckStatus::Error) => out.error += 1,
            None => out.unknown += 1,
        }
    }
    out
}
