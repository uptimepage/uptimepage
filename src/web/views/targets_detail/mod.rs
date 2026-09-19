use std::sync::Arc;

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use uuid::Uuid;

use crate::app::AppState;
use crate::domain::{CheckSpec, CheckStatus};
use crate::error::AppError;
use crate::error::codes;
use crate::storage::{HeartbeatMonitor, TimeRange};
use crate::web::error::WebResult;
use crate::web::filters;
use crate::web::views::coverage;
use crate::web::views::region_display::{LabeledRegion, labeled_regions};
use crate::web::views::{RangeOption, build_range_options, describe_check, resolve_range_key};
use crate::web::{AuthedBrowser, CurrentOrg};

use load::{
    FLAP_WINDOW_HOURS, LAST_RESULT_WINDOW_DAYS, alerts_nobody, flaps_by_region, load_flaps,
};
use rows::FLOW_RUNS_SHOWN;

mod charts;
mod load;
mod rows;
#[cfg(test)]
mod tests;

pub use charts::StatusSeg;
pub use load::{LiveData, PingTally, UnconfirmedFailures};
pub use rows::{
    DetailCheckRows, DetailLive, FlowEvidenceView, FlowRunRow, FlowStepRow, HeartbeatLiveness,
    IncidentRow, KpiTrend, RegionBreakdownRow, ResultRow, UptimeStatsView,
};

pub(crate) use rows::WindowLabels;

pub(crate) use load::{load_incidents_data, load_live_data_cached, ongoing_for_target};

// Recent rows for the share page's results table (the owner view dropped its
// table for the ribbon). 60 ≈ the last hour at a 1-minute interval.
const RESULTS_PAGE_LIMIT: usize = 60;

pub(crate) const RANGE_KEYS: [&str; 4] = ["1h", "24h", "7d", "30d"];
pub(crate) const DEFAULT_RANGE: &str = "24h";

pub(crate) const SUBTAB_MONITOR: &str = "monitor";
pub(crate) const SUBTAB_INCIDENTS: &str = "incidents";

pub(crate) const INCIDENT_RANGE_KEYS: [&str; 4] = ["24h", "7d", "30d", "90d"];
pub(crate) const INCIDENT_DEFAULT_RANGE: &str = "30d";
const INCIDENTS_PAGE_LIMIT: usize = 100;

#[derive(Debug, Default, Deserialize)]
pub struct DetailParams {
    #[serde(default)]
    pub range: Option<String>,
    #[serde(default)]
    pub from: Option<DateTime<Utc>>,
    #[serde(default)]
    pub to: Option<DateTime<Utc>>,
    #[serde(default)]
    pub region: Option<String>,
}

#[derive(Template, WebTemplate)]
#[template(path = "targets/incidents.html")]
pub struct IncidentsPage {
    pub active_tab: &'static str,
    pub subtab: &'static str,
    pub ongoing_count: usize,
    pub id: String,
    pub name: String,
    pub kind: &'static str,
    pub address: String,
    pub interval_s: u64,
    pub enabled: bool,
    pub tags: Vec<String>,
    /// `terraform`/`api` chip for externally-managed monitors; `None` (UI) hides it.
    pub managed_by: Option<&'static str>,
    /// Count of live (non-revoked) share links; drives the header "shared" chip.
    pub share_count: usize,
    /// No bound channel and no effective escalation policy: this monitor
    /// detects outages and tells no one. Drives the header warning.
    pub alerts_nobody: bool,
    pub last_status: &'static str,
    pub last_at_iso: String,
    pub incidents: Vec<IncidentRow>,
    pub incidents_has_more: bool,
    /// URL prefix incidents_tab.js appends `/results` to for the timeline
    /// drawer (`/api/v1/targets/{id}`).
    pub results_base: String,
    pub range: &'static str,
    pub range_options: Vec<RangeOption>,
    pub range_base_path: String,
    pub from_iso: String,
    pub to_iso: String,
    pub from_human: String,
    pub to_human: String,
    /// Always `None` here — the incidents tab is not region-filtered yet; the
    /// field exists so the shared range-pills partial type-checks.
    pub selected_region: Option<String>,
    /// `None` when the window was clean, or when incidents already explain it.
    pub unconfirmed: Option<UnconfirmedFailures>,
}

