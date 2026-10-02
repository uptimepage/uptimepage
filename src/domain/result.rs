use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// Prefix on the `error` field of results synthesized by the domain-expiry
/// executor when serving a cached last-good answer instead of a fresh
/// probe. Internal annotation — every renderer must strip it before
/// showing the field to a customer.
pub const SERVED_STALE_PREFIX: &str = "served_stale:";

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CheckResult {
    pub target_id: Uuid,
    /// Owning tenant of the target. Required (no default) so the type
    /// system forces every result to carry the real org — a wrong/missing
    /// org silently mis-files results under another tenant.
    pub org_id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub status: CheckStatus,
    pub duration_ms: u32,
    pub dns_ms: Option<u16>,
    pub connect_ms: Option<u16>,
    pub tls_ms: Option<u16>,
    pub ttfb_ms: Option<u16>,
    pub response_code: Option<u16>,
    pub response_size: Option<u32>,
    /// Explains a failed result. Never moves the verdict: assertions alone
    /// decide Up/Down.
    #[serde(
        default,
        deserialize_with = "lenient_diagnostic",
        skip_serializing_if = "Option::is_none"
    )]
    #[schema(nullable = true)]
    pub diagnostic: Option<CheckDiagnostic>,
    pub error: Option<String>,
}

impl CheckResult {
    pub fn error(target_id: Uuid, org_id: Uuid, reason: impl Into<String>) -> Self {
        Self::error_with_elapsed(target_id, org_id, Utc::now(), 0, reason)
    }

    pub fn error_with_elapsed(
        target_id: Uuid,
        org_id: Uuid,
        timestamp: DateTime<Utc>,
        duration_ms: u32,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            target_id,
            org_id,
            timestamp,
            status: CheckStatus::Error,
            duration_ms,
            dns_ms: None,
            connect_ms: None,
            tls_ms: None,
            ttfb_ms: None,
            response_code: None,
            response_size: None,
            diagnostic: None,
            error: Some(reason.into()),
        }
    }

    pub fn is_served_stale(&self) -> bool {
        self.error
            .as_deref()
            .is_some_and(|e| e.starts_with(SERVED_STALE_PREFIX))
    }
}

/// A kind from a newer agent would fail the whole ingest batch. A diagnostic
/// only explains a verdict, so drop it alone.
fn lenient_diagnostic<'de, D>(deserializer: D) -> Result<Option<CheckDiagnostic>, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(raw) = Option::<serde_json::Value>::deserialize(deserializer)? else {
        return Ok(None);
    };
    match serde_json::from_value(raw) {
        Ok(diagnostic) => Ok(Some(diagnostic)),
        Err(error) => {
            tracing::warn!(%error, "unreadable check diagnostic dropped from result");
            Ok(None)
        }
    }
}

/// Bounded diagnosis beside a check result. The HTTP error stays the
/// authority; this only attributes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(from = "CheckDiagnosticWire")]
pub struct CheckDiagnostic {
    pub kind: CheckDiagnosticKind,
    pub confidence: DiagnosticConfidence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = true)]
    pub provider: Option<EdgeProvider>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<DiagnosticEvidence>,
    /// Stable action codes for API clients. Derived from the kind, never
    /// stored, so an old row cannot outlive the advice it was written with.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub remediations: Vec<DiagnosticRemediation>,
}

/// Remediations re-derive from `kind` rather than trusting the wire, so a
/// live result and a stored one cannot disagree.
#[derive(Deserialize)]
struct CheckDiagnosticWire {
    kind: CheckDiagnosticKind,
    confidence: DiagnosticConfidence,
    #[serde(default)]
    provider: Option<EdgeProvider>,
    #[serde(default, deserialize_with = "known_evidence")]
    evidence: Vec<DiagnosticEvidence>,
}

fn known_evidence<'de, D>(deserializer: D) -> Result<Vec<DiagnosticEvidence>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Vec::<String>::deserialize(deserializer)?;
    Ok(raw
        .iter()
        .filter_map(|item| DiagnosticEvidence::parse(item))
        .collect())
}

impl From<CheckDiagnosticWire> for CheckDiagnostic {
    fn from(wire: CheckDiagnosticWire) -> Self {
        Self {
            kind: wire.kind,
            confidence: wire.confidence,
            provider: wire.provider,
            evidence: wire.evidence,
            remediations: wire.kind.remediations(),
        }
    }
}

