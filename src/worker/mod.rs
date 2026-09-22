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
pub mod tcp_check;
pub mod tls_cert;
pub mod whois;

pub use http_check::execute_http_check;
pub(crate) use http_check::{HttpProbe, execute_http_check_probe};
pub use pool::{CheckTask, ResultFanout, WorkerPool, host_for_spec};

use std::hash::Hash;
use std::net::SocketAddr;
use std::sync::Arc;

use dashmap::DashMap;
use tokio::net::TcpStream;
use uuid::Uuid;

use crate::domain::{CheckResult, CheckSpec};
use crate::http_client::HttpClients;
use crate::http_client::connector::tcp_reason;
use crate::worker::domain_expiry::DomainExpiryRuntime;

/// Off-hot-path eviction over a `DashMap<K, Arc<T>>`. Drops entries whose
/// only strong reference is the map's own and that the caller-supplied
/// `idle` predicate accepts. Atomic per shard via `DashMap::retain` — never
/// drops an entry another task just cloned out of the map.
///
/// Used by `HostThrottle::sweep`, `WorkerPool::sweep_breakers`, and
/// `RdapSingleflight::sweep`. Three sites converged on this shape so an
/// invariant fix (e.g. tightening the strong-count check) only has to be
/// made in one place.
pub(crate) fn sweep_idle<K, T, F>(map: &DashMap<K, Arc<T>>, idle: F) -> usize
where
    K: Eq + Hash,
    F: Fn(&T) -> bool,
{
    let mut removed = 0usize;
    map.retain(|_, slot| {
        if Arc::strong_count(slot) != 1 {
            return true;
        }
        if idle(slot.as_ref()) {
            removed += 1;
            false
        } else {
            true
        }
    });
    removed
}

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

/// Maps a `days_remaining` value to the canonical Up/Degraded/Down ladder used
/// by the TLS-cert and domain-expiry checks. Negative `days_remaining` always
/// falls below `critical_days` (which is `u32 >= 0`), so expiration is covered
/// by the same branch as the critical threshold.
pub(crate) fn classify_days(
    days_remaining: i64,
    warn_days: u32,
    critical_days: u32,
) -> crate::domain::CheckStatus {
    use crate::domain::CheckStatus;
    if days_remaining < i64::from(critical_days) {
        CheckStatus::Down
    } else if days_remaining < i64::from(warn_days) {
        CheckStatus::Degraded
    } else {
        CheckStatus::Up
    }
}

/// Resolves `host` and keeps only addresses the shared SSRF guard allows.
/// Errors when resolution fails or nothing survives the filter, so callers
/// always receive at least one address. Single chokepoint for the
/// resolve-then-filter dance (TCP, TLS-cert, ping) — a guard-semantics fix
/// lands once.
pub(crate) async fn allowed_addrs(
    host: &str,
    clients: &HttpClients,
) -> anyhow::Result<Vec<std::net::IpAddr>> {
    let guard = clients.ssrf_guard();
    // The resolver's own Display names the query it failed, brace-printed
    // struct and all. Same reason the connect below is normalised: no error
    // class can name that, so it reaches the customer verbatim and lands in
    // `ErrorClass::Other`.
    let resolved = clients.resolver().resolve_addrs(host).await.map_err(|e| {
        let reason = crate::http_client::connector::dns_reason(&e);
        e.context(reason)
    })?;
    let addrs: Vec<std::net::IpAddr> = resolved.into_iter().filter(|ip| guard.allow(*ip)).collect();
    if addrs.is_empty() {
        anyhow::bail!("no allowed addresses for {host}");
    }
    Ok(addrs)
}

/// Tries to open a TCP connection to `(ip, port)` for each allowed address of
/// `host`. Used by TCP and TLS-cert checks.
pub(crate) async fn connect_via_guard(
    host: &str,
    port: u16,
    clients: &HttpClients,
) -> anyhow::Result<TcpStream> {
    let mut last_err: Option<std::io::Error> = None;
    for ip in allowed_addrs(host, clients).await? {
        match TcpStream::connect(SocketAddr::new(ip, port)).await {
            Ok(s) => return Ok(s),
            Err(e) => last_err = Some(e),
        }
    }
    // The raw `io::Error` Display carries a platform-specific errno, which no
    // error class can name and the customer should never read. Kept as the
    // source so an operator can still tell a refused port from a blocked one:
    // `context` is what `to_string` yields, the errno survives in `{:?}`.
    let err = last_err.expect("allowed_addrs yields at least one address");
    tracing::debug!(host, port, error = %err, "connect failed");
    let reason = tcp_reason(&err);
    Err(anyhow::Error::new(err).context(reason))
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
