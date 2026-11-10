use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::domain::metrics::{FlowStepTrend, RegionLatencySeries, TargetsSummary};
use crate::domain::{CheckAdvice, CheckResult, CheckSpec, CheckStatus, explain_check_error};

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
    /// Incidents open at any point in the last 24 hours.
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
    /// Set every target's check interval, in seconds. Kinds that have no
    /// interval or need a longer one are left alone and listed in `failed`.
    SetInterval {
        interval: u32,
    },
    /// Bind each channel to every target that is not bound to it yet.
    ChannelAdd {
        channel_ids: Vec<Uuid>,
    },
    /// Unbind each channel from every target.
    ChannelRemove {
        channel_ids: Vec<Uuid>,
    },
    /// Replace every target's bindings with exactly these channels. An empty
    /// list unbinds every channel; a channel's tag rule still covers the
    /// targets carrying its tags.
    SetChannels {
        channel_ids: Vec<Uuid>,
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

/// What to try about a failed check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct CheckAdviceView {
    /// `diagnostic` when the result carries a diagnostic, whose guidance this
    /// is. Otherwise `hint`: read off the error alone, for the errors a
    /// different check or target would fix.
    #[schema(example = "hint")]
    pub source: &'static str,
    /// Stable identifier. A hint is one of `icmp_no_reply`, `private_address`,
    /// `handshake_dropped`, `name_not_public`, `no_answer`, `access_refused`
    /// or `not_found`; a diagnostic's is its `kind`.
    #[schema(example = "icmp_no_reply")]
    pub code: &'static str,
    /// Plain-language advice. The wording may change; `code` does not.
    pub text: &'static str,
    /// Documentation for the advice, when a page covers it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docs_url: Option<String>,
}

impl From<CheckAdvice> for CheckAdviceView {
    fn from(advice: CheckAdvice) -> Self {
        Self {
            source: advice.source(),
            code: advice.code(),
            text: advice.text(),
            docs_url: advice.docs_path().map(crate::templates::filters::docs_link),
        }
    }
}

/// A check-now result: every `CheckResult` field at the top level, as before,
/// with its error in plain words beside them. The reply has to explain itself:
/// it reaches the caller before the result is stored, so nothing read back
/// from storage can be relied on to.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CheckNowResponse {
    #[serde(flatten)]
    pub result: CheckResult,
    /// `error` in plain words, with what a diagnostic attributes it to. Absent
    /// when the result carries no error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_text: Option<String>,
}