impl CheckDiagnostic {
    pub fn access_interference(
        confidence: DiagnosticConfidence,
        provider: Option<EdgeProvider>,
        evidence: Vec<DiagnosticEvidence>,
    ) -> Self {
        Self {
            kind: CheckDiagnosticKind::AccessInterference,
            confidence,
            provider,
            evidence,
            remediations: CheckDiagnosticKind::AccessInterference.remediations(),
        }
    }

    /// `tunnel` picks the kind, not a remediation: only the kind is stored.
    pub fn origin_unreachable(evidence: Vec<DiagnosticEvidence>, tunnel: bool) -> Self {
        let kind = if tunnel {
            CheckDiagnosticKind::OriginTunnelDown
        } else {
            CheckDiagnosticKind::OriginUnreachable
        };
        Self {
            kind,
            confidence: DiagnosticConfidence::High,
            provider: Some(EdgeProvider::Cloudflare),
            evidence,
            remediations: kind.remediations(),
        }
    }

    pub fn summary(&self) -> String {
        let certainty = match (self.kind, self.confidence) {
            (CheckDiagnosticKind::AccessInterference, DiagnosticConfidence::High) => {
                "access-policy block detected"
            }
            (CheckDiagnosticKind::AccessInterference, DiagnosticConfidence::Medium) => {
                "possible access-policy block"
            }
            (CheckDiagnosticKind::OriginUnreachable, _) => "origin unreachable",
            (CheckDiagnosticKind::OriginTunnelDown, _) => "origin tunnel down",
        };
        match self.provider {
            Some(provider) => format!("{certainty} {} {}", self.kind.joiner(), provider.label()),
            None => certainty.to_string(),
        }
    }

    pub fn guidance(&self) -> &'static str {
        match self.kind {
            CheckDiagnosticKind::AccessInterference => {
                "use an authenticated health endpoint, or allow this monitor through the edge's access rules"
            }
            CheckDiagnosticKind::OriginUnreachable => {
                "check the origin is up and reachable from the edge"
            }
            CheckDiagnosticKind::OriginTunnelDown => {
                "restart the tunnel daemon on the origin, then check the origin is up"
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckDiagnosticKind {
    AccessInterference,
    OriginUnreachable,
    /// Own kind, not a flag: advice derives from the kind, and only the kind
    /// is stored.
    OriginTunnelDown,
}

impl CheckDiagnosticKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AccessInterference => "access_interference",
            Self::OriginUnreachable => "origin_unreachable",
            Self::OriginTunnelDown => "origin_tunnel_down",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "access_interference" => Some(Self::AccessInterference),
            "origin_unreachable" => Some(Self::OriginUnreachable),
            "origin_tunnel_down" => Some(Self::OriginTunnelDown),
            _ => None,
        }
    }

