use std::sync::Arc;

use askama::Template;
use askama_web::WebTemplate;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::load::PingTally;
use crate::domain::agent_wire::StepOutcome;
use crate::domain::{CheckResult, Incident};
use crate::storage::UptimeStats;
use crate::web::filters;
use crate::web::views::dashboard::KpiDelta;
use crate::web::views::{fmt_human, fmt_ts};

use super::charts::StatusSeg;
use super::fmt_error_display;
use crate::web::views::dashboard;

#[derive(Clone)]
pub struct ResultRow {
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub status: &'static str,
    pub duration_ms: u32,
    pub response_code: String,
    pub error: String,
    /// Kept apart from the error so the drawer reads cause, then fix.
    pub diagnostic_guidance: String,
    pub dns_ms: Option<u16>,
    pub connect_ms: Option<u16>,
    pub tls_ms: Option<u16>,
    pub ttfb_ms: Option<u16>,
    /// Probe region; `Some` only where the row is region-tagged (the drill
    /// drawer). `None` on the region-agnostic recent-results table.
    pub region: Option<String>,
    /// For a flow failure, the `step N/M · op` badge parsed off the error, so
    /// the drawer can surface which step broke. `None` for every other error.
    pub flow_step: Option<String>,
}

/// Split a flow step-failure error (`step N/M op: reason`) into a compact badge
/// and the bare reason. `None` for engine errors and non-flow errors.
fn parse_flow_step(raw: &str) -> Option<(String, String)> {
    let rest = raw.strip_prefix("step ")?;
    let (frac, tail) = rest.split_once(' ')?;
    let (n, m) = frac.split_once('/')?;
    n.parse::<u32>().ok()?;
    m.parse::<u32>().ok()?;
    let (op, reason) = tail.split_once(": ")?;
    if op.is_empty() || op.contains(' ') {
        return None;
    }
    Some((format!("step {n}/{m} · {op}"), reason.to_string()))
}

impl ResultRow {
    /// Row tagged with the region it ran in; an empty region label reads as `None`.
    pub(super) fn with_region(region: String, r: CheckResult) -> Self {
        Self {
            region: (!region.is_empty()).then_some(region),
            ..Self::from(r)
        }
    }
}

pub struct IncidentRow {
    pub id: Uuid,
    /// "down" | "error" | "degraded" — drives the `status-badge--*` CSS class.
    pub severity: &'static str,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    /// `Some(secs)` for closed incidents (template renders via
    /// `humanize_dur`); `None` while ongoing (template renders "Ongoing").
    pub duration_secs: Option<i64>,
    pub check_count: u64,
    pub error_sample: String,
    pub ongoing: bool,
    pub counts_as_downtime: bool,
}

/// What a heartbeat's own clock says. `Some` for every heartbeat, so the live
/// poll carries the answer that clears a notice as well as the one that raises it.
pub struct HeartbeatLiveness {
    /// No ping yet, so nothing to be late for.
    pub pending: bool,
    /// Where the wait is counted from; `None` while the row is provisioning.
    pub since: Option<DateTime<Utc>>,
    pub due_at: Option<DateTime<Utc>>,
    /// `None` when a zero grace lands it on `due_at`: one instant, one line.
    pub down_at: Option<DateTime<Utc>>,
    /// Past due, still inside grace. The stored status stays up.
    pub late: bool,
    /// Past the whole window, so the notice stops promising and speaks in the
    /// past tense.
    pub overdue: bool,
}

impl HeartbeatLiveness {
    pub fn derive(
        hb: &crate::api::handlers::targets::HeartbeatInfo,
        now: DateTime<Utc>,
        enabled: bool,
    ) -> Self {
        Self {
            pending: hb.pending && enabled,
            since: hb.created_at,
            due_at: hb.due_at,
            down_at: hb.down_at.filter(|d| Some(*d) != hb.due_at),
            late: hb.due_at.is_some_and(|d| now > d) && hb.down_at.is_some_and(|d| now <= d),
            overdue: hb.down_at.is_some_and(|d| now > d),
        }
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "targets/partials/detail_live.html")]
pub struct DetailLive {
    pub id: String,
    pub name: String,
    pub range: &'static str,
    pub enabled: bool,
    pub last_status: &'static str,
    pub uptime: Arc<UptimeStatsView>,
    pub kpi: Arc<KpiTrend>,
    pub pings: Option<PingTally>,
    pub last_at_iso: Arc<str>,
    /// Carried so the self-rearming live poll keeps the active region filter.
    pub selected_region: Option<String>,
    /// Per-bucket status ribbon, re-rendered here so the 60s poll keeps the
    /// newest cell current.
    pub segments: Arc<[StatusSeg]>,
    /// Marks the ribbon include as an out-of-band swap on the live response.
    pub ribbon_oob: bool,
    pub liveness: Option<HeartbeatLiveness>,
    /// The notice is only ever out of band here: it lives in the ping card,
    /// which this response does not own.
    pub liveness_oob: bool,
}

