//! Dashboard view models and the page/partial templates they render into.

use std::sync::Arc;

use askama::Template;
use askama_web::WebTemplate;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use uuid::Uuid;

use crate::domain::metrics::DashboardMetrics;
use crate::domain::{CheckStatus, IncidentSeverity, uptime_pct_from_downtime};
use crate::storage::IncidentBrief;
use crate::templates::filters;
use crate::templates::format::HumanDur;
use crate::web::views::RangeOption;
use crate::web::views::region_display::LabeledRegion;

use super::*;

#[derive(Debug, Default, Deserialize)]
pub struct DashboardParams {
    #[serde(default)]
    pub range: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    /// Bucket start (unix-seconds) of a clicked ribbon cell; filters the table
    /// to the monitors that dipped in that window.
    #[serde(default)]
    pub down_at: Option<i64>,
    /// Login flows set `joined=<slug>` after auto-accepting an invitation. The
    /// slug is validated against the active org before the banner renders, so
    /// unlike the one-shot flash signals it stays a (harmless) query param.
    #[serde(default)]
    pub joined: Option<String>,
}

/// One row in the dashboard table — every cell the template renders.
#[derive(Clone)]
pub struct DashboardRow {
    pub id: String,
    pub name: String,
    pub kind: &'static str,
    pub address: String,
    pub enabled: bool,
    /// "up" / "down" / "degraded" / "error" / "" — empty when the
    /// monitor has never reported in the selected range. Drives both
    /// the `dashboard-dot--*` colour and the `data-status` row attribute.
    pub last_status: &'static str,
    /// Median latency in ms, formatted (e.g. "142 ms"). "—" when no
    /// samples.
    pub p50_label: String,
    pub p95_label: String,
    /// `0.0..100.0` error rate (`100 - uptime%`). Displayed with one
    /// decimal so a 0.03 % flake doesn't round to 0.
    pub err_pct_label: String,
    pub uptime_pct_label: String,
    pub samples: u64,
    /// `60` minute-aligned average-duration points, oldest → newest.
    /// Empty buckets carry `None`; the renderer skips them and connects
    /// the line across the gap rather than drawing a drop-to-zero spike.
    pub spark: Vec<Option<f32>>,
    /// Pre-rendered SVG path `d` for the sparkline polyline. Built
    /// once server-side so the template stays static markup and the
    /// browser does no per-row math on render.
    pub spark_path: String,
    /// SVG `<path d=…>` for the soft area fill under the line — the line
    /// closed down to the baseline. Empty below two samples.
    pub spark_fill: String,
    pub spark_baseline_y: u32,
}

/// What [`load_snapshot`] returns. Held in `AppState::dashboard_cache`
/// behind an `Arc` so cache hits are pointer-bumps even when an org has
/// hundreds of monitors. Inner `Arc`s let the per-page template clone
/// fields out for the surrounding chrome without re-cloning the table.
#[derive(Clone)]
pub struct DashboardSnapshot {
    pub rows: Arc<[DashboardRow]>,
    pub kpi_cards: Arc<[KpiCardSpec]>,
    pub matches: usize,
    pub truncated: bool,
    pub active_incidents: Arc<[DashboardActiveIncident]>,
    pub status_counts: StatusCounts,
    pub type_counts: Arc<[TypeCount]>,
    pub ribbon: FleetRibbon,
}

#[derive(Clone)]
pub struct DashboardKpis {
    pub uptime_pct_label: String,
    pub avg_response_ms_label: String,
    pub checks_label: String,
    pub checks_successful_label: String,
    pub incidents: u64,
}

/// Per-card config passed to the KPI partial — kills the 3-way copy that
/// a hand-unrolled template would force.
#[derive(Clone)]
pub struct KpiCardSpec {
    pub label: String,
    pub value: String,
    pub hint_html: String,
    /// `None` when there is no prior data — template skips the line.
    pub delta: Option<KpiDelta>,
    pub spark_tint: &'static str,
    /// The card's own metric over time. Each card plots what it counts.
    pub spark_path: String,
    pub spark_fill: String,
}

/// Renderable Δ-vs-prior chip: `<span class="{class}">{arrow} {body} vs
/// prior</span>` — wrapper lives in the template so no `<span>` strings
/// are allocated per render.
#[derive(Clone)]
pub struct KpiDelta {
    pub class: &'static str,
    pub arrow: &'static str,
    pub body: String,
}

/// Health-rail counts. A disabled monitor is "paused" regardless of its
/// last sample; everything else flows from `last_status`.
#[derive(Clone, Copy, Default)]
pub struct StatusCounts {
    pub up: u32,
    pub degraded: u32,
    pub down: u32,
    pub paused: u32,
}

/// One cell of the 48-seg fleet ribbon. A non-empty `down_targets` makes the
/// cell a drill link (`bucket_ts` is its `down_at` value); `down_preview` caps
/// the tooltip's names while the drill keeps the full set.
#[derive(Clone)]
pub struct FleetRibbonSeg {
    pub class: &'static str,
    pub time: String,
    pub stat: String,
    pub down_preview: Arc<[String]>,
    pub bucket_ts: i64,
    pub down_targets: Arc<[Uuid]>,
}

#[derive(Clone)]
pub struct FleetRibbon {
    pub segs: Arc<[FleetRibbonSeg]>,
    /// Aggregate uptime label across the full 24h window ("99.71%" or "—").
    pub uptime_label: String,
}

#[derive(Clone)]
pub struct TypeCount {
    pub label: &'static str,
    pub count: u32,
    pub active: bool,
}

#[derive(Clone)]
pub struct DashboardActiveIncident {
    pub id: String,
    pub target_id: String,
    pub title: String,
    pub age_label: String,
    pub severity_label: &'static str,
    pub severity_class: &'static str,
    pub latest_update: Option<DashboardIncidentUpdate>,
}