    /// "blocked *at* the edge", but "origin unreachable *behind* it".
    pub const fn joiner(self) -> &'static str {
        match self {
            Self::AccessInterference => "at",
            Self::OriginUnreachable | Self::OriginTunnelDown => "behind",
        }
    }

    pub fn remediations(self) -> Vec<DiagnosticRemediation> {
        match self {
            Self::AccessInterference => vec![
                DiagnosticRemediation::UseAuthenticatedHealthEndpoint,
                DiagnosticRemediation::AllowMonitorThroughEdgeRules,
            ],
            Self::OriginUnreachable => vec![DiagnosticRemediation::VerifyOriginReachable],
            Self::OriginTunnelDown => vec![
                DiagnosticRemediation::VerifyEdgeTunnel,
                DiagnosticRemediation::VerifyOriginReachable,
            ],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticConfidence {
    High,
    Medium,
}

impl DiagnosticConfidence {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "high" => Some(Self::High),
            "medium" => Some(Self::Medium),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum EdgeProvider {
    Akamai,
    AwsWaf,
    Cloudflare,
    AzureFrontDoor,
    DataDome,
    Vercel,
}

impl EdgeProvider {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Akamai => "akamai",
            Self::AwsWaf => "aws_waf",
            Self::Cloudflare => "cloudflare",
            Self::AzureFrontDoor => "azure_front_door",
            Self::DataDome => "data_dome",
            Self::Vercel => "vercel",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Akamai => "the Akamai edge",
            Self::AwsWaf => "AWS WAF",
            Self::Cloudflare => "the Cloudflare edge",
            Self::AzureFrontDoor => "Azure Front Door",
            Self::DataDome => "DataDome",
            Self::Vercel => "the Vercel edge",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "akamai" => Some(Self::Akamai),
            "aws_waf" => Some(Self::AwsWaf),
            "cloudflare" => Some(Self::Cloudflare),
            "azure_front_door" => Some(Self::AzureFrontDoor),
            "data_dome" => Some(Self::DataDome),
            "vercel" => Some(Self::Vercel),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticEvidence {
    ChallengeHeader,
    EdgeServer,
    BlockPage,
    ReferenceId,
    OriginErrorCode,
    MitigationHeader,
}

impl DiagnosticEvidence {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ChallengeHeader => "challenge_header",
            Self::EdgeServer => "edge_server",
            Self::BlockPage => "block_page",
            Self::ReferenceId => "reference_id",
            Self::OriginErrorCode => "origin_error_code",
            Self::MitigationHeader => "mitigation_header",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "challenge_header" => Some(Self::ChallengeHeader),
            "edge_server" => Some(Self::EdgeServer),
            "block_page" => Some(Self::BlockPage),
            "reference_id" => Some(Self::ReferenceId),
            "origin_error_code" => Some(Self::OriginErrorCode),
            "mitigation_header" => Some(Self::MitigationHeader),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticRemediation {
    UseAuthenticatedHealthEndpoint,
    AllowMonitorThroughEdgeRules,
    VerifyOriginReachable,
    VerifyEdgeTunnel,
}

impl DiagnosticRemediation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UseAuthenticatedHealthEndpoint => "use_authenticated_health_endpoint",
            Self::AllowMonitorThroughEdgeRules => "allow_monitor_through_edge_rules",
            Self::VerifyOriginReachable => "verify_origin_reachable",
            Self::VerifyEdgeTunnel => "verify_edge_tunnel",
        }
    }
}

/// The error string with the operator-only `served_stale:` annotation removed,
/// leaving the wrapped reason — the trailing JSON detail, or the bare reason
/// for a non-stale error. Returns `None` when a stale annotation wraps nothing
/// renderable. Every customer-facing surface must route `error` through this.
pub fn strip_served_stale(raw: &str) -> Option<&str> {
    match raw.strip_prefix(SERVED_STALE_PREFIX) {
        None => Some(raw),
        Some(rest) => rest.find('{').map(|i| &rest[i..]),
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Up,
    Down,
    Degraded,
    Error,
}

impl CheckStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Degraded => "degraded",
            Self::Error => "error",
        }
    }

    pub const fn as_enum8(self) -> i8 {
        match self {
            Self::Up => 1,
            Self::Down => 2,
            Self::Degraded => 3,
            Self::Error => 4,
        }
    }

    pub const fn from_enum8(v: i8) -> Self {
        match v {
            1 => Self::Up,
            2 => Self::Down,
            3 => Self::Degraded,
            _ => Self::Error,
        }
    }

    /// The Up/Degraded/Down ladder shared by the TLS-cert and domain-expiry
    /// checks. A negative `days_remaining` always falls below `critical_days`
    /// (a `u32`), so an expired subject takes the same branch as a critical one.
    pub fn from_days_remaining(days_remaining: i64, warn_days: u32, critical_days: u32) -> Self {
        if days_remaining < i64::from(critical_days) {
            Self::Down
        } else if days_remaining < i64::from(warn_days) {
            Self::Degraded
        } else {
            Self::Up
        }
    }

    /// `None` for the view-layer pseudo-statuses ("no_data", "late").
    pub fn from_label(s: &str) -> Option<Self> {
        match s {
            "up" => Some(Self::Up),
            "down" => Some(Self::Down),
            "degraded" => Some(Self::Degraded),
            "error" => Some(Self::Error),
            _ => None,
        }
    }

    /// Exhaustive on purpose: a new variant must classify here, never default
    /// to healthy and silently auto-close incidents.
    pub const fn is_bad(self) -> bool {
        match self {
            Self::Down | Self::Error | Self::Degraded => true,
            Self::Up => false,
        }
    }

    pub const fn severity_rank(self) -> u8 {
        match self {
            Self::Up => 0,
            Self::Degraded => 1,
            Self::Error => 2,
            Self::Down => 3,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_remaining_ladder_breaks_at_warn_and_critical() {
        let status = |days| CheckStatus::from_days_remaining(days, 30, 7);
        assert_eq!(status(31), CheckStatus::Up);
        assert_eq!(status(30), CheckStatus::Up);
        assert_eq!(status(29), CheckStatus::Degraded);
        assert_eq!(status(7), CheckStatus::Degraded);
        assert_eq!(status(6), CheckStatus::Down);
        assert_eq!(status(-1), CheckStatus::Down);
    }

    fn synth(status: CheckStatus, error: Option<&str>) -> CheckResult {
        CheckResult {
            target_id: Uuid::nil(),
            org_id: Uuid::nil(),
            timestamp: Utc::now(),
            status,
            duration_ms: 0,
            dns_ms: None,
            connect_ms: None,
            tls_ms: None,
            ttfb_ms: None,
            response_code: None,
            response_size: None,
            diagnostic: None,
            error: error.map(str::to_owned),
        }
    }

    #[test]
    fn payloads_without_remediations_fill_them_from_their_own_kind() {
        for kind in [
            CheckDiagnosticKind::AccessInterference,
            CheckDiagnosticKind::OriginUnreachable,
            CheckDiagnosticKind::OriginTunnelDown,
        ] {
            let diagnostic: CheckDiagnostic = serde_json::from_value(serde_json::json!({
                "kind": kind.as_str(),
                "confidence": "high",
                "provider": "cloudflare",
                "evidence": ["edge_server"]
            }))
            .expect("agent payload predating the field");

            assert_eq!(diagnostic.remediations, kind.remediations());
        }
    }

    #[test]
    fn wire_remediations_are_ignored_in_favour_of_the_kind() {
        // Storage re-derives on read; trusting the wire only lets them differ.
        let diagnostic: CheckDiagnostic = serde_json::from_value(serde_json::json!({
            "kind": "origin_tunnel_down",
            "confidence": "high",
            "provider": "cloudflare",
            "evidence": ["edge_server"],
            "remediations": ["allow_monitor_through_edge_rules"]
        }))
        .expect("payload with mismatched remediations");

        assert_eq!(
            diagnostic.remediations,
            CheckDiagnosticKind::OriginTunnelDown.remediations()
        );
    }

    #[test]
    fn an_unknown_evidence_tag_does_not_cost_the_diagnostic() {
        let diagnostic: CheckDiagnostic = serde_json::from_value(serde_json::json!({
            "kind": "origin_tunnel_down",
            "confidence": "high",
            "provider": "cloudflare",
            "evidence": ["edge_server", "a_tag_from_a_newer_agent", "reference_id"]
        }))
        .expect("an unknown evidence tag must not fail the diagnostic");

        assert_eq!(diagnostic.kind, CheckDiagnosticKind::OriginTunnelDown);
        assert_eq!(
            diagnostic.evidence,
            vec![
                DiagnosticEvidence::EdgeServer,
                DiagnosticEvidence::ReferenceId,
            ]
        );
    }

    #[test]
    fn a_result_survives_a_diagnostic_kind_this_build_cannot_read() {
        // One unreadable field must not cost a region the whole flush.
        let result: CheckResult = serde_json::from_value(serde_json::json!({
            "target_id": Uuid::nil(),
            "org_id": Uuid::nil(),
            "timestamp": "2026-09-01T02:07:53Z",
            "status": "down",
            "duration_ms": 12,
            "dns_ms": null,
            "connect_ms": null,
            "tls_ms": null,
            "ttfb_ms": null,
            "response_code": 530,
            "response_size": null,
            "diagnostic": {
                "kind": "a_kind_from_a_newer_agent",
                "confidence": "high",
                "provider": "cloudflare",
                "evidence": ["edge_server"]
            },
            "error": "unexpected status 530"
        }))
        .expect("an unknown diagnostic kind must not fail the result");

        assert!(result.diagnostic.is_none());
        assert_eq!(result.response_code, Some(530));
        assert_eq!(result.status, CheckStatus::Down);
        assert_eq!(result.error.as_deref(), Some("unexpected status 530"));
    }

    #[test]
    fn a_readable_diagnostic_still_arrives_intact() {
        let result: CheckResult = serde_json::from_value(serde_json::json!({
            "target_id": Uuid::nil(),
            "org_id": Uuid::nil(),
            "timestamp": "2026-09-01T02:07:53Z",
            "status": "down",
            "duration_ms": 12,
            "response_code": 530,
            "diagnostic": {
                "kind": "origin_tunnel_down",
                "confidence": "high",
                "provider": "cloudflare",
                "evidence": ["edge_server", "reference_id", "origin_error_code"]
            },
            "error": "unexpected status 530"
        }))
        .expect("a known diagnostic kind");

        let diagnostic = result.diagnostic.expect("diagnostic kept");
        assert_eq!(diagnostic.kind, CheckDiagnosticKind::OriginTunnelDown);
        assert_eq!(
            diagnostic.remediations,
            CheckDiagnosticKind::OriginTunnelDown.remediations()
        );
    }

    #[test]
    fn diagnostic_storage_names_round_trip() {
        for kind in [
            CheckDiagnosticKind::AccessInterference,
            CheckDiagnosticKind::OriginUnreachable,
            CheckDiagnosticKind::OriginTunnelDown,
        ] {
            assert_eq!(CheckDiagnosticKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(CheckDiagnosticKind::parse("unknown_future_kind"), None);

        for evidence in [
            DiagnosticEvidence::ChallengeHeader,
            DiagnosticEvidence::EdgeServer,
            DiagnosticEvidence::BlockPage,
            DiagnosticEvidence::ReferenceId,
            DiagnosticEvidence::OriginErrorCode,
            DiagnosticEvidence::MitigationHeader,
        ] {
            assert_eq!(DiagnosticEvidence::parse(evidence.as_str()), Some(evidence));
        }
        assert_eq!(DiagnosticEvidence::parse("unknown_future_evidence"), None);
    }

    #[test]
    fn origin_unreachable_reads_as_english_and_gates_the_tunnel_arm() {
        let tunnel =
            CheckDiagnostic::origin_unreachable(vec![DiagnosticEvidence::EdgeServer], true);
        assert_eq!(tunnel.kind, CheckDiagnosticKind::OriginTunnelDown);
        assert_eq!(
            tunnel.summary(),
            "origin tunnel down behind the Cloudflare edge"
        );
        assert!(
            tunnel
                .remediations
                .contains(&DiagnosticRemediation::VerifyEdgeTunnel)
        );
        // The invariant storage depends on: only the kind decides the advice.
        assert_eq!(tunnel.remediations, tunnel.kind.remediations());

        // Tunnel up, service dead: naming the tunnel would misdirect.
        let service =
            CheckDiagnostic::origin_unreachable(vec![DiagnosticEvidence::EdgeServer], false);
        assert_eq!(
            service.summary(),
            "origin unreachable behind the Cloudflare edge"
        );
        assert!(
            !service
                .remediations
                .contains(&DiagnosticRemediation::VerifyEdgeTunnel)
        );
        assert_eq!(service.remediations, service.kind.remediations());
    }

    #[test]
    fn edge_provider_storage_names_round_trip() {
        for provider in [
            EdgeProvider::Akamai,
            EdgeProvider::AwsWaf,
            EdgeProvider::Cloudflare,
            EdgeProvider::AzureFrontDoor,
            EdgeProvider::DataDome,
            EdgeProvider::Vercel,
        ] {
            assert_eq!(EdgeProvider::parse(provider.as_str()), Some(provider));
        }
        assert_eq!(EdgeProvider::parse("unknown_future_provider"), None);
    }

    #[test]
    fn strip_served_stale_unwraps_or_passes_through() {
        // Non-stale errors pass through untouched.
        assert_eq!(strip_served_stale("no response"), Some("no response"));
        // Stale annotation with a wrapped JSON detail → surface the JSON.
        assert_eq!(
            strip_served_stale("served_stale: age=10; {\"domain\":\"x\"}"),
            Some("{\"domain\":\"x\"}")
        );
        // Stale annotation with nothing renderable → None.
        assert_eq!(strip_served_stale("served_stale: age=10"), None);
    }

    #[test]
    fn is_served_stale_matches_prefix() {
        assert!(
            synth(CheckStatus::Degraded, Some("served_stale: age=10; details")).is_served_stale()
        );
        assert!(synth(CheckStatus::Up, Some("served_stale: age=0")).is_served_stale());
        assert!(!synth(CheckStatus::Degraded, Some("upstream 503")).is_served_stale());
        assert!(!synth(CheckStatus::Degraded, None).is_served_stale());
    }
}