/// Rows for the ribbon drill drawer: a page of raw checks over a bucket window,
/// each tagged with its region. Renders the shared recent-results row partial
/// with the region column shown.
#[derive(Template, WebTemplate)]
#[template(path = "targets/partials/detail_live_rows.html")]
pub struct DetailCheckRows {
    pub results: Arc<[ResultRow]>,
    pub show_region: bool,
    /// Remediation is advice for whoever owns the monitor, so the anonymous
    /// share surface renders the cause without it.
    pub show_guidance: bool,
}

/// Enough to see a pattern across a day at the interval floor, without paging.
pub(super) const FLOW_RUNS_SHOWN: usize = 25;

pub struct FlowStepRow {
    pub number: usize,
    pub op: String,
    /// Playback class shared with the builder, so a step reads the same on both.
    pub state: &'static str,
    pub marker: &'static str,
    pub ms_label: String,
    /// Blank on every step but the one the run stopped at.
    pub reason: String,
}

pub struct FlowRunRow {
    pub at_iso: String,
    pub at_utc: String,
    pub region: String,
    pub status: &'static str,
    pub duration_label: String,
    /// What the run did, in the collapsed header.
    pub summary: String,
    /// Shown when no step carries it — a run that never reached the step list.
    pub error: String,
    pub steps: Vec<FlowStepRow>,
    /// `None` on a pass, and on a failure old enough to have shed its page.
    pub evidence: Option<FlowEvidenceView>,
    /// A step failure whose page has passed its window. A run that broke before
    /// the step list never captured one, which is a different thing.
    pub evidence_expired: bool,
}

pub struct FlowEvidenceView {
    pub final_url: String,
    pub title: String,
    pub text_snippet: String,
    pub console: Vec<String>,
}

impl FlowRunRow {
    pub(super) fn from_view(v: crate::storage::traits::FlowRunView) -> Self {
        let stopped = v.stopped_step;
        let total = v.steps.len();
        let reason = strip_step_prefix(v.error.as_deref().unwrap_or_default());
        let steps = v
            .steps
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let (state, marker) = match s.outcome {
                    StepOutcome::Passed => ("flow-step--pass", "✓"),
                    StepOutcome::Failed => ("flow-step--fail", "✗"),
                    StepOutcome::Skipped => ("flow-step--skip", "·"),
                };
                FlowStepRow {
                    number: i + 1,
                    op: s.op.clone(),
                    state,
                    marker,
                    ms_label: format!("{} ms", s.duration_ms),
                    reason: if Some(i) == stopped {
                        reason.clone()
                    } else {
                        String::new()
                    },
                }
            })
            .collect();
        let evidence = v.evidence.map(|e| FlowEvidenceView {
            final_url: e.final_url.unwrap_or_default(),
            title: e.title.unwrap_or_default(),
            text_snippet: e.text_snippet.unwrap_or_default(),
            console: e
                .console
                .into_iter()
                .map(|c| format!("{}: {}", c.level, c.text))
                .collect(),
        });
        // Only a run that reached a step could have had a page to lose, and only
        // the store knows whether the window is what took it.
        let evidence_expired = v.evidence_expired && evidence.is_none() && stopped.is_some();
        let summary = match stopped {
            Some(i) => format!(
                "stopped at step {}/{total} {}",
                i + 1,
                v.steps.get(i).map(|s| s.op.as_str()).unwrap_or_default()
            ),
            None if total > 0 => format!("all {total} steps passed"),
            None => "never reached its steps".to_string(),
        };
        Self {
            at_iso: v.timestamp.to_rfc3339(),
            at_utc: v.timestamp.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
            region: v.region,
            status: v.status.as_str(),
            duration_label: format!("{} ms", v.duration_ms),
            summary,
            // A trace pins the reason to the step it belongs to; without one
            // the run-level line is the only thing that can explain it.
            error: if total > 0 {
                String::new()
            } else {
                v.error.unwrap_or_default()
            },
            steps,
            evidence,
            evidence_expired,
        }
    }
}

/// Drops the "step 4/5 assert_url: " prefix: the row already names that step.
fn strip_step_prefix(error: &str) -> String {
    error
        .split_once(": ")
        .filter(|(head, _)| head.starts_with("step "))
        .map(|(_, rest)| rest.to_string())
        .unwrap_or_else(|| error.to_string())
}

/// One row of the per-region breakdown table on the monitor detail page.
pub struct RegionBreakdownRow {
    /// Lets a click on the region's bar in the breakdown chart find this row.
    pub region: String,
    /// Applies this region's filter; on the row already filtered to, clears it.
    pub filter_href: String,
    pub region_label: String,
    pub uptime_label: String,
    pub p50_label: String,
    pub p95_label: String,
    pub p99_label: String,
    /// "" when the region has no samples in the range.
    pub last_status: String,
    /// Marks the row matching the active region filter.
    pub selected: bool,
    /// Status changes over the last 24h. Bursts too short to open an incident
    /// show up here and nowhere else.
    pub flaps: u64,
}

