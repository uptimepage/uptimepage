//! Operator dashboard — V3 landing page. Replaces the donut+bar layout
//! with a dense per-monitor table backed by ONE batched ClickHouse
//! rollup + ONE batched 60-min sparkline query (both cached 5 s per
//! `(org, range)`). Scales to 1k+ monitors per org without N
//! round-trips per render.
//!
//! Hosts the `/` dispatcher too: a per-org status subdomain serves the
//! public status page, every other host falls through to the operator
//! dashboard. The branch lives here rather than as router-level
//! middleware because axum's `Router::layer` runs *after* path matching
//! — rewriting the URI in a layer would never re-route to `/status`.

use std::sync::Arc;

use askama::Template;
use axum::extract::{FromRequestParts, Query, State};
use axum::http::header;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use tower_cookies::Cookies;
use uuid::Uuid;

use crate::app::AppState;
use crate::domain::OrgId;
use crate::request::host::is_subdomain_public_request;
use crate::request::{AuthedBrowser, CurrentOrg, CurrentUser};
use crate::web::error::WebResult;
use crate::web::views::public_status::{self, StatusParams};
use crate::web::views::region_display::labeled_regions;
use crate::web::views::{build_range_options, resolve_range_key};

mod charts;
mod load;
mod rows;
#[cfg(test)]
mod tests;

pub(crate) use charts::{
    Polarity, count_delta, pct_label, render_spark_path, render_spark_path_domain, ribbon_class,
    uptime_pp_delta,
};
pub use rows::{
    DashboardActiveIncident, DashboardIncidentUpdate, DashboardKpis, DashboardPage,
    DashboardParams, DashboardRow, DashboardSnapshot, DashboardTablePartial, FleetRibbon,
    FleetRibbonSeg, KpiCardSpec, KpiDelta, StatusCounts, TypeCount,
};

use load::{build_snapshot, load_snapshot};

pub(crate) const RANGE_KEYS: [&str; 4] = ["24h", "7d", "30d", "90d"];
pub(crate) const DEFAULT_RANGE: &str = "24h";
pub(crate) const STATUS_FILTERS: [&str; 5] = ["any", "up", "degraded", "down", "paused"];
pub(crate) const TYPE_FILTERS: [&str; 9] = [
    "any",
    "http",
    "tcp",
    "ping",
    "heartbeat",
    "dns",
    "tls",
    "domain",
    "flow",
];
pub(crate) const FILTER_ANY: &str = "any";
/// Fixed sparkline window — "right-now" trend, decoupled from the
/// selected rollup range so an operator browsing 90 d still sees a
/// fresh 1 h trace per monitor.
const SPARK_MINUTES: i64 = 60;
const SPARK_BUCKETS: usize = SPARK_MINUTES as usize;
/// Fixed fleet ribbon: 24 h split into 48 × 30-minute cells. Window is
/// independent of the selected range so the ribbon always reads as
/// "last 24 h fleet health" regardless of the table's rollup span.
const RIBBON_HOURS: i64 = 24;
const RIBBON_BUCKETS: usize = 48;
const RIBBON_BUCKET_SECONDS: u32 = (RIBBON_HOURS as u32 * 3600) / RIBBON_BUCKETS as u32;
/// Names shown in a cell tooltip before "+N more"; the drill has the full set.
const DOWN_PREVIEW: usize = 6;
const _: () = assert!(
    (RIBBON_HOURS as u32 * 3600).is_multiple_of(RIBBON_BUCKETS as u32),
    "RIBBON_BUCKETS must divide the 24h window evenly so every second maps to a slot",
);
/// Cap rows on a single page render. Beyond this an org would benefit
/// from search/filter on `/targets`; rendering 5 k rows inline is a
/// browser-side hazard, not just a server cost.
const ROW_LIMIT: usize = 500;

pub async fn root(state: State<AppState>, mut parts: Parts) -> Response {
    let State(ref app_state) = state;
    if is_subdomain_public_request(app_state, &parts.headers) {
        // Preserve axum's standard `Query<T>` rejection — a malformed
        // `?fragment=` value used to 400 via the framework extractor, so a
        // bare `.unwrap_or(default)` here would silently turn invalid params
        // into a 200.
        let query = match Query::<StatusParams>::try_from_uri(&parts.uri) {
            Ok(q) => q,
            Err(rej) => return rej.into_response(),
        };
        return public_status::index(state, parts.headers, query).await;
    }
    // Operator dashboard. Re-run the extractors the routed `index`
    // would have run so rejection paths (login redirect, org error
    // envelope) stay byte-identical to a direct mount.
    let auth = match AuthedBrowser::from_request_parts(&mut parts, app_state).await {
        Ok(a) => a,
        Err(rej) => return rej.into_response(),
    };
    let org = match CurrentOrg::from_request_parts(&mut parts, app_state).await {
        Ok(o) => o,
        Err(rej) => return rej.into_response(),
    };
    let user = match CurrentUser::from_request_parts(&mut parts, app_state).await {
        Ok(u) => u,
        Err(rej) => return rej.into_response(),
    };
    let params = match Query::<DashboardParams>::try_from_uri(&parts.uri) {
        Ok(q) => q,
        Err(rej) => return rej.into_response(),
    };
    let cookies = match Cookies::from_request_parts(&mut parts, app_state).await {
        Ok(c) => c,
        Err(rej) => return rej.into_response(),
    };
    match index(auth, state, org, user, cookies, params).await {
        Ok(page) => page.into_response(),
        Err(e) => e.into_response(),
    }
}

