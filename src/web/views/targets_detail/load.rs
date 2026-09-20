use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use crate::app::AppState;
use crate::domain::{Incident, OrgId, Target, confirmed_downtime_secs, uptime_pct_from_downtime};
use crate::storage::{ClampedRange, TimeRange};
use crate::templates::format::fmt_ts;
use crate::web::error::{WebError, WebResult};

use super::charts::{
    KpiInputs, SPARK_SEGMENTS, StatusSeg, bucket_counts, build_kpi_trend, status_segments,
};
use super::rows::{IncidentRow, KpiTrend, ResultRow, UptimeStatsView};
use super::{INCIDENTS_PAGE_LIMIT, RESULTS_PAGE_LIMIT, resolve_window, wider_status_window};

// Confirmed incidents over the longest window (90d) are few; this bounds the
// downtime read without truncating a realistic count.
const CONFIRMED_UPTIME_INCIDENT_CAP: usize = 2000;

// Decoupled from the user's chart range so the header badge reflects the
// monitor's actual current state, not "no data" when the user picked 1h
// but the last check was 2h ago.
pub(super) const LAST_RESULT_WINDOW_DAYS: i64 = 7;

/// Open incidents for this monitor. Counted, never inferred from the last
/// check: a monitor that fails once and recovers opens no incident, so
/// inferring would badge a tab with nothing in it. That accuracy costs one
/// indexed round trip per render, which the inferred version deliberately
/// avoided — worth it only because the inferred answer was wrong often enough
/// to send readers to an empty tab. A store error reads as 0, so the banner
/// hedges rather than asserting an incident it could not confirm.
pub(crate) async fn ongoing_for_target(state: &AppState, org: OrgId, target_id: Uuid) -> usize {
    state
        .incident_narration_store
        .count_ongoing_for_target(org, target_id)
        .await
        .unwrap_or(0) as usize
}

/// Failures that never became an incident. Without them named, an empty
/// incidents tab reads as "nothing happened" beside an uptime card that agrees.
pub struct UnconfirmedFailures {
    pub failures: u64,
    pub transitions: u64,
    /// Regions that saw at least one failure, worst first.
    pub regions: Vec<String>,
    pub confirmations: u32,
    /// Regions that must agree, over the regions that reported — the same
    /// denominator the incident writer applies.
    pub quorum: usize,
    pub region_count: usize,
}

impl UnconfirmedFailures {
    /// `None` when there is nothing to explain.
    pub(super) fn new(
        flaps: &[crate::storage::traits::RegionFlaps],
        catalog: &[crate::storage::RegionOption],
        confirmations: u32,
        policy: crate::domain::RegionIncidentPolicy,
    ) -> Option<Self> {
        let failures: u64 = flaps.iter().map(|f| f.failures).sum();
        if failures == 0 {
            return None;
        }
        let mut failing: Vec<&crate::storage::traits::RegionFlaps> =
            flaps.iter().filter(|f| f.failures > 0).collect();
        failing.sort_by_key(|f| std::cmp::Reverse(f.failures));
        let region_count = flaps.len().max(1);
        Some(Self {
            failures,
            transitions: flaps.iter().map(|f| f.transitions).sum(),
            regions: failing
                .into_iter()
                .map(|f| {
                    catalog
                        .iter()
                        .find(|c| c.id == f.region)
                        .map(crate::web::views::region_display::region_label)
                        .unwrap_or_else(|| f.region.clone())
                })
                .collect(),
            confirmations,
            quorum: policy.required(region_count),
            region_count,
        })
    }
}

/// Whether an incident on this monitor would reach anybody. Mirrors the paging
/// path in `escalation::engine`: an effective policy (the monitor's own or the
/// org default) wins, and without one the bound channels are the only route.
/// A store error reads as "someone is reachable" so a failed lookup never
/// accuses a monitor that in fact alerts fine.
pub(super) async fn alerts_nobody(state: &AppState, org: OrgId, target: &Target) -> bool {
    // Settling this first is one indexed lookup against a read of every
    // channel in the org.
    match state
        .escalation_policy_store
        .resolve_for_target(org, target.id)
        .await
    {
        Ok(Some(_)) | Err(_) => return false,
        Ok(None) => {}
    }
    !binds_a_live_channel(state, org, target).await
}