#[derive(Template, WebTemplate)]
#[template(path = "targets/detail.html")]
pub struct DetailPage {
    pub active_tab: &'static str,
    /// Sub-tab strip selector under the monitor header. `"monitor"` on
    /// this view; the Incidents page renders the same partial with
    /// `"incidents"`.
    pub subtab: &'static str,
    pub ongoing_count: usize,
    pub id: String,
    pub name: String,
    pub kind: &'static str,
    pub address: String,
    pub interval_s: u64,
    pub enabled: bool,
    pub tags: Vec<String>,
    /// `terraform`/`api` chip for externally-managed monitors; `None` (UI) hides it.
    pub managed_by: Option<&'static str>,
    /// Count of live (non-revoked) share links; drives the header "shared" chip.
    pub share_count: usize,
    /// No bound channel and no effective escalation policy: this monitor
    /// detects outages and tells no one. Drives the header warning.
    pub alerts_nobody: bool,
    /// Opens counted in the flap window when the monitor is over the
    /// threshold; `None` when it is not flapping. Drives the banner that
    /// explains why repeat alerts have gone quiet.
    pub flapping_opens: Option<u32>,
    /// Minutes an outage must last before a held alert pages anyway.
    pub flap_hold_minutes: u64,
    pub last_status: &'static str,
    /// ISO 8601 timestamp of the most recent check, "" when none. Drives
    /// the client-side "checked Ns ago · next in Ns" ticker.
    pub last_at_iso: Arc<str>,
    pub uptime: Arc<UptimeStatsView>,
    pub kpi: Arc<KpiTrend>,
    pub pings: Option<PingTally>,
    /// Per-bucket status strip over the selected range, rendered under the header.
    pub segments: Arc<[StatusSeg]>,
    /// Ribbon include renders in place (not an OOB swap) on the full page.
    pub ribbon_oob: bool,
    /// Registrable domain a `domain_expiry` monitor queries; `None` otherwise.
    pub registered_domain: Option<String>,
    /// `None` once the host is fully covered, and the panel vanishes with it.
    pub coverage: Option<coverage::CoveragePanel>,
    pub config_json: String,
    pub range: &'static str,
    pub range_options: Vec<RangeOption>,
    pub range_base_path: String,
    pub from_iso: String,
    pub to_iso: String,
    pub from_human: String,
    pub to_human: String,
    /// Distinct regions this org's targets run in; drives the region selector,
    /// rendered only when there is more than one.
    pub regions: Vec<LabeledRegion>,
    pub selected_region: Option<String>,
    /// Per-region rollup rows; empty for single-region orgs (table hidden).
    pub region_breakdown: Vec<RegionBreakdownRow>,
    /// Ping-URL card for heartbeat monitors; `None` for every other kind.
    /// Shares the API handler's projection so the two surfaces can't diverge.
    pub heartbeat: Option<crate::api::handlers::targets::HeartbeatInfo>,
    pub liveness: Option<HeartbeatLiveness>,
    /// The notice renders in place here, not as an OOB swap.
    pub liveness_oob: bool,
    /// Stored runs, newest first. Empty for every kind but flow, which is what
    /// keeps the panel and its query off the other seven.
    pub flow_runs: Vec<FlowRunRow>,
}

