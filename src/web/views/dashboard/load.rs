//! Snapshot assembly: the cached batched reads behind one dashboard render.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{Duration, Utc};
use moka::sync::Cache;
use uuid::Uuid;

use crate::app::AppState;
use crate::domain::metrics::{DashboardMetrics, PriorPeriodSummary};
use crate::domain::{CheckStatus, OrgId, UserId};
use crate::storage::{
    IncidentBriefFilter, IncidentSpan, TargetFilter, TimeRange, weighted_downtime_by_target,
};
use crate::web::error::WebResult;
use crate::web::views::describe_check;
use crate::web::views::incidents::members_map;

use super::charts::{
    ACTIVE_INCIDENTS_LIMIT, ConfirmedRibbon, TYPE_CHIP_ORDER, avg_response_label,
    build_fleet_ribbon, build_kpi_cards, build_type_counts, confirmed_uptime_series, fleet_sparks,
    fleet_uptime_pct, format_count, group_sparks, pct_label, range_span, ribbon_from, tally_status,
    uptime_delta, uptime_pp_delta,
};
use super::*;

/// The banner's open incidents, read fresh on every render: it is where
/// people act on them, so it must not trail the action behind the snapshot
/// cache.
pub(super) async fn active_incidents(
    state: &AppState,
    org: OrgId,
    viewer: UserId,
) -> WebResult<Arc<[DashboardActiveIncident]>> {
    let briefs = state
        .incident_narration_store
        .list_briefs(
            org,
            IncidentBriefFilter {
                oldest_first: true,
                limit: ACTIVE_INCIDENTS_LIMIT,
                ..Default::default()
            },
        )
        .await?;
    if briefs.is_empty() {
        return Ok(Arc::from([]));
    }
    let ids: Vec<Uuid> = briefs.iter().map(|b| b.id).collect();
    let (acks, members) = tokio::try_join!(
        async { Ok(state.incident_ops_store.acknowledgements(org, &ids).await?) },
        members_map(state, org),
    )?;
    let now = Utc::now();
    Ok(briefs
        .into_iter()
        .map(|b| {
            let incident_acks = acks.get(&b.id).map(Vec::as_slice).unwrap_or_default();
            DashboardActiveIncident::build(b, incident_acks, viewer, now, &members)
        })
        .collect())
}