impl From<CheckResult> for CheckNowResponse {
    fn from(result: CheckResult) -> Self {
        Self {
            error_text: explain_check_error(result.error.as_deref(), result.diagnostic.as_ref()),
            result,
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TestResponse {
    pub result: CheckResult,
    /// `result.error` in plain words, with what a diagnostic attributes it to.
    /// Absent when the result carries no error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_text: Option<String>,
    /// It describes this region's run alone: a caller testing several regions
    /// should show it only when every region returns the same `source` and
    /// `code`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advice: Option<CheckAdviceView>,
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

impl TestResponse {
    pub fn new(delivered: crate::ad_hoc_dispatch::DeliveredResult, region: String) -> Self {
        let result = delivered.result;
        Self {
            matched_expectations: result.status == CheckStatus::Up,
            error_text: explain_check_error(result.error.as_deref(), result.diagnostic.as_ref()),
            advice: CheckAdvice::for_result(&result).map(Into::into),
            result,
            warnings: Vec::new(),
            response_headers_preview: delivered.response_headers_preview,
            response_body_snippet: delivered.response_body_snippet,
            flow_evidence: delivered.flow_evidence,
            flow_steps: delivered.flow_steps,
            region: Some(region),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ad_hoc_dispatch::DeliveredResult;
    use crate::domain::{CheckDiagnostic, CheckHint, DiagnosticConfidence, EdgeProvider};

    fn delivered(result: CheckResult) -> DeliveredResult {
        DeliveredResult {
            result,
            response_headers_preview: Vec::new(),
            response_body_snippet: None,
            flow_evidence: None,
            flow_steps: Vec::new(),
        }
    }

    fn failed(reason: &str) -> CheckResult {
        CheckResult {
            status: CheckStatus::Down,
            ..CheckResult::error(Uuid::nil(), Uuid::nil(), reason)
        }
    }

    fn blocked() -> CheckResult {
        CheckResult {
            response_code: Some(403),
            diagnostic: Some(CheckDiagnostic::access_interference(
                DiagnosticConfidence::High,
                Some(EdgeProvider::Cloudflare),
                Vec::new(),
            )),
            ..failed("unexpected status 403")
        }
    }

    fn test_body(result: CheckResult) -> serde_json::Value {
        serde_json::to_value(TestResponse::new(delivered(result), "eu".into())).unwrap()
    }

    #[test]
    fn a_failed_test_explains_itself_beside_the_raw_error() {
        let raw = "no echo reply from 192.0.2.1 within 3000ms";
        let body = test_body(failed(raw));
        assert_eq!(
            body["result"]["error"], raw,
            "the raw error is the contract"
        );
        assert_eq!(body["error_text"], raw);
        assert_eq!(body["advice"]["source"], "hint");
        assert_eq!(body["advice"]["code"], "icmp_no_reply");
        assert_eq!(body["advice"]["text"], CheckHint::IcmpNoReply.text());
        assert_eq!(
            body["advice"]["docs_url"],
            "https://uptimepage.dev/docs/monitor-types#a-new-monitor-fails-but-the-service-works-for-you"
        );
        assert_eq!(body["matched_expectations"], false);
    }

    #[test]
    fn a_terse_code_is_worded_and_an_unhinted_failure_carries_no_advice() {
        let body = test_body(failed("no response"));
        assert_eq!(body["error_text"], "connected, but no response (timed out)");
        assert!(body.get("advice").is_none(), "{body}");

        let missing = test_body(failed("unexpected status 404"));
        assert_eq!(missing["advice"]["code"], "not_found");
        assert!(missing["advice"].get("docs_url").is_none(), "{missing}");
    }

    /// The better-evidenced failure must not get less help than a plain 403.
    #[test]
    fn a_diagnosed_403_carries_its_guidance_and_no_generic_hint() {
        let body = test_body(blocked());
        assert_eq!(
            body["error_text"],
            "unexpected status 403 · access-policy block detected at the Cloudflare edge"
        );
        assert_eq!(body["advice"]["source"], "diagnostic");
        assert_eq!(body["advice"]["code"], "access_interference");
        assert_eq!(
            body["advice"]["text"],
            "use an authenticated health endpoint, or allow this monitor through the edge's access rules"
        );
        assert_ne!(body["advice"]["text"], CheckHint::AccessRefused.text());

        let plain = test_body(failed("unexpected status 403"));
        assert_eq!(plain["advice"]["source"], "hint");
        assert_eq!(plain["advice"]["code"], "access_refused");
    }

    #[test]
    fn a_passing_test_adds_nothing() {
        let mut up = failed("x");
        up.status = CheckStatus::Up;
        up.error = None;
        let body = test_body(up);
        assert_eq!(body["matched_expectations"], true);
        assert!(body.get("error_text").is_none(), "{body}");
        assert!(body.get("advice").is_none(), "{body}");
    }

    /// Clients read the result's fields at the top level; nesting them would
    /// break every one of those reads.
    #[test]
    fn check_now_keeps_every_result_field_at_the_top_level() {
        for result in [failed("tls handshake reset"), blocked()] {
            let bare = serde_json::to_value(&result).unwrap();
            let body = serde_json::to_value(CheckNowResponse::from(result)).unwrap();
            let mut fields = body.as_object().unwrap().clone();
            assert!(fields.remove("error_text").is_some(), "{body}");
            assert_eq!(serde_json::Value::Object(fields), bare);
        }

        let reset =
            serde_json::to_value(CheckNowResponse::from(failed("tls handshake reset"))).unwrap();
        assert_eq!(reset["error"], "tls handshake reset");
        assert_eq!(reset["error_text"], "TLS handshake reset");

        let mut up = failed("x");
        up.status = CheckStatus::Up;
        up.error = None;
        let bare = serde_json::to_value(&up).unwrap();
        assert_eq!(
            serde_json::to_value(CheckNowResponse::from(up)).unwrap(),
            bare,
            "a passing result is byte for byte what it was"
        );
    }
}
