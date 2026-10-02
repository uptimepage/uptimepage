pub mod circuit_breaker;
pub mod dns;
pub mod domain_expiry;
pub mod flow;
pub mod heartbeat;
pub mod host_throttle;
pub mod http_check;
pub mod ping;
pub mod pool;
pub mod rdap;
pub mod rdap_singleflight;
pub mod registration;
pub(crate) mod sweep;
pub mod tcp_check;
pub mod tls_cert;
pub mod whois;

pub use http_check::execute_http_check;
pub(crate) use http_check::{HttpProbe, execute_http_check_probe};
pub use pool::{CheckTask, ResultFanout, WorkerPool, host_for_spec};

use uuid::Uuid;

use crate::domain::{CheckResult, CheckSpec};
use crate::http_client::HttpClients;
use crate::worker::domain_expiry::DomainExpiryRuntime;

/// Per-dispatch dependencies handed to `execute`. Bundles everything an
/// executor sub-handler might need so adding a new dep (e.g. another
/// per-resource bulkhead, another store) doesn't ripple through every call
/// site's argument list.
pub struct WorkerDeps<'a> {
    pub http: &'a HttpClients,
    pub domain_expiry: &'a DomainExpiryRuntime,
    /// Browser-flow engine on a flow-capable node; `None` elsewhere (routing
    /// never sends flow to a node without it).
    pub flow: Option<&'a crate::worker::flow::engine::CdpEngine>,
}

pub async fn execute(
    target_id: Uuid,
    org_id: Uuid,
    spec: &CheckSpec,
    deps: &WorkerDeps<'_>,
) -> CheckResult {
    match spec {
        CheckSpec::Http(http) => execute_http_check(target_id, org_id, http, deps.http).await,
        CheckSpec::Tcp(tcp) => {
            tcp_check::execute_tcp_check(target_id, org_id, tcp, deps.http).await
        }
        CheckSpec::Ping(p) => ping::execute_ping_check(target_id, org_id, p, deps.http).await,
        // Evaluated on the WorkerPool's passive fast path; the WorkerDeps
        // paths (agents, ad-hoc) have no ping state and reject the kind
        // upstream.
        CheckSpec::Heartbeat(_) => CheckResult::error(
            target_id,
            org_id,
            "heartbeat monitors are evaluated on the control plane, not probed",
        ),
        CheckSpec::TlsCert(cert) => {
            tls_cert::execute_tls_cert_check(target_id, org_id, cert, deps.http).await
        }
        CheckSpec::DomainExpiry(domain) => {
            domain_expiry::execute_domain_expiry_check(
                target_id,
                org_id,
                domain,
                deps.domain_expiry,
                deps.http,
            )
            .await
        }
        CheckSpec::Dns(d) => dns::execute_dns_check(target_id, org_id, d, deps.http).await,
        CheckSpec::Flow(flow) => flow::execute_flow_check(target_id, org_id, flow, deps.flow).await,
    }
}

/// Runs a scheduled check and, for a flow, the record of how it got there.
/// Every other kind returns `None`, so nothing on the hot path changes.
pub(crate) async fn execute_recorded(
    target_id: Uuid,
    org_id: Uuid,
    spec: &CheckSpec,
    deps: &WorkerDeps<'_>,
) -> (
    CheckResult,
    Option<crate::domain::agent_wire::FlowRunRecord>,
) {
    let CheckSpec::Flow(f) = spec else {
        return (execute(target_id, org_id, spec, deps).await, None);
    };
    let (result, probe) =
        flow::execute_flow_check_probe(target_id, org_id, f, deps.flow, None).await;
    let record = crate::domain::agent_wire::FlowRunRecord {
        org_id,
        target_id,
        timestamp: result.timestamp,
        status: result.status,
        duration_ms: result.duration_ms,
        error: result.error.clone(),
        steps: probe.steps,
        evidence: probe.evidence,
    };
    (result, Some(record))
}

/// Extra detail a test-check carries back beyond the verdict. One struct
/// rather than a per-kind enum: kinds with no detail just leave it empty.
#[derive(Debug, Clone, Default)]
pub(crate) struct ProbeDetail {
    pub response_headers_preview: Vec<crate::domain::agent_wire::HeaderPreview>,
    pub response_body_snippet: Option<String>,
    pub flow_evidence: Option<crate::domain::agent_wire::FlowEvidence>,
    pub flow_steps: Vec<crate::domain::agent_wire::StepTrace>,
}

impl From<HttpProbe> for ProbeDetail {
    fn from(p: HttpProbe) -> Self {
        Self {
            response_headers_preview: p.response_headers_preview,
            response_body_snippet: p.response_body_snippet,
            flow_evidence: None,
            flow_steps: Vec::new(),
        }
    }
}

/// Verbose variant of `execute` for the test-check UI.
pub(crate) async fn execute_with_probe(
    target_id: Uuid,
    org_id: Uuid,
    spec: &CheckSpec,
    deps: &WorkerDeps<'_>,
) -> (CheckResult, ProbeDetail) {
    match spec {
        CheckSpec::Http(http) => {
            let (r, p) = execute_http_check_probe(target_id, org_id, http, deps.http).await;
            (r, p.into())
        }
        CheckSpec::Flow(flow) => {
            let (r, probe) = flow::execute_flow_check_probe(
                target_id,
                org_id,
                flow,
                deps.flow,
                Some(flow::engine::INTERACTIVE_QUEUE_LIMIT),
            )
            .await;
            (
                r,
                ProbeDetail {
                    flow_evidence: probe.evidence,
                    flow_steps: probe.steps,
                    ..Default::default()
                },
            )
        }
        _ => (
            execute(target_id, org_id, spec, deps).await,
            ProbeDetail::default(),
        ),
    }
}
