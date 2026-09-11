use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::domain::{CheckResult, CheckSpec};

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TagCount {
    pub name: String,
    pub count: u64,
}

#[derive(Debug, Clone, Default, Serialize, ToSchema)]
pub struct TargetsSummary {
    pub total: u64,
    pub enabled: u64,
    pub disabled: u64,
}

#[derive(Debug, Clone, Default, Serialize, ToSchema)]
pub struct StatusBreakdown {
    pub up: u64,
    pub down: u64,
    pub degraded: u64,
    pub error: u64,
    /// No readable result in the window; the buckets sum to the monitor count.
    pub unknown: u64,
}

#[derive(Debug, Clone, Default, Serialize, ToSchema)]
pub struct Last24hSummary {
    pub checks_total: u64,
    pub checks_up: u64,
    #[schema(example = 99.94)]
    pub uptime_pct: f64,
    pub incidents: u64,
}

#[derive(Debug, Clone, Default, Serialize, ToSchema)]
pub struct SystemSummary {
    pub in_flight_checks: u32,
    pub result_queue_depth: u32,
    /// Cumulative drops since process start; reset on restart.
    pub dropped_results_last_5m: u64,
    pub circuit_breakers_open: u32,
}

#[derive(Debug, Clone, Default, Serialize, ToSchema)]
pub struct DashboardSummary {
    pub targets: TargetsSummary,
    pub current_status: StatusBreakdown,
    pub last_24h: Last24hSummary,
    pub system: SystemSummary,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BulkActionRequest {
    /// Up to 10 000 ids per request.
    pub ids: Vec<Uuid>,
    pub action: BulkAction,
}

/// The bare actions are empty struct variants, not unit ones: serde only
/// refuses a stray key next to `type` for a variant that has fields.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum BulkAction {
    Enable {},
    Disable {},
    Delete {},
    TagAdd {
        tags: Vec<String>,
    },
    TagRemove {
        tags: Vec<String>,
    },
    /// Set every target's `group_name` to `group`. Pass `null` to clear.
    SetGroup {
        group: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BulkActionResponse {
    pub succeeded: Vec<Uuid>,
    pub failed: Vec<BulkActionFailure>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BulkActionFailure {
    pub id: Uuid,
    pub code: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TestRequest {
    pub check: CheckSpec,
    /// Region to run the test in. Omitted → the control plane's default
    /// region. The UI fans out one request per selected region.
    #[serde(default)]
    pub region: Option<String>,
}

pub use crate::domain::agent_wire::HeaderPreview;

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TestResponse {
    pub result: CheckResult,
    /// Whether the check would be considered `up` given the spec's
    /// `expected_status` / `body_contains`.
    pub matched_expectations: bool,
    /// Validation warnings that did not block execution.
    pub warnings: Vec<String>,
    /// Response headers preview, HTTP only. Sensitive headers redacted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub response_headers_preview: Vec<HeaderPreview>,
    /// First 1 KiB of decoded body, HTTP only. UTF-8 lossy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_body_snippet: Option<String>,
    /// Page state when a step failed, flow only. Absent on a pass, and on an
    /// engine error, where the page state says nothing about the target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_evidence: Option<crate::domain::agent_wire::FlowEvidence>,
    /// One entry per declared step, flow only. Positional, so an entry's index
    /// is its step index, and steps the run never reached are `skipped`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flow_steps: Vec<crate::domain::agent_wire::StepTrace>,
    /// Region the test ran in, echoed so a fan-out caller can correlate rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
}

/// Per-monitor rollup for the operator dashboard table. One row per target
/// over the chosen range — drives every numeric cell in the Dashboard list
/// (p50/p95/error rate/uptime%/last status). Produced by a single batched
/// ClickHouse aggregation so a 1k-monitor org renders in one round-trip.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DashboardMetrics {
    pub target_id: Uuid,
    /// Number of check samples observed in the range.
    pub samples: u64,
    /// Samples with `status = up`.
    pub up: u64,
    /// Mean check duration in milliseconds. `0` when `samples == 0`.
    pub avg_ms: u32,
    /// Median check duration in milliseconds. `0` when `samples == 0`.
    pub p50_ms: u32,
    /// 95th-percentile check duration in milliseconds. `0` when `samples == 0`.
    pub p95_ms: u32,
    /// Latest observed status string ("up" / "down" / "degraded" / "error"),
    /// or empty when the range contains no samples.
    pub last_status: String,
    /// Unix-seconds of `max(minute)` over the rollup. `None` when the
    /// range contains no samples.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = true)]
    pub last_minute_ts: Option<i64>,
}

/// One sparkline bucket — minute-aligned average duration. The dashboard
/// renders a fixed 60-minute trace per target so operators see a
/// "right-now" trend independent of the selected aggregation range.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DashboardSparkBucket {
    pub target_id: Uuid,
    /// Unix-seconds for the bucket's `toStartOfMinute(timestamp)`.
    pub bucket_ts: i64,
    pub avg_ms: f32,
    /// Checks behind `avg_ms`, and how many passed — the weights the fleet
    /// sparklines aggregate by.
    pub checks: u64,
    pub up: u64,
}

/// One time-bucket of a single monitor's latency, merged from the
/// `check_results_1m` rollup. Powers both monitor-detail charts (p50/p95/p99
/// line + phase-breakdown area). The server picks a bucket width so any range
/// yields ~60 buckets — switching 1h↔30d actually re-scales the series, and
/// the cost stays O(buckets) instead of pulling raw samples to the client.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct LatencyBucket {
    /// Unix-milliseconds at the bucket's start (JS `new Date(t)`).
    pub t: i64,
    /// Median / 95th / 99th-percentile duration over the bucket, in ms.
    pub p50: u32,
    pub p95: u32,
    pub p99: u32,
    /// Mean total duration over the bucket, in ms. The breakdown chart
    /// derives "processing" time as `avg − (dns+connect+tls+ttfb)`.
    pub avg: u32,
    /// Mean per-phase timings over the bucket, in ms. `0` for check kinds
    /// that don't record the phase (tcp/dns/tls-cert/domain).
    pub dns: u32,
    pub connect: u32,
    pub tls: u32,
    pub ttfb: u32,
    /// Samples in the bucket. `0` marks a gap the chart leaves unconnected.
    pub samples: u64,
}

/// Bucketed latency series for one monitor over a range, plus the bucket
/// width the server chose. Returned by `GET /targets/{id}/latency`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct LatencySeries {
    pub buckets: Vec<LatencyBucket>,
    /// Bucket width in seconds (always a multiple of the 60s rollup grain).
    pub bucket_seconds: u32,
}

