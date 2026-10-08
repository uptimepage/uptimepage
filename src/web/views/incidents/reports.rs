//! Incident metrics over a window: counts, MTTA/MTTR and the noisiest
//! monitors.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Query, State};
use serde::Deserialize;

use crate::app::AppState;
use crate::request::{AuthedBrowser, CurrentOrg};
use crate::templates::filters;
use crate::web::error::WebResult;

use super::fmt_secs;

pub(super) const WINDOW_DAYS: &[u32] = &[7, 30, 90];
const DEFAULT_WINDOW: u32 = 30;

#[derive(Debug, Default, Deserialize)]
pub struct ReportParams {
    pub window_days: Option<u32>,
}

pub struct WindowOption {
    pub days: u32,
    pub active: bool,
}

pub struct ReportBucket {
    pub label: String,
    pub count: u64,
}

pub struct ReportMonitorRow {
    /// `None` once the monitor has been deleted.
    pub id: Option<String>,
    pub name: String,
    pub count: u64,
}

#[derive(Template, WebTemplate)]
#[template(path = "incidents/reports.html")]
pub struct IncidentsReportPage {
    pub active_tab: &'static str,
    pub window_days: u32,
    pub windows: Vec<WindowOption>,
    pub total: u64,
    pub mtta: Option<String>,
    pub mttr: Option<String>,
    pub by_severity: Vec<ReportBucket>,
    pub by_state: Vec<ReportBucket>,
    pub auto_resolved: u64,
    pub human_resolved: u64,
    pub closed_with_monitor: u64,
    pub top_monitors: Vec<ReportMonitorRow>,
}

pub async fn reports(
    _auth: AuthedBrowser,
    CurrentOrg(org): CurrentOrg,
    State(state): State<AppState>,
    Query(params): Query<ReportParams>,
) -> WebResult<IncidentsReportPage> {
    let window = params
        .window_days
        .filter(|d| WINDOW_DAYS.contains(d))
        .unwrap_or(DEFAULT_WINDOW);
    // 30s cache: a report view tolerates slight staleness and the aggregate
    // scans need not re-run on every load / window flip.
    let m = match state.incident_metrics_cache.get(&(org, window)) {
        Some(m) => m,
        None => {
            let m = state.incident_ops_store.metrics(org, window).await?;
            state
                .incident_metrics_cache
                .insert((org, window), m.clone());
            m
        }
    };
    let bucket = |b: crate::domain::MetricBucket| ReportBucket {
        label: b.key,
        count: b.count,
    };
    Ok(IncidentsReportPage {
        active_tab: "incidents",
        window_days: m.window_days,
        windows: WINDOW_DAYS
            .iter()
            .map(|d| WindowOption {
                days: *d,
                active: *d == window,
            })
            .collect(),
        total: m.total,
        mtta: fmt_secs(m.mtta_secs),
        mttr: fmt_secs(m.mttr_secs),
        by_severity: m.by_severity.into_iter().map(bucket).collect(),
        by_state: m.by_state.into_iter().map(bucket).collect(),
        auto_resolved: m.auto_resolved,
        human_resolved: m.human_resolved,
        closed_with_monitor: m.closed_with_monitor,
        top_monitors: m
            .top_monitors
            .into_iter()
            .map(|t| ReportMonitorRow {
                id: t.target_id.map(|id| id.to_string()),
                name: t.name,
                count: t.count,
            })
            .collect(),
    })
}
