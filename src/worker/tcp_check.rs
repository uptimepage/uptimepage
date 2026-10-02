use std::time::Instant;

use chrono::Utc;
use tokio::time::timeout;
use uuid::Uuid;

use crate::domain::{CheckResult, CheckStatus, TcpCheck};
use crate::http_client::HttpClients;
use crate::http_client::dial::connect_via_guard;

pub async fn execute_tcp_check(
    target_id: Uuid,
    org_id: Uuid,
    check: &TcpCheck,
    clients: &HttpClients,
) -> CheckResult {
    let started_at = Utc::now();
    let start = Instant::now();

    let outcome = timeout(
        check.timeout,
        connect_via_guard(&check.host, check.port, clients),
    )
    .await;
    let duration_ms = start.elapsed().as_millis() as u32;

    match outcome {
        Ok(Ok(_stream)) => CheckResult {
            target_id,
            org_id,
            timestamp: started_at,
            status: CheckStatus::Up,
            duration_ms,
            dns_ms: None,
            // Clamp like the other phase producers (dns/tls_cert/http): the
            // column is UInt16, so a >65 s connect must saturate, not wrap.
            connect_ms: Some(duration_ms.min(u16::MAX as u32) as u16),
            tls_ms: None,
            ttfb_ms: None,
            response_code: None,
            response_size: None,
            diagnostic: None,
            error: None,
        },
        Ok(Err(err)) => CheckResult {
            target_id,
            org_id,
            timestamp: started_at,
            status: CheckStatus::Down,
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