pub async fn index(
    _auth: AuthedBrowser,
    CurrentOrg(org): CurrentOrg,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(params): Query<DetailParams>,
) -> WebResult<DetailPage> {
    let target = state
        .target_store
        .get(org, id)
        .await?
        .ok_or_else(|| AppError::not_found(codes::TARGET_NOT_FOUND, "monitor not found"))?;

    let range_key = resolve_range_key(params.range.as_deref(), &RANGE_KEYS, DEFAULT_RANGE);
    let (from, to) = resolve_window(range_key, params.from, params.to);
    let labels = WindowLabels::new(from, to);
    let region_ids = state.regions_for_org(org).await?;
    let selected_region = super::dashboard::resolve_region(params.region, &region_ids);
    let catalog = state.regions_detailed().await?;
    let live = load_live_data_cached(
        &state,
        org,
        &target,
        range_key,
        params.from,
        params.to,
        selected_region.as_deref(),
    )
    .await?;
    let base_path = format!("/targets/{}", target.id);
    let region_breakdown = if region_ids.len() > 1 && !target.check.is_passive() {
        let live: Option<std::collections::HashSet<String>> = state
            .silence_store
            .live_regions(state.cfg.operator.agent_stale_after_secs)
            .await
            .ok()
            .map(|v| v.into_iter().collect());
        let flaps = flaps_by_region(&state, org, target.id).await;
        state
            .results_store
            .region_breakdown(org, target.id, TimeRange { from, to })
            .await?
            .into_iter()
            .map(|r| {
                let region_flaps = flaps.get(&r.region).copied().unwrap_or(0);
                RegionBreakdownRow::from_rollup(
                    r,
                    selected_region.as_deref(),
                    &catalog,
                    live.as_ref(),
                    &base_path,
                    range_key,
                    region_flaps,
                )
            })
            .collect()
    } else {
        Vec::new()
    };
    let ongoing_count = ongoing_for_target(&state, org, target.id).await;
    let registered_domain = match &target.check {
        CheckSpec::DomainExpiry(d) => d.reduced_domain_hint(),
        _ => None,
    };
    let config_json = config_json_with_derived(&target.check, registered_domain.as_deref())?;
    let coverage = {
        let covered = state
            .target_store
            .hosts_by_kind(org, &coverage::COVERAGE_KINDS)
            .await?;
        coverage::panel(&target.check, &covered)
    };
    let (kind, address) = describe_check(&target.check);
    let share_count = state
        .monitor_share_store
        .count_active_for_target(org, target.id)
        .await? as usize;
    let flow_runs = if matches!(target.check, CheckSpec::Flow(_)) {
        // Same window the rest of the page is showing, clamped to what the plan
        // retains, so the panel never claims history the charts do not have.
        let window = state
            .quotas
            .clamp_raw(
                org,
                TimeRange {
                    from,
                    to: to.min(Utc::now()),
                },
            )
            .await?;
        state
            .results_store
            .flow_runs(
                org,
                target.id,
                window,
                selected_region.as_deref(),
                FLOW_RUNS_SHOWN,
            )
            .await?
            .into_iter()
            .map(FlowRunRow::from_view)
            .collect()
    } else {
        Vec::new()
    };
    // The ping card is one panel on a page that still has uptime, incidents and
    // history to show, and the incidents tab already degrades on this same read.
    // The API endpoint keeps propagating: a caller asking for the token must
    // hear that the answer is unavailable, not receive one without it.
    let heartbeat = match target.check.as_heartbeat() {
        Some(check) => crate::api::handlers::targets::heartbeat_info(
            &state,
            org,
            target.id,
            check,
            target.enabled,
        )
        .await
        .map_err(|err| tracing::warn!(error = %err, "heartbeat card unavailable"))
        .ok(),
        None => None,
    };
    // Only counted when the damper is on, so the banner never promises a hold
    // that is not happening.
    let flap_cfg = &state.cfg.escalation;
    let flapping_opens = match flap_cfg.flap_max_opens {
        0 => None,
        max => {
            let since =
                Utc::now() - chrono::Duration::seconds(flap_cfg.flap_window_secs.max(1) as i64);
            match state
                .incident_ops_store
                .opens_since(org, target.id, since)
                .await
            {
                // Above, not at: the crossing open still pages.
                Ok(opens) if opens > max => Some(opens),
                _ => None,
            }
        }
    };

    let reaches_nobody = alerts_nobody(&state, org, &target).await;

    // Passive kinds have no probe region, so no region selector.
    let regions = if target.check.is_passive() {
        Vec::new()
    } else {
        labeled_regions(&catalog, region_ids)
    };

    // From the projection, so a ping landing mid-render cannot split the page.
    let liveness = heartbeat
        .as_ref()
        .map(|h| HeartbeatLiveness::derive(h, Utc::now(), target.enabled));
    // A region-filtered page shows that region's own results, so the badge
    // above them must not fold across the regions it is hiding.
    let folded = selected_region
        .is_none()
        .then(|| fold_breakdown(&region_breakdown, target.region_policy))
        .flatten();
    let last_status = badge_status(folded.unwrap_or(live.last_status), liveness.as_ref());

    Ok(DetailPage {
        active_tab: "targets",
        subtab: SUBTAB_MONITOR,
        ongoing_count,
        id: target.id.to_string(),
        name: target.name,
        kind,
        address,
        interval_s: target.interval.as_secs(),
        enabled: target.enabled,
        tags: target.tags,
        managed_by: target.write_source.managed_label(),
        share_count,
        alerts_nobody: reaches_nobody,
        flapping_opens,
        flap_hold_minutes: state.cfg.escalation.flap_hold_secs.div_ceil(60),
        last_status,
        last_at_iso: Arc::clone(&live.last_at_iso),
        uptime: Arc::clone(&live.uptime),
        kpi: Arc::clone(&live.kpi),
        pings: live.pings,
        segments: Arc::clone(&live.segments),
        ribbon_oob: false,
        registered_domain,
        coverage,
        config_json,
        range: range_key,
        range_options: build_range_options(range_key, &RANGE_KEYS),
        range_base_path: base_path,
        from_iso: labels.from_iso,
        to_iso: labels.to_iso,
        from_human: labels.from_human,
        to_human: labels.to_human,
        regions,
        selected_region,
        region_breakdown,
        liveness,
        liveness_oob: false,
        heartbeat,
        flow_runs,
    })
}

