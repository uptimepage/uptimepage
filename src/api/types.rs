use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::domain::metrics::{FlowStepTrend, LatencyBucket, RegionLatencySeries, TargetsSummary};
use crate::domain::{CheckResult, CheckSpec};

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
    /// Response headers preview, HTTP only. Sensitive headers redacted;
    /// a value over 512 bytes is cut and ends in `…`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub response_headers_preview: Vec<HeaderPreview>,
    /// First 1 KiB of decoded body, HTTP only. UTF-8 lossy; ends in `…` when cut.
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

/// Bucketed latency series for one monitor over a range, plus the bucket
/// width the server chose. Returned by `GET /targets/{id}/latency`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct LatencySeries {
    pub buckets: Vec<LatencyBucket>,
    /// Bucket width in seconds (always a multiple of the 60s rollup grain).
    pub bucket_seconds: u32,
}

/// Per-region latency for one monitor — each region overlaid as its own line.
/// Returned by `GET /targets/{id}/latency/by-region`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct LatencySeriesByRegion {
    pub regions: Vec<RegionLatencySeries>,
    pub bucket_seconds: u32,
}

/// Returned by `GET /targets/{id}/flow-steps`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct FlowStepSeries {
    pub steps: Vec<FlowStepTrend>,
    pub bucket_seconds: u32,
}