pub async fn index(
    _auth: AuthedBrowser,
    State(state): State<AppState>,
    org: CurrentOrg,
    _user: CurrentUser,
    cookies: Cookies,
    Query(params): Query<DashboardParams>,
) -> WebResult<DashboardPage> {
    // One-shot post-login banners ride a flash cookie (consumed here) rather
    // than spoofable query params.
    let flash = crate::request::flash::take(&cookies, &state.cfg.auth.session.cookie_domain);
    let range = resolve_range_key(params.range.as_deref(), &RANGE_KEYS, DEFAULT_RANGE);
    let status = resolve_range_key(params.status.as_deref(), &STATUS_FILTERS, FILTER_ANY);
    let selected_status = (status != FILTER_ANY).then_some(status);
    let kind = resolve_range_key(params.kind.as_deref(), &TYPE_FILTERS, FILTER_ANY);
    let selected_kind = (kind != FILTER_ANY).then_some(kind);
    let region_ids = state.regions_for_org(org.0).await?;
    let selected_region = resolve_region(params.region, &region_ids);
    let snapshot = snapshot_for(&state, org.0, range, selected_region.as_deref()).await?;
    let catalog = state.regions_detailed().await?;
    let regions = labeled_regions(&catalog, region_ids);
    let onboarding = snapshot.matches == 0;
    let drill = params.down_at.and_then(|ts| resolve_drill(&snapshot, ts));
    let (rows, matches) = filter_rows(&snapshot, selected_status, selected_kind, drill.as_ref());
    // Banner only when the slug matches the ACTIVE org — a pasted/crafted
    // ?joined= for some other org renders nothing.
    let org_row = match state.db.as_ref() {
        Some(pool) if params.joined.is_some() => crate::storage::orgs::get_org(pool, org.0).await?,
        _ => None,
    };
    let joined_notice = params.joined.as_deref().and_then(|slug| {
        org_row
            .as_ref()
            .filter(|o| o.slug == slug)
            .map(|o| o.name.clone())
    });
    Ok(DashboardPage {
        active_tab: "dashboard",
        range,
        range_options: build_range_options(range, &RANGE_KEYS),
        kpi_cards: Arc::clone(&snapshot.kpi_cards),
        rows,
        matches,
        truncated: snapshot.truncated,
        onboarding,
        active_incidents: Arc::clone(&snapshot.active_incidents),
        status_counts: snapshot.status_counts,
        type_counts: type_chips(&snapshot, selected_status, selected_kind),
        ribbon: snapshot.ribbon.clone(),
        regions,
        selected_region,
        status_options: build_range_options(status, &STATUS_FILTERS),
        selected_status,
        selected_kind,
        drill: drill.map(|d| d.chip),
        restored_notice: flash.restored,
        joined_notice,
        invite_missed_notice: flash.invite_missed,
    })
}

/// An unknown region collapses to the all-regions view, not an empty dashboard.
pub(crate) fn resolve_region(requested: Option<String>, regions: &[String]) -> Option<String> {
    requested.filter(|r| regions.iter().any(|x| x == r))
}

/// The active ribbon-cell drill, surfaced to the template for the poll URL and
/// the clear chip. One field so the two can't drift out of lockstep.
#[derive(Clone)]
pub struct DrillChip {
    pub down_at: i64,
    pub label: String,
}

/// A ribbon-cell drill resolved against the cached snapshot: the chip plus the
/// set of monitors that dipped in the clicked window. `None` when the bucket
/// has aged out of the 24h ribbon or never had a dip.
struct Drill {
    chip: DrillChip,
    down: std::collections::HashSet<String>,
}

fn resolve_drill(snapshot: &DashboardSnapshot, down_at: i64) -> Option<Drill> {
    let seg = snapshot
        .ribbon
        .segs
        .iter()
        .find(|s| s.bucket_ts == down_at && !s.down_targets.is_empty())?;
    let label = DateTime::<Utc>::from_timestamp(down_at, 0)?
        .format("%H:%M")
        .to_string();
    Some(Drill {
        chip: DrillChip { down_at, label },
        down: seg.down_targets.iter().map(Uuid::to_string).collect(),
    })
}

