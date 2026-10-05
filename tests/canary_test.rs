//! Checks run against real public URLs, on a schedule, never in the PR gate.
//!
//! Every other HTTP test points at a local server that answers exactly what it
//! was told, which is why none of them could have caught a real origin gating
//! compression on the User-Agent or answering an automated client with a
//! challenge page. These target the two behaviours a fake server cannot
//! reproduce without someone already knowing the bug.
//!
//! A failure here is a signal to look, not a broken build: these depend on
//! third parties who may redesign a page or change a policy at any time.
//!
//!     cargo nextest run --test it --run-ignored only -E 'test(/^canary_test::/)'

use std::collections::HashMap;
use std::time::Duration;

use uptimepage::config::{CheckerConfig, DnsConfig, HttpClientConfig, SecurityConfig};
use uptimepage::domain::{
    CheckDiagnosticKind, CheckResult, CheckStatus, ExpectedStatus, HttpCheck, HttpMethod,
};
use uptimepage::http_client::client::build_clients;
use url::Url;
use uuid::Uuid;

/// The real probe, not the test one. `test_client` sends `Uptimepage/test`,
/// and the whole point of the compression case is the exact prefix CDNs read
/// before deciding what to send, so the User-Agent is taken from the app's own
/// default rather than copied here.
fn canary_client() -> uptimepage::http_client::HttpClients {
    let http_cfg: HttpClientConfig =
        serde_json::from_value(serde_json::json!({ "tcp_keepalive_secs": 30 }))
            .expect("http client config defaults");
    let checker_cfg = CheckerConfig {
        max_concurrent_checks: 4,
        default_timeout_ms: 15_000,
        connect_timeout_ms: 5_000,
        default_check_interval_secs: 60,
        per_host_max_inflight: tokio::sync::Semaphore::MAX_PERMITS,
        rdap_max_inflight: tokio::sync::Semaphore::MAX_PERMITS,
    };
    let security_cfg = SecurityConfig {
        // Public targets only: keep the production SSRF posture.
        allow_private_targets: false,
        credentials_kek_base64: secrecy::SecretString::from(String::new()),
        trusted_proxies: vec![],
    };
    let dns_cfg = DnsConfig {
        cache_size: 64,
        positive_ttl_secs: 30,
        negative_ttl_secs: 5,
        servers: vec!["1.1.1.1".into()],
    };
    build_clients(&http_cfg, &checker_cfg, &dns_cfg, &security_cfg).expect("build canary client")
}

/// Shaped like a customer's monitor: expect 200, follow the redirect a bare
/// apex almost always answers with.
async fn probe(url: &str) -> CheckResult {
    let check = HttpCheck {
        url: Url::parse(url).expect("canary url"),
        method: HttpMethod::Get,
        timeout: Duration::from_secs(15),
        follow_redirects: true,
        max_redirects: 3,
        expected_status: ExpectedStatus::Exact(200),
        expected_body_contains: None,
        headers: HashMap::new(),
        body: None,
        verify_tls: true,
        basic_auth: None,
        bearer_token: None,
    };
    uptimepage::worker::execute_http_check(Uuid::now_v7(), Uuid::nil(), &check, &canary_client())
        .await
}

/// Runs first by name. If this fails the runner has no usable egress and every
/// other result in the file is noise, so read this one before filing anything.
#[tokio::test]
#[ignore = "network-dependent; runs on a schedule, not in the PR gate"]
async fn a_control_a_plain_public_endpoint_is_reachable() {
    let result = probe("https://example.com/").await;
    assert_eq!(
        result.status,
        CheckStatus::Up,
        "no egress from this runner, or example.com is down: {:?}",
        result.error
    );
}

/// The regression that shipped: cloud.google.com answers a non-Mozilla agent
/// with megabytes of uncompressed HTML, which blew past the raw read cap and
/// was recorded as a hard error on a page that was answering 200 the whole
/// time. A local server cannot reproduce that without being told to.
#[tokio::test]
#[ignore = "network-dependent; runs on a schedule, not in the PR gate"]
async fn a_ua_gated_cdn_page_is_up_and_fits_the_read_cap() {
    let result = probe("https://cloud.google.com/").await;

    assert_eq!(
        result.status,
        CheckStatus::Up,
        "a healthy 200 must not record an error: {:?}",
        result.error
    );
    // `None` means the read was abandoned at the cap, so the body arrived
    // uncompressed and the User-Agent stopped earning the compressed variant.
    assert!(
        result.response_size.is_some(),
        "body did not fit the read cap, so the CDN did not compress for our agent"
    );
}

/// A live edge that challenges automated clients. Asserts we explain the
/// failure rather than reporting a bare status, which is the difference
/// between a customer reading "403" and reading "an edge blocked the probe".
#[tokio::test]
#[ignore = "network-dependent; runs on a schedule, not in the PR gate"]
async fn a_challenged_request_carries_a_diagnosis_not_just_a_status() {
    let result = probe("https://www.godaddy.com/").await;

    // If this target ever stops challenging us the assertion below is the one
    // that fails, and the fix is to pick another blocked target, not to change
    // the detector.
    assert_ne!(
        result.status,
        CheckStatus::Up,
        "target no longer challenges automated clients; pick another canary"
    );
    let diagnostic = result
        .diagnostic
        .expect("a challenged request must carry a diagnosis");
    assert_eq!(diagnostic.kind, CheckDiagnosticKind::AccessInterference);
}