pub type DashboardPageCache = Cache<(OrgId, &'static str), Arc<DashboardSnapshot>>;

pub fn dashboard_page_cache() -> DashboardPageCache {
    // A page entry holds a row plus ~60 sparkline buckets per monitor: 1024 orgs x 4 ranges.
    Cache::builder()
        .time_to_live(std::time::Duration::from_secs(5))
        .max_capacity(4_096)
        .build()
}

/// Cached front door — both `index` and `table_partial` reach the same
/// `Arc<DashboardSnapshot>` so a tab-spam burst collapses to one CH
/// round-trip. The cache itself enforces the 5 s TTL.
pub(super) async fn load_snapshot(
    state: &AppState,
    cache: &DashboardPageCache,
    org: OrgId,
    range: &'static str,
) -> WebResult<Arc<DashboardSnapshot>> {
    if let Some(snap) = cache.get(&(org, range)) {
        return Ok(snap);
    }
    let snap = Arc::new(build_snapshot(state, org, range, None).await?);
    cache.insert((org, range), Arc::clone(&snap));
    Ok(snap)
}

/// What the all-regions view reads from incidents: the downtime spans over the
/// period, the one before it and the ribbon's day, read once, with the
/// monitors checked in the ribbon's day and in the prior period. A region view
/// paints from raw checks, so it reads none of it.
struct ConfirmedReads {
    spans: Vec<IncidentSpan>,
    ribbon: TimeRange,
    ribbon_checked: HashSet<Uuid>,
    prior: TimeRange,
    prior_checked: HashSet<Uuid>,
}

async fn confirmed_reads(
    state: &AppState,
    org: OrgId,
    period: TimeRange,
    region: Option<&str>,
) -> crate::error::Result<Option<ConfirmedReads>> {
    if region.is_some() {
        return Ok(None);
    }
    let ribbon = TimeRange {
        from: period.to - Duration::hours(RIBBON_HOURS),
        to: period.to,
    };
    let prior = TimeRange {
        from: period.from - (period.to - period.from),
        to: period.from,
    };
    let all = TimeRange {
        from: prior.from.min(ribbon.from),
        to: period.to,
    };
    let (spans, ribbon_checked, prior_checked) = tokio::try_join!(
        state
            .incident_narration_store
            .downtime_spans(org, all, None),
        state
            .results_store
            .sampled_targets(org, ribbon.from, ribbon.to),
        state
            .results_store
            .sampled_targets(org, prior.from, prior.to),
    )?;
    Ok(Some(ConfirmedReads {
        spans,
        ribbon,
        ribbon_checked,
        prior,
        prior_checked,
    }))
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
        (checks_total, checks_up, avg_ms_current),
        incidents,
        ribbon_rows,
        confirmed_reads,
        prior,
    ) = tokio::try_join!(
        state.target_store.list(org, target_filter),
        state
            .results_store
            .dashboard_rollup(org, time_range, region),
        state
            .results_store
            .dashboard_sparkline(org, spark_from, to, region),
        state.results_store.last_n_summary(org, time_range, region),
        async {
            match region {
                None => {
                    state
                        .incident_narration_store
                        .count_overlapping(org, time_range)
                        .await
                }
                Some(r) => {
                    state
                        .results_store
                        .failure_streaks(org, time_range, r)
                        .await
                }
            }
        },
        state
            .results_store
            .fleet_ribbon(org, ribbon_from(to), to, RIBBON_BUCKET_SECONDS, region),
        confirmed_reads(state, org, time_range, region),
        state
            .results_store
            .prior_period_summary(org, time_range, region),
    )?;

    let window_secs = (time_range.to - time_range.from).num_seconds();
    // A region filter keeps the raw per-region rate; only all-regions is confirmed.
    let confirmed = region.is_none();
    let downtime_by_target: HashMap<Uuid, i64> = confirmed_reads
        .as_ref()
        .map(|c| weighted_downtime_by_target(&c.spans, time_range))
        .unwrap_or_default();
    let prior_uptime = confirmed_reads.as_ref().and_then(|c| {
        let downtime = weighted_downtime_by_target(&c.spans, c.prior);
        fleet_uptime_pct(&c.prior_checked, &downtime, window_secs)
    });
    let uptime_series = confirmed_reads
        .as_ref()
        .map(|c| confirmed_uptime_series(&spark_rows, &c.spans, spark_from));
    let ribbon_confirmed = confirmed_reads.map(|c| ConfirmedRibbon {
        window: c.ribbon,
        spans: c
            .spans
            .into_iter()
            .filter(|s| s.clip(c.ribbon).is_some())
            .collect(),
        sampled: c.ribbon_checked,
    });

    let truncated = targets.len() > ROW_LIMIT;
    if truncated {
        targets.truncate(ROW_LIMIT);
    }

    // Only monitors that dipped or had an incident are named in the ribbon
    // tooltip, so clone names for those ids alone rather than the whole fleet.
    let down_ids: std::collections::HashSet<Uuid> = ribbon_rows
        .iter()
        .flat_map(|r| r.down_targets.iter().copied())
        .chain(
            ribbon_confirmed
                .iter()
                .flat_map(|c| c.spans.iter().map(|s| s.target_id)),
        )
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
        crate::targets::folded_status(
            state.results_store.as_ref(),
            org,
            time_range,
            crate::targets::folded_status_policies(&targets),
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

    let mut fleet_sparks = fleet_sparks(&spark_rows, spark_from);
    if let Some(series) = uptime_series {
        fleet_sparks.uptime = series;
    }
    let checks_successful_label = format!("{} successful", format_count(checks_up));
    let current = PriorPeriodSummary {
        checks_total,
        checks_up,
        avg_ms: avg_ms_current,
    };
    // Time-weighted over sampled monitors, plus any with downtime and no
    // checks, so the fleet KPI matches the rows.
    let fleet_uptime = confirmed
        .then(|| {
            let sampled = metrics_by_target
                .iter()
                .filter(|(_, m)| m.samples > 0)
                .map(|(id, _)| *id)
                .collect();
            fleet_uptime_pct(&sampled, &downtime_by_target, window_secs)
        })
        .flatten();
    let fleet_uptime_label = fleet_uptime.map_or_else(
        || pct_label(checks_total, checks_up),
        |p| format!("{p:.2}%"),
    );
    let uptime_delta = if confirmed {
        fleet_uptime
            .zip(prior_uptime)
            .map(|(cur, prior)| uptime_pp_delta(cur, prior))
    } else {
        uptime_delta(&current, &prior)
    };
    let kpis = DashboardKpis {
        uptime_pct_label: fleet_uptime_label,
        avg_response_ms_label: avg_response_label(avg_ms_current, checks_total),
        checks_label: format_count(checks_total),
        checks_successful_label: checks_successful_label.clone(),
        incidents_label: if confirmed {
            "Incidents"
        } else {
            "Failure streaks"
        },
        incidents,
    };
    let kpi_cards = build_kpi_cards(
        &kpis,
        range,
        checks_successful_label,
        &current,
        &prior,
        uptime_delta,
        &fleet_sparks,
    );

    let matches = rows.len();
    let ribbon = build_fleet_ribbon(&ribbon_rows, to, &target_names, ribbon_confirmed.as_ref());
    Ok(DashboardSnapshot {
        rows: Arc::from(rows.into_boxed_slice()),
        kpi_cards: Arc::from(kpi_cards.into_boxed_slice()),
        matches,
        truncated,
        status_counts,
        type_counts: Arc::from(build_type_counts(type_acc).into_boxed_slice()),
        ribbon,
    })
}