#[derive(Clone)]
pub struct DashboardIncidentUpdate {
    pub posted_at: chrono::DateTime<chrono::Utc>,
    pub phase_label: &'static str,
    pub message: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "dashboard.html")]
pub struct DashboardPage {
    pub active_tab: &'static str,
    pub range: &'static str,
    pub range_options: Vec<RangeOption>,
    pub kpi_cards: Arc<[KpiCardSpec]>,
    pub rows: Arc<[DashboardRow]>,
    pub matches: usize,
    pub truncated: bool,
    pub onboarding: bool,
    pub active_incidents: Arc<[DashboardActiveIncident]>,
    pub status_counts: StatusCounts,
    pub type_counts: Arc<[TypeCount]>,
    pub ribbon: FleetRibbon,
    pub regions: Vec<LabeledRegion>,
    pub selected_region: Option<String>,
    pub status_options: Vec<RangeOption>,
    pub selected_status: Option<&'static str>,
    pub selected_kind: Option<&'static str>,
    pub drill: Option<DrillChip>,
    pub restored_notice: bool,
    pub joined_notice: Option<String>,
    pub invite_missed_notice: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "dashboard/table.html")]
pub struct DashboardTablePartial {
    pub range: &'static str,
    pub range_options: Vec<RangeOption>,
    pub kpi_cards: Arc<[KpiCardSpec]>,
    pub rows: Arc<[DashboardRow]>,
    pub matches: usize,
    pub truncated: bool,
    pub active_incidents: Arc<[DashboardActiveIncident]>,
    pub status_counts: StatusCounts,
    pub type_counts: Arc<[TypeCount]>,
    pub ribbon: FleetRibbon,
    pub regions: Vec<LabeledRegion>,
    pub selected_region: Option<String>,
    pub status_options: Vec<RangeOption>,
    pub selected_status: Option<&'static str>,
    pub selected_kind: Option<&'static str>,
    pub drill: Option<DrillChip>,
}

impl DashboardRow {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn build(
        id: Uuid,
        name: String,
        kind: &'static str,
        address: String,
        enabled: bool,
        metrics: Option<&DashboardMetrics>,
        folded: Option<CheckStatus>,
        spark: Vec<Option<f32>>,
        confirmed_downtime_secs: Option<i64>,
        window_secs: i64,
    ) -> Self {
        let (p50_label, p95_label, err_pct_label, uptime_pct_label, last_status, samples) =
            match metrics {
                Some(m) if m.samples > 0 => {
                    let err_pct = ((m.samples - m.up) as f64 / m.samples as f64) * 100.0;
                    let uptime_pct = match confirmed_downtime_secs {
                        Some(d) => uptime_pct_from_downtime(d, window_secs),
                        None => 100.0 - err_pct,
                    };
                    (
                        format!("{} ms", m.p50_ms),
                        format!("{} ms", m.p95_ms),
                        format!("{err_pct:.1}"),
                        format!("{uptime_pct:.2}"),
                        match folded {
                            Some(f) => status_label(f.as_str()),
                            None => status_label(&m.last_status),
                        },
                        m.samples,
                    )
                }
                _ => ("—".into(), "—".into(), "—".into(), "—".into(), "", 0),
            };
        let (spark_path, spark_fill, spark_baseline_y) = render_spark_path(&spark);
        Self {
            id: id.to_string(),
            name,
            kind,
            address,
            enabled,
            last_status,
            p50_label,
            p95_label,
            err_pct_label,
            uptime_pct_label,
            samples,
            spark,
            spark_path,
            spark_fill,
            spark_baseline_y,
        }
    }
}

impl DashboardActiveIncident {
    pub(super) fn build(raw: IncidentBrief, now: DateTime<Utc>) -> Self {
        let IncidentBrief {
            id,
            target_id,
            target_name,
            severity,
            started_at,
            public_title,
            latest_update,
            ..
        } = raw;
        let title = public_title
            .filter(|t| !t.trim().is_empty())
            .or_else(|| (!target_name.is_empty()).then_some(target_name))
            .unwrap_or_else(|| "Active incident".into());
        let age_secs = (now - started_at).num_seconds().max(0);
        Self {
            id: id.to_string(),
            target_id: target_id.to_string(),
            title,
            age_label: HumanDur(age_secs).to_string(),
            severity_label: severity_display(severity),
            severity_class: severity_class(severity),
            latest_update: latest_update.map(|u| DashboardIncidentUpdate {
                posted_at: u.posted_at,
                phase_label: phase_display(u.phase),
                message: u.message,
            }),
        }
    }
}

fn severity_display(s: IncidentSeverity) -> &'static str {
    match s {
        IncidentSeverity::Minor => "MINOR",
        IncidentSeverity::Major => "MAJOR",
        IncidentSeverity::Critical => "CRITICAL",
    }
}

fn severity_class(s: IncidentSeverity) -> &'static str {
    match s {
        IncidentSeverity::Minor => "minor",
        IncidentSeverity::Major => "major",
        IncidentSeverity::Critical => "critical",
    }
}

fn phase_display(p: crate::domain::IncidentStatusPhase) -> &'static str {
    use crate::domain::IncidentStatusPhase::*;
    match p {
        Investigating => "Investigating",
        Identified => "Identified",
        Monitoring => "Monitoring",
        Resolved => "Resolved",
        Postmortem => "Postmortem",
    }
}

fn status_label(raw: &str) -> &'static str {
    match raw {
        "up" => "up",
        "down" => "down",
        "degraded" => "degraded",
        "error" => "error",
        _ => "",
    }
}