/// Status, type and the ribbon drill are post-filters over the (cached)
/// snapshot: the table and its match count narrow while the KPI cards, health
/// rail, ribbon and chip counts stay fleet-wide, so the breakdown stays visible.
fn filter_rows(
    snapshot: &DashboardSnapshot,
    status: Option<&'static str>,
    kind: Option<&'static str>,
    drill: Option<&Drill>,
) -> (Arc<[DashboardRow]>, usize) {
    if status.is_none() && kind.is_none() && drill.is_none() {
        return (Arc::clone(&snapshot.rows), snapshot.matches);
    }
    let rows: Vec<DashboardRow> = snapshot
        .rows
        .iter()
        .filter(|r| status.is_none_or(|s| row_matches_status(r, s)))
        .filter(|r| kind.is_none_or(|k| r.kind.eq_ignore_ascii_case(k)))
        .filter(|r| drill.is_none_or(|d| d.down.contains(&r.id)))
        .cloned()
        .collect();
    let matches = rows.len();
    (Arc::from(rows.into_boxed_slice()), matches)
}

/// Chip strip for this request — the snapshot is cached across requests,
/// so neither `active` nor status-filtered counts can live there. Kinds
/// come from the fleet snapshot so the strip stays stable (a chip drops
/// to 0 under a status filter instead of vanishing while selected).
fn type_chips(
    snapshot: &DashboardSnapshot,
    status: Option<&'static str>,
    kind: Option<&'static str>,
) -> Arc<[TypeCount]> {
    if status.is_none() && kind.is_none() {
        // Unfiltered poll is the hot path — snapshot counts and the
        // "All" active flag are already correct, keep it a pointer bump.
        return Arc::clone(&snapshot.type_counts);
    }
    let count_for = |label: &'static str| {
        snapshot
            .rows
            .iter()
            .filter(|r| status.is_none_or(|s| row_matches_status(r, s)))
            .filter(|r| label == "All" || r.kind.eq_ignore_ascii_case(label))
            .count() as u32
    };
    let chips: Vec<TypeCount> = snapshot
        .type_counts
        .iter()
        .map(|c| TypeCount {
            label: c.label,
            count: match status {
                None => c.count,
                Some(_) => count_for(c.label),
            },
            active: match kind {
                None => c.label == "All",
                Some(k) => c.label.eq_ignore_ascii_case(k),
            },
        })
        .collect();
    Arc::from(chips.into_boxed_slice())
}

/// Mirrors `tally_status` bucketing so the filter agrees with the rail.
fn row_matches_status(row: &DashboardRow, status: &str) -> bool {
    match status {
        "paused" => !row.enabled,
        "down" => row.enabled && matches!(row.last_status, "down" | "error"),
        _ => row.enabled && row.last_status == status,
    }
}

/// All-regions is cached; a region-filtered view is built directly (selection is
/// rare, not worth widening the cache key).
async fn snapshot_for(
    state: &AppState,
    org: OrgId,
    range: &'static str,
    region: Option<&str>,
) -> WebResult<Arc<DashboardSnapshot>> {
    match region {
        Some(r) => Ok(Arc::new(build_snapshot(state, org, range, Some(r)).await?)),
        None => load_snapshot(state, org, range).await,
    }
}

/// htmx partial — the range-tab strip swaps just this fragment so a tab
/// click costs one CH query (cached) and a ~10 KB body, not a full
/// page. Returns `no-store` because the snapshot is time-relative.
pub async fn table_partial(
    _auth: AuthedBrowser,
    State(state): State<AppState>,
    org: CurrentOrg,
    Query(params): Query<DashboardParams>,
) -> WebResult<Response> {
    let range = resolve_range_key(params.range.as_deref(), &RANGE_KEYS, DEFAULT_RANGE);
    let status = resolve_range_key(params.status.as_deref(), &STATUS_FILTERS, FILTER_ANY);
    let selected_status = (status != FILTER_ANY).then_some(status);
    let kind = resolve_range_key(params.kind.as_deref(), &TYPE_FILTERS, FILTER_ANY);
    let selected_kind = (kind != FILTER_ANY).then_some(kind);
    let region_ids = state.regions_for_org(org.0).await?;
    let selected_region = resolve_region(params.region, &region_ids);
    let snapshot = snapshot_for(&state, org.0, range, selected_region.as_deref()).await?;
    let catalog = state.regions_detailed().await?;
    let regions = labeled_regions(&catalog, region_ids);
    let drill = params.down_at.and_then(|ts| resolve_drill(&snapshot, ts));
    let (rows, matches) = filter_rows(&snapshot, selected_status, selected_kind, drill.as_ref());
    let partial = DashboardTablePartial {
        range,
        range_options: build_range_options(range, &RANGE_KEYS),
        kpi_cards: Arc::clone(&snapshot.kpi_cards),
        rows,
        matches,
        truncated: snapshot.truncated,
        active_incidents: Arc::clone(&snapshot.active_incidents),
        status_counts: snapshot.status_counts,
        type_counts: type_chips(&snapshot, selected_status, selected_kind),
        ribbon: snapshot.ribbon.clone(),
        regions,
        selected_region,
        status_options: build_range_options(status, &STATUS_FILTERS),
        selected_status,
        selected_kind,
        drill: drill.map(|d| d.chip),
    };
    let rendered = partial.render().map_err(|e| {
        crate::web::error::WebError::from(crate::error::AppError::Other(anyhow::anyhow!(e)))
    })?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        rendered,
    )
        .into_response())
}