/// A channel pages only while it is there and able to deliver; the sweep
/// skips the rest, so counting a dead one as coverage would hide the very
/// monitor this warning exists for.
async fn binds_a_live_channel(state: &AppState, org: OrgId, target: &Target) -> bool {
    // One read, not one per candidate: the binding and the tag rule are both
    // answered from the same rows.
    let Ok(channels) = state.notification_channel_store.list(org).await else {
        return true;
    };
    let folded: Vec<String> = target.tags.iter().map(|t| t.to_lowercase()).collect();
    channels.iter().any(|c| {
        c.can_deliver()
            && (target.alerts.iter().any(|b| b.channel_id == c.id)
                || crate::domain::matches_folded(&c.auto_bind_tags, &folded))
    })
}

/// Best-effort: a failed read costs the flap column, never the page.
pub(super) async fn flaps_by_region(
    state: &AppState,
    org: OrgId,
    target_id: Uuid,
) -> std::collections::HashMap<String, u64> {
    load_flaps(state, org, target_id)
        .await
        .into_iter()
        .map(|f| (f.region, f.transitions))
        .collect()
}

/// Flaps always cover this window, whatever range or region the page is showing:
/// the label is fixed, so the number behind it has to be. Also caps the raw read
/// a 30d page would otherwise widen — measured on the busiest production
/// monitor, 24h costs 17 ms against 233 ms for 30d.
pub(super) const FLAP_WINDOW_HOURS: i64 = 24;

pub(super) async fn load_flaps(
    state: &AppState,
    org: OrgId,
    target_id: Uuid,
) -> Vec<crate::storage::traits::RegionFlaps> {
    let to = Utc::now();
    let window = TimeRange {
        from: to - chrono::Duration::hours(FLAP_WINDOW_HOURS),
        to,
    };
    let Ok(clamped) = state.quotas.clamp_raw(org, window).await else {
        return Vec::new();
    };
    state
        .results_store
        .flap_counts(org, target_id, clamped)
        .await
        .unwrap_or_default()
}

/// Snapshot of the per-target live region: uptime stats + recent rows +
/// last-seen status. Cached in `AppState::live_data_cache` for 5s; both
/// the full-page detail view and the htmx live-partial poll read from
/// it so a burst of either kind collapses to one CH round-trip. Inner
/// fields are `Arc` so a cache hit clones a pointer instead of the
/// full row vector + uptime struct per request.
/// What the page can say about a heartbeat's own reports. `Unavailable` keeps
/// a failed read from falling back to the check count, which is the number the
/// card exists to stop showing.
#[derive(Clone, Copy)]
pub enum PingTally {
    Counted(u64),
    Unavailable,
}

impl PingTally {
    pub fn count(self) -> Option<u64> {
        match self {
            Self::Counted(n) => Some(n),
            Self::Unavailable => None,
        }
    }
}

pub struct LiveData {
    pub uptime: Arc<UptimeStatsView>,
    pub kpi: Arc<KpiTrend>,
    /// Recent rows for the public share page's results table. The owner detail
    /// dropped its table for the ribbon, but both surfaces share this loader.
    pub result_rows: Arc<[ResultRow]>,
    pub results_has_more: bool,
    pub last_status: &'static str,
    pub last_at_iso: Arc<str>,
    /// Per-bucket status strip over the window; drives the timeline under the header.
    pub segments: Arc<[StatusSeg]>,
    /// `None` for the kinds nobody pings.
    pub pings: Option<PingTally>,
}

/// Cached front door for [`load_live_data`]. Returns a moka-shared
/// `Arc<LiveData>` keyed on `(org, target_id, range_key)`. Preset
/// ranges are cached for 5s; ad-hoc `from`/`to` windows skip the cache
/// (one-off queries shouldn't pollute the shared bucket). Both the
/// full-page `index` and the htmx `live_partial` go through here so a
/// burst of either request type collapses to a single CH round-trip.
pub(crate) async fn load_live_data_cached(
    state: &AppState,
    org: OrgId,
    target: &Target,
    range_key: &'static str,
    custom_from: Option<DateTime<Utc>>,
    custom_to: Option<DateTime<Utc>>,
    region: Option<&str>,
) -> WebResult<Arc<LiveData>> {
    let target_id = target.id;
    let passive = target.check.is_passive();
    // A region-filtered view skips the shared cache (selection is rare, not
    // worth widening the key) — same call as the custom-window path.
    let cacheable = custom_from.is_none() && custom_to.is_none() && region.is_none();
    if cacheable {
        let key = (org, target_id, range_key);
        if let Some(data) = state.live_data_cache.get(&key) {
            return Ok(data);
        }
        let (from, to) = resolve_window(range_key, custom_from, custom_to);
        let data =
            Arc::new(load_live_data(state, org, target_id, from, to, region, passive).await?);
        state.live_data_cache.insert(key, data.clone());
        Ok(data)
    } else {
        let (from, to) = resolve_window(range_key, custom_from, custom_to);
        Ok(Arc::new(
            load_live_data(state, org, target_id, from, to, region, passive).await?,
        ))
    }
}

