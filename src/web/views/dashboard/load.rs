//! Snapshot assembly: the cached batched reads behind one dashboard render.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{Duration, Utc};
use uuid::Uuid;

use crate::app::AppState;
use crate::domain::metrics::{DashboardMetrics, PriorPeriodSummary};
use crate::domain::{CheckStatus, OrgId, uptime_pct_from_downtime};
use crate::storage::{IncidentBriefFilter, TargetFilter, TimeRange};
use crate::web::error::WebResult;
use crate::web::views::describe_check;

use super::charts::{
    ACTIVE_INCIDENTS_LIMIT, TYPE_CHIP_ORDER, avg_response_label, build_fleet_ribbon,
    build_kpi_cards, build_type_counts, fleet_sparks, format_count, group_sparks, pct_label,
    range_span, snap_to_bucket, tally_status,
};
use super::*;

/// Cached front door — both `index` and `table_partial` reach the same
/// `Arc<DashboardSnapshot>` so a tab-spam burst collapses to one CH
/// round-trip. The cache itself enforces the 5 s TTL.
pub(super) async fn load_snapshot(
    state: &AppState,
    org: OrgId,
    range: &'static str,
) -> WebResult<Arc<DashboardSnapshot>> {
    if let Some(snap) = state.dashboard_page_cache.get(&(org, range)) {
        return Ok(snap);
    }
    let snap = Arc::new(build_snapshot(state, org, range, None).await?);
    state
        .dashboard_page_cache
        .insert((org, range), Arc::clone(&snap));
    Ok(snap)
}