/// One region's latency buckets — a single overlay line.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RegionLatencySeries {
    pub region: String,
    /// Display name from the region catalog (the id when unnamed).
    pub label: String,
    pub buckets: Vec<LatencyBucket>,
}

/// Per-region latency for one monitor — each region overlaid as its own line.
/// Returned by `GET /targets/{id}/latency/by-region`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct LatencySeriesByRegion {
    pub regions: Vec<RegionLatencySeries>,
    pub bucket_seconds: u32,
}

/// One time-bucket of a single step's duration.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct FlowStepBucket {
    /// Unix-milliseconds at the bucket's start (JS `new Date(t)`).
    pub t: i64,
    /// Mean duration of the runs that passed the step, in ms. `null` when none
    /// did, which is a gap in the timing rather than an instant step.
    #[schema(nullable = true)]
    pub avg: Option<u32>,
    /// Runs that passed the step — what `avg` is drawn from.
    pub samples: u64,
    /// Runs that reached the step and failed it. Kept out of `avg`: a failed
    /// step sat in its whole step timeout, and a handful of those bury the
    /// timings of every run around them.
    pub failed: u64,
}

/// One declared step's duration over time. A bucket appears whenever the step
/// was reached at all, so a step that only ever fails is distinguishable from
/// one the journey never got to.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct FlowStepTrend {
    /// Zero-based index into the flow's declared steps.
    pub step: u16,
    /// What the newest run in the range recorded here, so an edited flow is
    /// labelled with what it runs today.
    pub op: String,
    pub buckets: Vec<FlowStepBucket>,
}

/// Returned by `GET /targets/{id}/flow-steps`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct FlowStepSeries {
    pub steps: Vec<FlowStepTrend>,
    pub bucket_seconds: u32,
}

/// One region's rollup for a single monitor over a range — drives the
/// per-region breakdown table on the monitor detail page.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RegionRollup {
    pub region: String,
    pub samples: u64,
    pub up: u64,
    pub p50_ms: u32,
    pub p95_ms: u32,
    pub p99_ms: u32,
    pub last_status: String,
}

/// Aggregate health for the period immediately before the selected range
/// — drives the Δ-vs-prior hints on each KPI card. Same shape as the
/// "current" totals so the view layer subtracts cleanly. `avg_ms = 0`
/// when there were no samples.
#[derive(Debug, Clone, Default, Serialize, ToSchema)]
pub struct PriorPeriodSummary {
    pub checks_total: u64,
    pub checks_up: u64,
    pub avg_ms: u32,
}

/// One slice of the fleet 24h uptime ribbon. Aggregates every monitor in
/// the org into a single bucket so the dashboard renders 48 × 30-minute
/// cells from a single matview merge — cost stays O(buckets), not O(orgs
/// × monitors).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct FleetRibbonBucket {
    /// Unix-seconds at the bucket's start (`toStartOfInterval(minute, …)`).
    pub bucket_ts: i64,
    /// Total samples across every monitor in the bucket window.
    pub samples: u64,
    /// Samples with `status = up`.
    pub up: u64,
    /// Monitors with any non-up sample in the bucket — degraded/error too, not
    /// just `down`.
    pub down_targets: Vec<Uuid>,
}

/// One time-bucket of a monitor's availability — up-ratio per bucket drives
/// the uptime-card sparkline. Same bucket grid as [`LatencyBucket`].
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AvailabilityBucket {
    /// Unix-seconds at the bucket's start.
    pub bucket_ts: i64,
    /// Empty buckets are omitted by the query, leaving a gap in the line.
    pub total: u64,
    pub up: u64,
}
