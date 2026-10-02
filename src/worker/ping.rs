use std::net::IpAddr;
use std::time::{Duration, Instant};

use chrono::Utc;
use surge_ping::{Client, Config, ICMP, PingIdentifier, PingSequence};
use uuid::Uuid;

use crate::domain::{CheckResult, CheckStatus, PingCheck};
use crate::http_client::HttpClients;
use crate::http_client::dial::allowed_addrs;

/// Sized like the classic `ping` default so middleboxes treat the probe as
/// ordinary diagnostic traffic.
const PAYLOAD: [u8; 32] = [0u8; 32];

pub async fn execute_ping_check(
    target_id: Uuid,
    org_id: Uuid,
    check: &PingCheck,
    clients: &HttpClients,
) -> CheckResult {
    let started_at = Utc::now();
    let start = Instant::now();

    let outcome = probe(&check.host, check.timeout, clients).await;
    let duration_ms = start.elapsed().as_millis() as u32;

    match outcome {
        Ok(rtt) => CheckResult {
            target_id,
            org_id,
            timestamp: started_at,
            status: CheckStatus::Up,
            duration_ms: rtt.as_millis() as u32,
            dns_ms: None,
            connect_ms: None,
            tls_ms: None,
            ttfb_ms: None,
            response_code: None,
            response_size: None,
            diagnostic: None,
            error: None,
        },
        Err(EchoError::Unreachable(err)) => CheckResult {
            status: CheckStatus::Down,
            ..CheckResult::error_with_elapsed(target_id, org_id, started_at, duration_ms, err)
        },
        Err(EchoError::Probe(err)) => {
            CheckResult::error_with_elapsed(target_id, org_id, started_at, duration_ms, err)
        }
    }
}

enum EchoError {
    /// The probe couldn't run (no ICMP socket) — `Error`, never pages.
    Probe(String),
    /// The host was probed and stayed silent — `Down`.
    Unreachable(String),
}

/// Echoes the first allowed address per family (at most one v4 + one v6),
/// splitting `total` across the attempts — a blackholed first family still
/// leaves budget to reach the healthy one.
async fn probe(host: &str, total: Duration, clients: &HttpClients) -> Result<Duration, EchoError> {
    let addrs = allowed_addrs(host, clients)
        .await
        .map_err(|e| EchoError::Unreachable(e.to_string()))?;
    let first_v4 = addrs.iter().copied().find(IpAddr::is_ipv4);
    let first_v6 = addrs.iter().copied().find(IpAddr::is_ipv6);
    let picks: Vec<IpAddr> = [first_v4, first_v6].into_iter().flatten().collect();
    let per_attempt = total / picks.len().max(1) as u32;
    let mut last_err = None;
    for ip in picks {
        match echo(ip, per_attempt).await {
            Ok(rtt) => return Ok(rtt),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.expect("allowed_addrs yields at least one address"))
}

async fn echo(ip: IpAddr, timeout: Duration) -> Result<Duration, EchoError> {
    // Fresh socket per echo (the fresh-connect discipline of the other
    // executors): the kernel demuxes replies per socket, so concurrent checks
    // can't cross-match, and dropping the Client reaps its recv task.
    let mut config = Config::builder().sock_type_hint(socket2::Type::DGRAM);
    if ip.is_ipv6() {
        config = config.kind(ICMP::V6);
    }
    let client = Client::new(&config.build()).map_err(|e| {
        EchoError::Probe(format!(
            "icmp socket unavailable ({e}); grant CAP_NET_RAW or widen net.ipv4.ping_group_range"
        ))
    })?;
    // Random ident disambiguates concurrent probes on raw-socket (CAP_NET_RAW)
    // deployments, where every raw socket sees every echo reply.
    let mut pinger = client.pinger(ip, PingIdentifier(fastrand::u16(..))).await;
    pinger.timeout(timeout);
    match pinger.ping(PingSequence(0), &PAYLOAD).await {
        Ok((_packet, rtt)) => Ok(rtt),
        // Silence is the only down signal ICMP has; every other failure is
        // probe-side (send error, dead client) and must not page.
        Err(surge_ping::SurgeError::Timeout { .. }) => Err(EchoError::Unreachable(format!(
            "no echo reply from {ip} within {}ms",
            timeout.as_millis()
        ))),
        Err(e) => Err(EchoError::Probe(format!("echo to {ip} failed: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CheckerConfig, DnsConfig, HttpClientConfig, SecurityConfig};
    use crate::http_client::build_clients;

    fn clients(allow_private_targets: bool) -> HttpClients {
        let http_cfg = HttpClientConfig {
            tcp_keepalive_secs: 60,
            user_agent: "test".into(),
        };
        let checker_cfg = CheckerConfig {
            max_concurrent_checks: 8,
            default_timeout_ms: 2_000,
            connect_timeout_ms: 2_000,
            default_check_interval_secs: 60,
            per_host_max_inflight: tokio::sync::Semaphore::MAX_PERMITS,
            rdap_max_inflight: tokio::sync::Semaphore::MAX_PERMITS,
        };
        let dns_cfg = DnsConfig {
            cache_size: 16,
            positive_ttl_secs: 300,
            negative_ttl_secs: 60,
            servers: vec![],
        };
        let security_cfg = SecurityConfig {
            allow_private_targets,
            credentials_kek_base64: secrecy::SecretString::from(String::new()),
            trusted_proxies: vec![],
        };
        build_clients(&http_cfg, &checker_cfg, &dns_cfg, &security_cfg).expect("build clients")
    }

    fn spec(host: &str, timeout_ms: u64) -> PingCheck {
        PingCheck {
            host: host.into(),
            timeout: Duration::from_millis(timeout_ms),
        }
    }

    fn icmp_available() -> bool {
        Client::new(
            &Config::builder()
                .sock_type_hint(socket2::Type::DGRAM)
                .build(),
        )
        .is_ok()
    }

    #[tokio::test]
    async fn loopback_echo_is_up() {
        if !icmp_available() {
            eprintln!("skipping: unprivileged ICMP unavailable on this host");
            return;
        }
        let clients = clients(true);
        let result = execute_ping_check(
            Uuid::new_v4(),
            Uuid::new_v4(),
            &spec("127.0.0.1", 2_000),
            &clients,
        )
        .await;
        assert_eq!(result.status, CheckStatus::Up, "error: {:?}", result.error);
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn ssrf_guarded_host_is_down_with_reason() {
        let clients = clients(false);
        let result = execute_ping_check(
            Uuid::new_v4(),
            Uuid::new_v4(),
            &spec("127.0.0.1", 500),
            &clients,
        )
        .await;
        assert_eq!(result.status, CheckStatus::Down);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|e| e.contains("no allowed addresses")),
            "error: {:?}",
            result.error
        );
    }

    #[tokio::test]
    async fn resolve_failure_is_down() {
        let clients = clients(true);
        let result = execute_ping_check(
            Uuid::new_v4(),
            Uuid::new_v4(),
            &spec("host.invalid", 1_000),
            &clients,
        )
        .await;
        assert_eq!(result.status, CheckStatus::Down);
        assert!(result.error.is_some());
    }
}