pub(super) async fn build_snapshot(
    state: &AppState,
    org: OrgId,
    range: &'static str,
    region: Option<&str>,
) -> WebResult<DashboardSnapshot> {
    let to = Utc::now();
    let from = to - range_span(range);
    let time_range = TimeRange { from, to };
    let spark_from = to - Duration::minutes(SPARK_MINUTES);
    // Snap to a bucket boundary so the CH-side `toStartOfInterval` grid
    // aligns 1:1 with the labels we render. Without this `from = now -
    // 24h` is mid-bucket and the tooltips drift by up to 29 minutes.
    let ribbon_from = snap_to_bucket(to - Duration::hours(RIBBON_HOURS), RIBBON_BUCKET_SECONDS);

    // Region view lists only targets that run there — otherwise off-region
    // monitors fill the page (showing "—") and the ROW_LIMIT truncates the
    // wrong set. All-regions (region None) keeps the full list.
    let target_filter = TargetFilter {
        limit: Some(ROW_LIMIT + 1),
        offset: 0,
        region: region.map(str::to_owned),
        ..Default::default()
    };

    let (
        mut targets,
        rollup,
        spark_rows,
        (checks_total, checks_up, avg_ms_current, incidents),
        active_raw,
        ribbon_rows,
        prior,
        downtime_by_target,
    ) = tokio::try_join!(
        state.target_store.list(org, target_filter),
        state
            .results_store
            .dashboard_rollup(org, time_range, region),
        state
            .results_store
            .dashboard_sparkline(org, spark_from, to, region),
        state.results_store.last_n_summary(org, time_range, region),
        state.incident_narration_store.list_briefs(
            org,
            IncidentBriefFilter {
                oldest_first: true,
                limit: ACTIVE_INCIDENTS_LIMIT,
                ..Default::default()
            },
        ),
        state
            .results_store
            .fleet_ribbon(org, ribbon_from, to, RIBBON_BUCKET_SECONDS, region),
        state
            .results_store
            .prior_period_summary(org, time_range, region),
        state
            .incident_narration_store
            .confirmed_downtime_by_target(org, time_range),
    )?;

    let window_secs = (time_range.to - time_range.from).num_seconds();
    // A region filter keeps the raw per-region rate; only all-regions is confirmed.
    let confirmed = region.is_none();

    let truncated = targets.len() > ROW_LIMIT;
    if truncated {
        targets.truncate(ROW_LIMIT);
    }

    // Only dipped monitors are named in the ribbon tooltip, so clone names for
    // those ids alone rather than the whole (healthy) fleet on every build.
    let down_ids: std::collections::HashSet<Uuid> = ribbon_rows
        .iter()
        .flat_map(|r| r.down_targets.iter().copied())
        .collect();
    let target_names: HashMap<Uuid, String> = targets
        .iter()
        .filter(|t| down_ids.contains(&t.id))
        .map(|t| (t.id, t.name.clone()))
        .collect();
    let metrics_by_target: HashMap<Uuid, DashboardMetrics> =
        rollup.into_iter().map(|m| (m.target_id, m)).collect();
    // Same split as `confirmed`: a single-region view wants that region's raw
    // verdict, so there is nothing to fold.
    let folded_status: HashMap<Uuid, CheckStatus> = if confirmed {
        state
            .folded_status(
                org,
                time_range,
                crate::app::folded_status_policies(&targets),
            )
            .await
    } else {
        HashMap::new()
    };
    let spark_by_target = group_sparks(&spark_rows, spark_from);
    // Cosmetic overlay — a silence-query hiccup must not fail the dashboard.
    let silenced: std::collections::HashSet<Uuid> = state
        .silence_store
        .open_target_ids(org)
        .await
        .unwrap_or_default()
        .into_iter()
        .collect();

    let mut status_counts = StatusCounts::default();
    let mut type_acc: [u32; TYPE_CHIP_ORDER.len()] = [0; TYPE_CHIP_ORDER.len()];
    let rows: Vec<DashboardRow> = targets
        .into_iter()
        .map(|t| {
            let (kind, address) = describe_check(&t.check);
            let metrics = metrics_by_target.get(&t.id);
            let spark = spark_by_target
                .get(&t.id)
                .cloned()
                .unwrap_or_else(|| vec![None; SPARK_BUCKETS]);
            let dt = confirmed.then(|| downtime_by_target.get(&t.id).copied().unwrap_or(0));
            let folded = folded_status.get(&t.id).copied();
            let mut row = DashboardRow::build(
                t.id,
                t.name,
                kind,
                address,
                t.enabled,
                metrics,
                folded,
                spark,
                dt,
                window_secs,
            );
            // No live probe overrides the stale last status with grey "no data".
            if silenced.contains(&t.id) {
                row.last_status = "no_data";
            }
            tally_status(&mut status_counts, &row);
            if let Some(idx) = TYPE_CHIP_ORDER.iter().position(|k| *k == kind) {
                type_acc[idx] += 1;
            }
            row
        })
        .collect();

    let fleet_sparks = fleet_sparks(&spark_rows, spark_from);
    let checks_successful_label = format!("{} successful", format_count(checks_up));
    let current = PriorPeriodSummary {
        checks_total,
        checks_up,
        avg_ms: avg_ms_current,
    };
    // Time-weighted over sampled monitors so the fleet KPI matches the rows.
    let fleet_uptime_label = if confirmed {
        let mut total_down = 0i64;
        let mut sampled = 0i64;
        for (id, m) in &metrics_by_target {
            if m.samples > 0 {
                sampled += 1;
                total_down += downtime_by_target.get(id).copied().unwrap_or(0);
            }
        }
        if sampled > 0 {
            format!(
                "{:.2}%",
                uptime_pct_from_downtime(total_down, window_secs * sampled)
            )
        } else {
            pct_label(checks_total, checks_up)
        }
    } else {
        pct_label(checks_total, checks_up)
    };
    let kpis = DashboardKpis {
        uptime_pct_label: fleet_uptime_label,
        avg_response_ms_label: avg_response_label(avg_ms_current, checks_total),
        checks_label: format_count(checks_total),
        checks_successful_label: checks_successful_label.clone(),
        incidents,
    };
    let kpi_cards = build_kpi_cards(
        &kpis,
        range,
        checks_successful_label,
        &current,
        &prior,
        &fleet_sparks,
    );

    let now = Utc::now();
    let active_incidents: Vec<DashboardActiveIncident> = active_raw
        .into_iter()
        .map(|i| DashboardActiveIncident::build(i, now))
        .collect();

    let matches = rows.len();
    let ribbon = build_fleet_ribbon(&ribbon_rows, ribbon_from, &target_names);
    Ok(DashboardSnapshot {
        rows: Arc::from(rows.into_boxed_slice()),
        kpi_cards: Arc::from(kpi_cards.into_boxed_slice()),
        matches,
        truncated,
        active_incidents: Arc::from(active_incidents.into_boxed_slice()),
        status_counts,
        type_counts: Arc::from(build_type_counts(type_acc).into_boxed_slice()),
        ribbon,
    })
}