impl RegionBreakdownRow {
    pub(super) fn from_rollup(
        r: crate::domain::metrics::RegionRollup,
        selected_region: Option<&str>,
        catalog: &[crate::storage::RegionOption],
        live_regions: Option<&std::collections::HashSet<String>>,
        base_path: &str,
        range: &str,
        flaps: u64,
    ) -> Self {
        let uptime_label = dashboard::pct_label(r.samples, r.up);
        let selected = selected_region == Some(r.region.as_str());
        let filter_href = if selected {
            format!("{base_path}?range={range}")
        } else {
            let region: String =
                url::form_urlencoded::byte_serialize(r.region.as_bytes()).collect();
            format!("{base_path}?range={range}&region={region}")
        };
        let region_label = catalog
            .iter()
            .find(|c| c.id == r.region)
            .map(crate::web::views::region_display::region_label)
            .unwrap_or_else(|| r.region.clone());
        // No live probe overrides the stale last status with grey "no data".
        // `None` = liveness unknown (query failed); leave the status untouched
        // rather than greying every region on a transient blip.
        let last_status = match live_regions {
            Some(live) if !live.contains(&r.region) => "no_data".to_string(),
            _ => r.last_status,
        };
        Self {
            selected,
            uptime_label,
            p50_label: format!("{} ms", r.p50_ms),
            p95_label: format!("{} ms", r.p95_ms),
            p99_label: format!("{} ms", r.p99_ms),
            last_status,
            region_label,
            filter_href,
            region: r.region,
            flaps,
        }
    }
}

#[derive(Clone)]
pub struct UptimeStatsView {
    pub total: u64,
    pub up: u64,
    pub down: u64,
    pub degraded: u64,
    pub error: u64,
    /// `None` for an empty window, rendered as "no data" rather than a rate.
    pub uptime_pct: Option<String>,
}

impl From<UptimeStats> for UptimeStatsView {
    fn from(s: UptimeStats) -> Self {
        Self {
            total: s.total,
            up: s.up,
            down: s.down,
            degraded: s.degraded,
            error: s.error,
            uptime_pct: s.uptime_pct.map(fmt_uptime_pct),
        }
    }
}

/// Two-decimal uptime with trailing zeros trimmed: `100`, `99.98`, `99.9`.
fn fmt_uptime_pct(v: f64) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Sparkline + Δ-vs-prior chips for the KPI strip, shared by the owner detail
/// and public share pages. `Default` renders no spark and no deltas — used
/// when there is no prior window to compare against.
#[derive(Default)]
pub struct KpiTrend {
    pub spark_path: String,
    pub spark_fill: String,
    pub uptime_delta: Option<KpiDelta>,
    pub up_delta: Option<KpiDelta>,
    pub down_delta: Option<KpiDelta>,
    pub error_delta: Option<KpiDelta>,
}

/// ISO and human-readable strings for a `(from, to)` window. Both
/// detail-page templates render the same four fields in the chrome
/// (range pill caption, time inputs); precomputing once keeps the
/// template free of filter chains for one-shot values.
pub(crate) struct WindowLabels {
    pub from_iso: String,
    pub to_iso: String,
    pub from_human: String,
    pub to_human: String,
}

impl WindowLabels {
    pub(crate) fn new(from: DateTime<Utc>, to: DateTime<Utc>) -> Self {
        Self {
            from_iso: fmt_ts(from),
            to_iso: fmt_ts(to),
            from_human: fmt_human(from),
            to_human: fmt_human(to),
        }
    }
}

impl From<CheckResult> for ResultRow {
    fn from(r: CheckResult) -> Self {
        let diagnostic = r.diagnostic.as_ref().map(|item| item.summary());
        let diagnostic_guidance = r
            .diagnostic
            .as_ref()
            .map(|item| item.guidance().to_string())
            .unwrap_or_default();
        let (flow_step, mut error) = match r.error.as_deref() {
            Some(raw) => match parse_flow_step(raw) {
                Some((label, reason)) => (Some(label), fmt_error_display(&reason)),
                None => (None, fmt_error_display(raw)),
            },
            None => (None, String::new()),
        };
        if let Some(summary) = diagnostic {
            if error.is_empty() {
                error = summary;
            } else {
                error.push_str(" · ");
                error.push_str(&summary);
            }
        }
        Self {
            timestamp: r.timestamp,
            status: r.status.as_str(),
            duration_ms: r.duration_ms,
            response_code: r.response_code.map(|c| c.to_string()).unwrap_or_default(),
            error,
            diagnostic_guidance,
            dns_ms: r.dns_ms,
            connect_ms: r.connect_ms,
            tls_ms: r.tls_ms,
            ttfb_ms: r.ttfb_ms,
            region: None,
            flow_step,
        }
    }
}

impl From<Incident> for IncidentRow {
    fn from(inc: Incident) -> Self {
        let ongoing = inc.ended_at.is_none();
        let duration_secs = inc.closed_duration().map(|d| d.num_seconds());
        Self {
            id: inc.id,
            severity: inc.status.as_str(),
            started_at: inc.started_at,
            ended_at: inc.ended_at,
            duration_secs,
            check_count: inc.check_count,
            error_sample: inc
                .error_sample
                .as_deref()
                .map(fmt_error_display)
                .unwrap_or_default(),
            ongoing,
            counts_as_downtime: inc.counts_as_downtime,
        }
    }
}