/// Cheapest possible read of "what status is this monitor in right
/// now?" — one row from the last [`LAST_RESULT_WINDOW_DAYS`] days.
/// Used by the incidents tab so the header badge doesn't require the
/// full uptime + recent-results scan `load_live_data` performs.
async fn latest_status_probe(
    state: &AppState,
    org: OrgId,
    target_id: Uuid,
) -> WebResult<(&'static str, String)> {
    let to = Utc::now();
    let from = to - Duration::days(LAST_RESULT_WINDOW_DAYS);
    // Current-status badge, not history browsing — exempt from the plan clamp.
    let row = state
        .results_store
        .list_results(
            org,
            target_id,
            ClampedRange::unclamped(TimeRange { from, to }),
            1,
            0,
            None,
        )
        .await?
        .into_iter()
        .next();
    Ok((
        row.as_ref().map(|r| r.status.as_str()).unwrap_or(""),
        row.map(|r| fmt_ts(r.timestamp)).unwrap_or_default(),
    ))
}

// Shared "what does the live detail region show?" loader. The full-page
// `index` handler enriches it with chrome (range options, config JSON,
// header strings); the `live_partial` handler returns it as-is for htmx
// polling. Keeping the query graph in one place keeps the partial
// byte-identical to the section the full page renders.
async fn load_live_data(
    state: &AppState,
    org: OrgId,
    target_id: Uuid,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    region: Option<&str>,
    passive: bool,
) -> WebResult<LiveData> {
    // Cap `to` at now: a monitor has no future samples, and an unbounded `to`
    // would let the span (hence the spark series length) grow without limit.
    let time_range = state
        .quotas
        .clamp_raw(
            org,
            TimeRange {
                from,
                to: to.min(Utc::now()),
            },
        )
        .await?;
    let span = time_range.to - time_range.from;
    let prior_range = state
        .quotas
        .clamp_raw(
            org,
            TimeRange {
                from: time_range.from - span,
                to: time_range.from,
            },
        )
        .await?;
    let spark_bucket_seconds = (span.num_seconds() / SPARK_SEGMENTS).max(60) as u32;
    // Confirmed downtime drives uptime only for the all-regions view; a region
    // filter keeps the raw per-region sample rate and needs no incident reads.
    let confirmed = region.is_none();

    let (mut uptime, mut results, avail, prior, cur_incidents, prior_incidents, pings) = tokio::try_join!(
        state
            .results_store
            .uptime(org, target_id, time_range, region),
        state.results_store.list_results(
            org,
            target_id,
            time_range,
            RESULTS_PAGE_LIMIT + 1,
            0,
            region
        ),
        state.results_store.availability_buckets(
            org,
            target_id,
            time_range,
            spark_bucket_seconds,
            region
        ),
        state
            .results_store
            .uptime(org, target_id, prior_range, region),
        confirmed_incidents(state, org, target_id, time_range, confirmed),
        confirmed_incidents(state, org, target_id, prior_range, confirmed),
        // Commentary: losing it must not lose the page.
        async {
            if !passive {
                return Ok(None);
            }
            Ok(Some(
                match state
                    .results_store
                    .heartbeat_ping_count(org, target_id, time_range)
                    .await
                {
                    Ok(Some(n)) => PingTally::Counted(n),
                    Ok(None) => PingTally::Unavailable,
                    Err(err) => {
                        tracing::warn!(error = %err, "heartbeat ping count unavailable");
                        PingTally::Unavailable
                    }
                },
            ))
        },
    )?;
    if confirmed && uptime.total > 0 {
        uptime.uptime_pct = Some(confirmed_uptime_pct(&cur_incidents, time_range));
    }
    let kpi = build_kpi_trend(KpiInputs {
        current: &uptime,
        prior: &prior,
        cur_incidents: &cur_incidents,
        prior_incidents: &prior_incidents,
        avail: &avail,
        range: time_range,
        prior_range,
        spark_bucket_seconds,
        confirmed,
    });
    // The strip shows raw per-bucket check outcomes so short error/degraded
    // patches stay visible, even when they never breached the incident
    // threshold the confirmed headline uptime is measured against. Empty
    // buckets read as grey gaps, not green.
    let segments: Arc<[StatusSeg]> = status_segments(
        &bucket_counts(&avail, time_range, spark_bucket_seconds),
        time_range,
        spark_bucket_seconds,
    )
    .into();
    let results_has_more = results.len() > RESULTS_PAGE_LIMIT;
    if results_has_more {
        results.truncate(RESULTS_PAGE_LIMIT);
    }

    // Last-known status when the window is empty — current-status, not history.
    let latest_outside_window = if results.is_empty()
        && let Some(window) = wider_status_window(from, to)
    {
        state
            .results_store
            .list_results(
                org,
                target_id,
                ClampedRange::unclamped(window),
                1,
                0,
                region,
            )
            .await?
            .into_iter()
            .next()
    } else {
        None
    };
    let latest_for_badge = results.first().or(latest_outside_window.as_ref());
    let last_status = latest_for_badge.map(|r| r.status.as_str()).unwrap_or("");
    let last_at_iso: Arc<str> = latest_for_badge
        .map(|r| fmt_ts(r.timestamp))
        .unwrap_or_default()
        .into();
    let result_rows: Arc<[ResultRow]> = results
        .into_iter()
        .map(ResultRow::from)
        .collect::<Vec<_>>()
        .into();

    Ok(LiveData {
        uptime: Arc::new(uptime.into()),
        kpi: Arc::new(kpi),
        result_rows,
        results_has_more,
        last_status,
        last_at_iso,
        segments,
        pings,
    })
}

