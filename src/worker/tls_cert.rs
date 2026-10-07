use std::time::Instant;

use chrono::Utc;
use serde::Serialize;
use tokio::time::timeout;
use uuid::Uuid;

use crate::domain::{CheckResult, CheckStatus, TlsCertCheck};
use crate::http_client::HttpClients;
use crate::http_client::connector::tls_reason;
use crate::http_client::dial::connect_via_guard;
use crate::security::cert_probe::{self, CertFacts, CertProbeError};

pub async fn execute_tls_cert_check(
    target_id: Uuid,
    org_id: Uuid,
    check: &TlsCertCheck,
    clients: &HttpClients,
) -> CheckResult {
    let started_at = Utc::now();
    let start = Instant::now();

    let outcome = timeout(check.timeout, run_check(check, clients)).await;
    let duration_ms = start.elapsed().as_millis() as u32;

    match outcome {
        Ok(Ok(probe)) => CheckResult {
            target_id,
            org_id,
            timestamp: started_at,
            status: probe.verdict.status,
            duration_ms,
            dns_ms: None,
            connect_ms: None,
            tls_ms: Some(probe.handshake_ms.min(u16::MAX as u32) as u16),
            ttfb_ms: None,
            response_code: None,
            response_size: Some(probe.verdict.details_json.len() as u32),
            diagnostic: None,
            // `error` doubles as the structured details payload for cert
            // checks. Up results stay None (matching every other check
            // type's convention); Degraded/Down carries the JSON document
            // documented in docs/api.md.
            error: match probe.verdict.status {
                CheckStatus::Up => None,
                _ => Some(probe.verdict.details_json),
            },
        },
        Ok(Err(err)) => CheckResult {
            target_id,
            org_id,
            timestamp: started_at,
            status: CheckStatus::Error,
            duration_ms,
            dns_ms: None,
            connect_ms: None,
            tls_ms: None,
            ttfb_ms: None,
            response_code: None,
            response_size: None,
            diagnostic: None,
            error: Some(err.to_string()),
        },
        Err(_) => {
            CheckResult::error_with_elapsed(target_id, org_id, started_at, duration_ms, "timeout")
        }
    }
}

struct ProbeOutcome {
    verdict: CertVerdict,
    handshake_ms: u32,
}

struct CertVerdict {
    status: CheckStatus,
    details_json: String,
}

async fn run_check(check: &TlsCertCheck, clients: &HttpClients) -> anyhow::Result<ProbeOutcome> {
    let stream = connect_via_guard(&check.host, check.port, clients, check.timeout).await?;
    let peer = stream.peer_addr()?.ip();
    let server_name = check.server_name.as_deref().unwrap_or(&check.host);

    let facts = cert_probe::read_cert(stream, server_name, peer)
        .await
        .map_err(|e| probe_error(&check.host, e))?;
    let handshake_ms = facts.handshake_ms;

    Ok(ProbeOutcome {
        verdict: grade(&facts, check),
        handshake_ms,
    })
}

/// The raw `io::Error` Display carries a platform-specific errno, which no
/// error class can name and the customer should never read. `tls_reason`
/// replaces it with a class while the errno survives as the source.
fn probe_error(host: &str, err: CertProbeError) -> anyhow::Error {
    match err {
        CertProbeError::Handshake(io) => {
            tracing::debug!(host, error = %io, "tls handshake failed");
            let reason = tls_reason(&io);
            anyhow::Error::new(io).context(reason)
        }
        other => anyhow::Error::new(other),
    }
}

fn grade(facts: &CertFacts, check: &TlsCertCheck) -> CertVerdict {
    let status = CheckStatus::from_days_remaining(
        facts.days_remaining,
        check.warn_days,
        check.critical_days,
    );

    #[derive(Serialize)]
    struct Details<'a> {
        days_remaining: i64,
        not_after: String,
        subject_common_name: &'a str,
        issuer_common_name: &'a str,
    }
    let details_json = serde_json::to_string(&Details {
        days_remaining: facts.days_remaining,
        not_after: facts.not_after.to_rfc3339(),
        subject_common_name: facts.subject_common_name.as_deref().unwrap_or("<no CN>"),
        issuer_common_name: facts.issuer_common_name.as_deref().unwrap_or("<no CN>"),
    })
    .expect("infallible serialize for fixed struct");

    CertVerdict {
        status,
        details_json,
    }
}
