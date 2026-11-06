//! Wire contract between the control plane and regional agents: config-pull,
//! result-ingest, and ad-hoc dispatch payloads. Serializable DTOs depending
//! only on other `domain` types so both sides share one definition.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::domain::{CheckResult, CheckSpec, Target};

/// HTTP response-header name/value preview returned by a probe. Lives in the
/// wire contract (not `api::types`) so the contract owns every type it
/// serializes; `api::types` re-exports it for the public `TestResponse`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct HeaderPreview {
    pub name: String,
    pub value: String,
}

/// One enabled target served to an agent on config pull.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTargetDto {
    pub org_id: Uuid,
    pub target: Target,
}

/// Far tighter than the result cap: a monitor runs one every 300s at most, so a
/// batch of thousands is a bug or an attempt to make us write rows on demand.
pub const MAX_FLOW_RUNS_PER_BATCH: usize = 256;

/// Config-pull response: every enabled target assigned to the agent's region.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTargetsResponse {
    pub region: String,
    pub targets: Vec<AgentTargetDto>,
}

/// Result-ingest request body (owned; control-plane decode side).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestRequest {
    pub batch_id: Uuid,
    pub results: Vec<CheckResult>,
    /// Alongside the results, never inside a `CheckResult`: that type is the row
    /// every check kind writes. Defaulted so a control plane deployed ahead of
    /// its agents keeps accepting batches.
    #[serde(default)]
    pub flow_runs: Vec<FlowRunRecord>,
}

/// Borrowed mirror of [`IngestRequest`] so the agent serializes a result batch
/// without cloning. Wire-identical to `IngestRequest`.
#[derive(Debug, Serialize)]
pub struct IngestRequestRef<'a> {
    pub batch_id: Uuid,
    pub results: &'a [CheckResult],
    pub flow_runs: &'a [FlowRunRecord],
}

/// What one browser-flow run left behind. The verdict fields are repeated from
/// the `CheckResult` so a run reads without a join.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowRunRecord {
    /// Agent-supplied but never trusted: ingest restamps it from the target's
    /// region assignment, the same as it does for a `CheckResult`.
    pub org_id: Uuid,
    pub target_id: Uuid,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub status: crate::domain::CheckStatus,
    pub duration_ms: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub steps: Vec<StepTrace>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<FlowEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestResponse {
    pub accepted: usize,
    /// Rows silently dropped (future-skewed clock or not assigned to this
    /// region) so a bad clock or stale assignment is visible rather than
    /// poisoning the whole batch.
    pub dropped: usize,
    pub duplicate: bool,
}

/// Which interactive surface an ad-hoc check serves. `test` results are
/// ephemeral (held only until the waiting request reads them); `check_now`
/// results are persisted like a scheduled check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchKind {
    Test,
    CheckNow,
}

/// An ad-hoc check handed to an agent on claim. Carries the decrypted check spec
/// so the agent can probe without DB access, exactly like config pull.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchedCheck {
    pub id: Uuid,
    pub kind: DispatchKind,
    pub org_id: Uuid,
    /// `None` for `test` (no stored target); the target's id for `check_now`.
    pub target_id: Option<Uuid>,
    pub spec: CheckSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchBatch {
    pub checks: Vec<DispatchedCheck>,
}

/// What a failed flow run left on the page, standing in for the screenshot the
/// renderer-less engine cannot take. Bounded at capture; resolved secrets are
/// scrubbed on the control plane, so this crosses the agent wire unredacted.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct FlowEvidence {
    /// Often the whole answer: still on `/login` means the login never took.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = true)]
    pub final_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = true)]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = true)]
    pub text_snippet: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub console: Vec<ConsoleLine>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ConsoleLine {
    /// `log`, `warning`, `error`, and the other console API verbs.
    pub level: String,
    pub text: String,
}

/// How one declared step ended. `Skipped` means the run stopped before the step
/// was reached; anything the run did reach and did not pass is `Failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum StepOutcome {
    Passed,
    Failed,
    Skipped,
}

impl StepOutcome {
    /// Wire value for the ClickHouse `Enum8` column; the migration pins these.
    pub fn as_enum8(self) -> i8 {
        match self {
            Self::Passed => 1,
            Self::Failed => 2,
            Self::Skipped => 3,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }

    /// An unknown value reads as not-reached: claim less about a step, not more.
    pub fn from_enum8(v: i8) -> Self {
        match v {
            1 => Self::Passed,
            2 => Self::Failed,
            _ => Self::Skipped,
        }
    }
}

/// One entry per declared step, positional, so an entry's index is its step
/// index. Carried in full whenever the run reached the step list at all; a run
/// that died before then — no engine on the node, browser never started —
/// carries none rather than a partial one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct StepTrace {
    pub op: String,
    pub outcome: StepOutcome,
    pub duration_ms: u32,
}

/// Result an agent posts back for one claimed check. `result` is always present
/// (a failed probe still yields a `CheckResult`); the probe preview is HTTP
/// `test` detail only, and the evidence and step trace flow `test` detail only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchReport {
    pub check_id: Uuid,
    pub result: CheckResult,
    #[serde(default)]
    pub response_headers_preview: Vec<HeaderPreview>,
    #[serde(default)]
    pub response_body_snippet: Option<String>,
    #[serde(default)]
    pub flow_evidence: Option<FlowEvidence>,
    #[serde(default)]
    pub flow_steps: Vec<StepTrace>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The body is decoded before the handler runs, so one unreadable field
    /// would reject every result in the flush.
    #[test]
    fn a_batch_survives_a_diagnostic_kind_this_build_cannot_read() {
        let req: IngestRequest = serde_json::from_value(serde_json::json!({
            "batch_id": Uuid::nil(),
            "results": [
                {
                    "target_id": Uuid::nil(),
                    "org_id": Uuid::nil(),
                    "timestamp": "2026-09-01T02:07:53Z",
                    "status": "down",
                    "duration_ms": 12,
                    "response_code": 530,
                    "diagnostic": {
                        "kind": "a_kind_from_a_newer_agent",
                        "confidence": "high",
                        "evidence": ["edge_server"]
                    },
                    "error": "unexpected status 530"
                },
                {
                    "target_id": Uuid::nil(),
                    "org_id": Uuid::nil(),
                    "timestamp": "2026-09-01T02:08:53Z",
                    "status": "up",
                    "duration_ms": 40
                }
            ]
        }))
        .expect("an unknown diagnostic kind must not fail the batch");

        assert_eq!(req.results.len(), 2);
        assert!(req.results[0].diagnostic.is_none());
        assert_eq!(req.results[1].status, crate::domain::CheckStatus::Up);
    }
}