/// Confirmed incidents for the downtime read, or an empty vec (no DB hit) when
/// the view is region-filtered and uses the raw rate instead. Kept separate so
/// `load_live_data` can fan out current + prior reads in one `try_join`.
async fn confirmed_incidents(
    state: &AppState,
    org: OrgId,
    target_id: Uuid,
    range: ClampedRange,
    confirmed: bool,
) -> crate::error::Result<Vec<Incident>> {
    if !confirmed {
        return Ok(Vec::new());
    }
    state
        .incident_narration_store
        .list_for_target(
            org,
            target_id,
            range.inner(),
            CONFIRMED_UPTIME_INCIDENT_CAP,
            0,
            false,
        )
        .await
}

pub(super) fn confirmed_uptime_pct(incidents: &[Incident], range: ClampedRange) -> f64 {
    let down = confirmed_downtime_secs(incidents, range.from, range.to, Utc::now());
    uptime_pct_from_downtime(down, (range.to - range.from).num_seconds())
}

/// The incidents-tab data for one monitor: the header badge's live status, the
/// page of incident rows, and the derived ongoing count. Loaded by
/// [`load_incidents_data`] so the operator incidents page and the public share
/// view render the same dataset from one query graph.
pub(crate) struct IncidentsData {
    pub last_status: &'static str,
    pub last_at_iso: String,
    pub rows: Vec<IncidentRow>,
    pub has_more: bool,
    pub ongoing_count: usize,
}

/// Load the incidents tab's dataset for `(org, target_id)` over `time_range`
/// from the confirmed materialised incidents.
pub(crate) async fn load_incidents_data(
    state: &AppState,
    org: OrgId,
    target_id: Uuid,
    time_range: TimeRange,
) -> WebResult<IncidentsData> {
    let range = state.quotas.clamp_raw(org, time_range).await?;
    // The incidents tab needs only the badge's `last_status` from the live
    // region — not the uptime stats or 60 recent rows. Probe one row instead of
    // running the full live loader; a 90d preset would otherwise scan the entire
    // window for the same single field.
    let ((last_status, last_at_iso), mut incidents) =
        tokio::try_join!(latest_status_probe(state, org, target_id), async {
            state
                .incident_narration_store
                .list_for_target(
                    org,
                    target_id,
                    range.inner(),
                    INCIDENTS_PAGE_LIMIT + 1,
                    0,
                    false,
                )
                .await
                .map_err(WebError::from)
        },)?;

    let has_more = incidents.len() > INCIDENTS_PAGE_LIMIT;
    if has_more {
        incidents.truncate(INCIDENTS_PAGE_LIMIT);
    }
    // Counted org-wide for the monitor, not from the page above it: an open
    // incident that started before the selected window is still open now.
    let ongoing_count = ongoing_for_target(state, org, target_id).await;

    Ok(IncidentsData {
        last_status,
        last_at_iso,
        rows: incidents.into_iter().map(IncidentRow::from).collect(),
        has_more,
        ongoing_count,
    })
}