#[derive(Debug, Deserialize)]
pub struct CheckRowsParams {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    #[serde(default)]
    pub offset: usize,
    /// Mirrors the detail view's region filter so the drawer lists the same
    /// region the ribbon cell's counts were measured over.
    pub region: Option<String>,
}

// One 30-row page is plenty per drill; the drawer pages with `offset` for more.
const CHECK_ROWS_PAGE: usize = 30;

/// HTML rows for the ribbon drill drawer: the failing checks over a bucket
/// window, each tagged with its region, rendered with the shared recent-results
/// row partial. Paginates by `offset`; `X-Sm-Has-More` tells the drawer whether
/// to keep its "load more" control. A wide bucket can hold thousands of failures,
/// so the caller only ever pulls one bounded page at a time.
pub async fn check_rows(
    _auth: AuthedBrowser,
    CurrentOrg(org): CurrentOrg,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(params): Query<CheckRowsParams>,
) -> WebResult<Response> {
    // Cloak an unknown/foreign id behind the same 404 the other detail reads use.
    let target = state
        .target_store
        .get(org, id)
        .await?
        .ok_or_else(|| AppError::not_found(codes::TARGET_NOT_FOUND, "monitor not found"))?;

    let range = state
        .quotas
        .clamp_raw(
            org,
            TimeRange {
                from: params.from,
                to: params.to,
            },
        )
        .await?;
    // The drawer only opens on a failing cell, so list only the failing checks —
    // success rows would bury them. `region` matches the ribbon's filter. Counts
    // come from the rollup and these rows from raw, so they can diverge at a TTL
    // or bucket edge; the count is the headline, not a row total.
    let mut rows = state
        .results_store
        .list_failures_by_region(
            org,
            target.id,
            range,
            CHECK_ROWS_PAGE + 1,
            params.offset,
            params.region.as_deref(),
        )
        .await?;
    let has_more = rows.len() > CHECK_ROWS_PAGE;
    if has_more {
        rows.truncate(CHECK_ROWS_PAGE);
    }
    let results: Arc<[ResultRow]> = rows
        .into_iter()
        .map(|(region, r)| ResultRow::with_region(region, r))
        .collect::<Vec<_>>()
        .into();

    let rendered = DetailCheckRows {
        results,
        show_region: true,
        show_guidance: true,
    }
    .render()
    .map_err(|e| AppError::Other(anyhow::anyhow!(e)))?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (
                header::HeaderName::from_static("x-sm-has-more"),
                if has_more { "true" } else { "false" },
            ),
        ],
        rendered,
    )
        .into_response())
}

/// Pretty check config for the panel, with a read-only `registered_domain`
/// added when a `domain_expiry` input reduces, so the displayed input isn't
/// rewritten.
fn config_json_with_derived(
    check: &CheckSpec,
    registered_domain: Option<&str>,
) -> Result<String, AppError> {
    // The panel shows the config to any org member, so mask credential inputs
    // (HTTP auth, flow fill values) the same way the API and share views do.
    let mut check = check.clone();
    crate::security::redaction::redact_check(&mut check);
    let mut value =
        serde_json::to_value(&check).map_err(|e| AppError::Other(anyhow::anyhow!(e)))?;
    if let (Some(rd), Some(object)) = (registered_domain, value.as_object_mut()) {
        object.insert("registered_domain".into(), rd.into());
    }
    serde_json::to_string_pretty(&value).map_err(|e| AppError::Other(anyhow::anyhow!(e)))
}

/// htmx-polled fragment that re-renders the KPI cards + recent-results
/// table. Byte-identical to the section the full page initially served
/// so swap is invisible. Reads from `live_data_cache` (5s TTL); a
/// burst of pollers + the full-page handler share the same snapshot.
/// Custom `from`/`to` query params skip the cache.
pub async fn live_partial(
    _auth: AuthedBrowser,
    CurrentOrg(org): CurrentOrg,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(params): Query<DetailParams>,
) -> WebResult<Response> {
    let target = state
        .target_store
        .get(org, id)
        .await?
        .ok_or_else(|| AppError::not_found(codes::TARGET_NOT_FOUND, "monitor not found"))?;

    let range_key = resolve_range_key(params.range.as_deref(), &RANGE_KEYS, DEFAULT_RANGE);
    // The poll only echoes the region into its own URL — it doesn't render the
    // selector — so it trusts the region the full page already validated rather
    // than re-running regions_for_org every tick. An unknown value just filters
    // to empty (org-scoped), corrected on the next full load.
    let selected_region = params.region;
    let live = load_live_data_cached(
        &state,
        org,
        &target,
        range_key,
        params.from,
        params.to,
        selected_region.as_deref(),
    )
    .await?;

    let liveness = read_liveness(&state, org, &target).await;
    let last_status = badge_status(live.last_status, liveness.as_ref());

    let page = DetailLive {
        id: target.id.to_string(),
        name: target.name,
        range: range_key,
        enabled: target.enabled,
        last_status,
        uptime: Arc::clone(&live.uptime),
        kpi: Arc::clone(&live.kpi),
        pings: live.pings,
        last_at_iso: Arc::clone(&live.last_at_iso),
        selected_region,
        segments: Arc::clone(&live.segments),
        ribbon_oob: true,
        liveness,
        liveness_oob: true,
    };
    let rendered = page
        .render()
        .map_err(|e| AppError::Other(anyhow::anyhow!(e)))?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            // The htmx poll URL is time-relative ("range=24h" → window
            // relative to now). Browser cache would silently serve
            // stale rows even after the server's 5s TTL elapsed.
            (header::CACHE_CONTROL, "no-store"),
        ],
        rendered,
    )
        .into_response())
}

/// Maps a preset range key to a window. Explicit `from`/`to` query params
/// override the preset (custom range case). Covers every key from both
/// `RANGE_KEYS` and `INCIDENT_RANGE_KEYS` so the shared cached loader
/// can serve either tab without falling back to the default branch.
pub(crate) fn resolve_window(
    key: &'static str,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
) -> (DateTime<Utc>, DateTime<Utc>) {
    let to = to.unwrap_or_else(Utc::now);
    let span = match key {
        "1h" => Duration::hours(1),
        "7d" => Duration::days(7),
        "30d" => Duration::days(30),
        "90d" => Duration::days(90),
        _ => Duration::hours(24),
    };
    (from.unwrap_or(to - span), to)
}

/// Fallback window for the "current health" badge when the user-picked
/// chart range is empty. Returns `None` when the user's range already
/// covers the last-result window so callers can skip a redundant query.
fn wider_status_window(from: DateTime<Utc>, to: DateTime<Utc>) -> Option<TimeRange> {
    let widened = to - Duration::days(LAST_RESULT_WINDOW_DAYS);
    if from <= widened {
        None
    } else {
        Some(TimeRange { from: widened, to })
    }
}

pub(crate) use crate::domain::humanize_check_error as fmt_error_display;

pub async fn incidents(
    _auth: AuthedBrowser,
    CurrentOrg(org): CurrentOrg,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(params): Query<DetailParams>,
) -> WebResult<IncidentsPage> {
    let target = state
        .target_store
        .get(org, id)
        .await?
        .ok_or_else(|| AppError::not_found(codes::TARGET_NOT_FOUND, "monitor not found"))?;

    let range_key = resolve_range_key(
        params.range.as_deref(),
        &INCIDENT_RANGE_KEYS,
        INCIDENT_DEFAULT_RANGE,
    );
    let (from, to) = resolve_incident_window(range_key, params.from, params.to);
    let time_range = TimeRange { from, to };
    let labels = WindowLabels::new(from, to);
    let data = load_incidents_data(&state, org, target.id, time_range).await?;
    // Compared against the flap window, not the page range: an incident 29 days
    // ago explains nothing about failures from this morning.
    let flap_cutoff = Utc::now() - chrono::Duration::hours(FLAP_WINDOW_HOURS);
    let unconfirmed =
        if !explained_by_incident(&data.rows, flap_cutoff) && !target.check.is_passive() {
            let catalog = state.regions_detailed().await.unwrap_or_default();
            UnconfirmedFailures::new(
                &load_flaps(&state, org, target.id).await,
                &catalog,
                target.alert_confirmations,
                target.region_policy,
            )
        } else {
            None
        };
    let (kind, address) = describe_check(&target.check);
    let share_count = state
        .monitor_share_store
        .count_active_for_target(org, target.id)
        .await? as usize;
    let reaches_nobody = alerts_nobody(&state, org, &target).await;
    let last_status = badge_status(
        data.last_status,
        read_liveness(&state, org, &target).await.as_ref(),
    );

    Ok(IncidentsPage {
        active_tab: "targets",
        subtab: SUBTAB_INCIDENTS,
        ongoing_count: data.ongoing_count,
        id: target.id.to_string(),
        name: target.name,
        kind,
        address,
        interval_s: target.interval.as_secs(),
        enabled: target.enabled,
        tags: target.tags,
        managed_by: target.write_source.managed_label(),
        share_count,
        alerts_nobody: reaches_nobody,
        last_status,
        last_at_iso: data.last_at_iso,
        incidents: data.rows,
        incidents_has_more: data.has_more,
        results_base: format!("/api/v1/targets/{}", target.id),
        range: range_key,
        range_options: build_range_options(range_key, &INCIDENT_RANGE_KEYS),
        range_base_path: format!("/targets/{}/incidents", target.id),
        from_iso: labels.from_iso,
        to_iso: labels.to_iso,
        from_human: labels.from_human,
        to_human: labels.to_human,
        selected_region: None,
        unconfirmed,
    })
}

/// An unwired heartbeat is not being checked, so the badge must not borrow the
/// "checking…" an empty status renders for probed monitors.
const WAITING_FOR_PING: &str = "waiting";

/// The evaluator calls this up, correctly. The page derives it rather than
/// storing a status the alerting path would then see.
const LATE_FOR_PING: &str = "late";

/// The incidents tab has no ping card, so it reads the row itself. `None` for
/// every other kind, and on a read error.
pub(crate) async fn read_liveness(
    state: &AppState,
    org: crate::domain::OrgId,
    target: &crate::domain::Target,
) -> Option<HeartbeatLiveness> {
    let check = target.check.as_heartbeat()?;
    let hb = match state.heartbeat_store.get(org, target.id).await {
        Ok(hb) => hb,
        Err(err) => {
            tracing::warn!(error = %err, "heartbeat state unavailable");
            return None;
        }
    };
    let now = Utc::now();
    let window = hb
        .as_ref()
        .filter(|h| target.enabled && !h.is_pending() && h.ping_state().failing().is_none())
        .map(|h| check.window(h.ping_state().success_at));
    Some(HeartbeatLiveness {
        pending: target.enabled && hb.as_ref().is_none_or(HeartbeatMonitor::is_pending),
        since: hb.map(|h| h.created_at),
        due_at: window.map(|(due, _)| due),
        down_at: window
            .map(|(_, down)| down)
            .filter(|d| window.is_some_and(|(due, _)| *d != due)),
        late: window.is_some_and(|(due, down)| now > due && now <= down),
        overdue: window.is_some_and(|(_, down)| now > down),
    })
}

/// Folds the per-region table below the badge, so the headline cannot
/// contradict the breakdown it sits above. `None` leaves the raw last status.
fn fold_breakdown(
    breakdown: &[RegionBreakdownRow],
    policy: crate::domain::RegionIncidentPolicy,
) -> Option<&'static str> {
    if breakdown.is_empty() {
        return None;
    }
    let reported = breakdown
        .iter()
        .filter_map(|r| CheckStatus::from_label(&r.last_status));
    policy.fold_regions(reported).map(CheckStatus::as_str)
}

/// Every other kind gets its stored status back untouched.
pub(crate) fn badge_status(
    last_status: &'static str,
    hb: Option<&HeartbeatLiveness>,
) -> &'static str {
    match hb {
        Some(h) if h.pending && last_status.is_empty() => WAITING_FOR_PING,
        Some(h) if h.late && matches!(last_status, "up" | "") => LATE_FOR_PING,
        _ => last_status,
    }
}

/// Whether a listed incident accounts for the flap window. Incidents older than
/// it explain nothing about failures inside it, which is the case the block
/// exists for.
fn explained_by_incident(rows: &[IncidentRow], cutoff: DateTime<Utc>) -> bool {
    rows.iter()
        .any(|i| i.ended_at.is_none() || i.started_at >= cutoff)
}

pub(crate) fn resolve_incident_window(
    key: &'static str,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
) -> (DateTime<Utc>, DateTime<Utc>) {
    let to = to.unwrap_or_else(Utc::now);
    let span = match key {
        "24h" => Duration::hours(24),
        "7d" => Duration::days(7),
        "90d" => Duration::days(90),
        _ => Duration::days(30),
    };
    (from.unwrap_or(to - span), to)
}
