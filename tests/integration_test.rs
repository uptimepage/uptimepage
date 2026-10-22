use crate::common;

use std::collections::HashMap;
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::http::header::{COOKIE, HOST, SET_COOKIE};
use axum::http::uri::Authority;
use axum::http::{HeaderMap, StatusCode, Uri, Version, header};
use axum::routing::get;
use uptimepage::domain::{
    CheckDiagnosticKind, CheckStatus, DiagnosticConfidence, DiagnosticEvidence,
    DiagnosticRemediation, EdgeProvider, ExpectedStatus, HttpCheck, HttpMethod,
};
use uptimepage::http_client::{H1_MAX_HEADERS, HttpClients};
use uptimepage::storage::{InMemorySink, ResultSink};
use uptimepage::worker::execute_http_check;
use url::Url;
use uuid::Uuid;

use crate::common::{
    default_http_check, link_lines, router_with, spawn_mock_dns, spawn_router,
    spawn_self_signed_tls_router, test_client, test_client_with_dns, test_client_with_failing_dns,
};

#[tokio::test]
async fn http_check_returns_up_on_200() {
    let app = Router::new().route("/health", get(|| async { "pong" }));
    let addr = spawn_router(app).await;

    let client = test_client();
    let url = Url::parse(&format!("http://{addr}/health")).unwrap();
    let check = default_http_check(url, ExpectedStatus::Exact(200));

    let result = execute_http_check(Uuid::now_v7(), uuid::Uuid::nil(), &check, &client).await;

    assert_eq!(result.status, CheckStatus::Up);
    assert_eq!(result.response_code, Some(200));
    assert_eq!(result.response_size, Some(4));
    assert!(result.error.is_none());
    // Per-phase timings populated for the breakdown chart (#31). Plain HTTP has
    // no TLS phase; connect + ttfb are always timed.
    assert!(result.connect_ms.is_some(), "connect phase must be timed");
    assert!(result.tls_ms.is_none(), "plain http has no tls phase");
    assert!(result.ttfb_ms.is_some(), "ttfb must be timed");
    assert!(
        result.network.is_none(),
        "a passing check needs no explaining"
    );
}

/// Echoes request headers, joining repeats with `|` so a duplicated name is
/// visible to a body assertion.
fn header_echo_router() -> Router {
    Router::new().route(
        "/",
        get(|headers: header::HeaderMap| async move {
            let joined = |name: header::HeaderName| {
                headers
                    .get_all(name)
                    .iter()
                    .map(|v| v.to_str().unwrap_or("<non-ascii>"))
                    .collect::<Vec<_>>()
                    .join("|")
            };
            format!(
                "ua=[{}] accept=[{}] encoding=[{}]",
                joined(header::USER_AGENT),
                joined(header::ACCEPT),
                joined(header::ACCEPT_ENCODING),
            )
        }),
    )
}

#[tokio::test]
async fn http_check_sends_one_identity_and_accept_header() {
    let addr = spawn_router(header_echo_router()).await;
    let mut check = default_http_check(
        Url::parse(&format!("http://{addr}/")).unwrap(),
        ExpectedStatus::Exact(200),
    );
    check.expected_body_contains =
        Some("ua=[Uptimepage/test] accept=[*/*] encoding=[gzip, br]".into());

    let result = execute_http_check(Uuid::now_v7(), Uuid::nil(), &check, &test_client()).await;

    assert_eq!(result.status, CheckStatus::Up, "{:?}", result.error);
}

#[tokio::test]
async fn configured_headers_replace_probe_defaults_instead_of_joining_them() {
    let addr = spawn_router(header_echo_router()).await;
    let mut check = default_http_check(
        Url::parse(&format!("http://{addr}/")).unwrap(),
        ExpectedStatus::Exact(200),
    );
    check
        .headers
        .insert("User-Agent".into(), "acme-monitor/9".into());
    check
        .headers
        .insert("accept".into(), "application/json".into());
    // Accept-Encoding is not theirs to set: only gzip and br can be decoded,
    // so a configured codec would hand every assertion undecodable bytes.
    check
        .headers
        .insert("Accept-Encoding".into(), "zstd".into());
    check.expected_body_contains =
        Some("ua=[acme-monitor/9] accept=[application/json] encoding=[gzip, br]".into());

    let result = execute_http_check(Uuid::now_v7(), Uuid::nil(), &check, &test_client()).await;

    assert_eq!(result.status, CheckStatus::Up, "{:?}", result.error);
}

#[tokio::test]
async fn a_rejected_header_value_leaves_the_probe_default_in_place() {
    let addr = spawn_router(header_echo_router()).await;
    let mut check = default_http_check(
        Url::parse(&format!("http://{addr}/")).unwrap(),
        ExpectedStatus::Exact(200),
    );
    // A newline can never reach the wire; the probe must not end up nameless.
    check
        .headers
        .insert("user-agent".into(), "bad\nvalue".into());
    check.expected_body_contains = Some("ua=[Uptimepage/test]".into());

    let result = execute_http_check(Uuid::now_v7(), Uuid::nil(), &check, &test_client()).await;

    assert_eq!(result.status, CheckStatus::Up, "{:?}", result.error);
}

/// More than the probe buffers, uncompressed, like a large marketing page.
fn oversize_body_router() -> Router {
    Router::new().route("/", get(|| async { "x".repeat(2 << 20) }))
}

#[tokio::test]
async fn a_page_larger_than_the_read_cap_still_passes_a_status_only_check() {
    let addr = spawn_router(oversize_body_router()).await;
    let check = default_http_check(
        Url::parse(&format!("http://{addr}/")).unwrap(),
        ExpectedStatus::Exact(200),
    );

    let result = execute_http_check(Uuid::now_v7(), Uuid::nil(), &check, &test_client()).await;

    assert_eq!(result.status, CheckStatus::Up, "{:?}", result.error);
    assert_eq!(result.response_code, Some(200));
    // Abandoned read: length unknown, not the cap.
    assert_eq!(result.response_size, None);
}

#[tokio::test]
async fn a_body_assertion_over_the_read_cap_fails_and_says_why() {
    let addr = spawn_router(oversize_body_router()).await;
    let mut check = default_http_check(
        Url::parse(&format!("http://{addr}/")).unwrap(),
        ExpectedStatus::Exact(200),
    );
    check.expected_body_contains = Some("needle".into());

    let result = execute_http_check(Uuid::now_v7(), Uuid::nil(), &check, &test_client()).await;

    assert_eq!(result.status, CheckStatus::Error);
    assert_eq!(result.response_code, Some(200));
    assert_eq!(
        result.error.as_deref(),
        Some("body over the 1 MiB read cap")
    );
}

#[tokio::test]
async fn h1_response_with_many_header_lines_is_up() {
    let addr = spawn_router(router_with(link_lines(150), Version::HTTP_11)).await;
    let check = default_http_check(
        Url::parse(&format!("http://{addr}/")).unwrap(),
        ExpectedStatus::Exact(200),
    );

    let result = execute_http_check(Uuid::now_v7(), Uuid::nil(), &check, &test_client()).await;

    assert_eq!(result.status, CheckStatus::Up, "{:?}", result.error);
    assert_eq!(result.response_code, Some(200));
}

#[tokio::test]
async fn h1_response_over_the_header_limit_names_the_probe() {
    let addr = spawn_router(router_with(
        link_lines(H1_MAX_HEADERS + 1),
        Version::HTTP_11,
    ))
    .await;
    let check = default_http_check(
        Url::parse(&format!("http://{addr}/")).unwrap(),
        ExpectedStatus::Exact(200),
    );

    let result = execute_http_check(Uuid::now_v7(), Uuid::nil(), &check, &test_client()).await;

    assert_eq!(result.status, CheckStatus::Error);
    assert_eq!(result.error.as_deref(), Some("response headers too large"));
}

#[tokio::test]
async fn http_check_returns_down_on_unexpected_status() {
    let app = Router::new().route(
        "/broken",
        get(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "boom") }),
    );
    let addr = spawn_router(app).await;

    let client = test_client();
    let url = Url::parse(&format!("http://{addr}/broken")).unwrap();
    let check = default_http_check(url, ExpectedStatus::Exact(200));

    let result = execute_http_check(Uuid::now_v7(), uuid::Uuid::nil(), &check, &client).await;

    assert_eq!(result.status, CheckStatus::Down);
    assert_eq!(result.response_code, Some(500));
    assert!(result.error.is_some());
}

#[tokio::test]
async fn http_check_names_the_server_that_answered() {
    let app = Router::new().route("/gone", get(|| async { StatusCode::NOT_FOUND }));
    let addr = spawn_router(app).await;

    let client = test_client();
    let url = Url::parse(&format!("http://{addr}/gone")).unwrap();
    let check = default_http_check(url, ExpectedStatus::Exact(200));

    let result = execute_http_check(Uuid::now_v7(), uuid::Uuid::nil(), &check, &client).await;

    assert_eq!(result.response_code, Some(404));
    assert_eq!(result.remote_ip(), Some(addr.ip()));
}

#[tokio::test]
async fn http_check_diagnoses_akamai_access_denial_without_changing_verdict() {
    let body = r#"<HTML><HEAD><TITLE>Access Denied</TITLE></HEAD><BODY>
        <H1>Access Denied</H1>
        You don't have permission to access this server.
        <P>Reference&#32;#18.5c1f1602</P>
        <P>https&#58;&#47;&#47;errors&#46;edgesuite&#46;net&#47;18.5c1f1602</P>
        </BODY></HTML>"#;
    let app = Router::new().route(
        "/",
        get(move || async move {
            (
                StatusCode::FORBIDDEN,
                [(header::SERVER, "AkamaiGHost")],
                body,
            )
        }),
    );
    let addr = spawn_router(app).await;
    let check = default_http_check(
        Url::parse(&format!("http://{addr}/")).unwrap(),
        ExpectedStatus::Exact(200),
    );

    let result = execute_http_check(Uuid::now_v7(), Uuid::nil(), &check, &test_client()).await;

    assert_eq!(result.status, CheckStatus::Down);
    assert_eq!(result.response_code, Some(403));
    assert_eq!(result.error.as_deref(), Some("unexpected status 403"));
    let diagnostic = result.diagnostic.expect("Akamai denial diagnosis");
    assert_eq!(diagnostic.kind, CheckDiagnosticKind::AccessInterference);
    assert_eq!(diagnostic.confidence, DiagnosticConfidence::High);
    assert_eq!(diagnostic.provider, Some(EdgeProvider::Akamai));
    assert_eq!(
        diagnostic.evidence,
        vec![
            DiagnosticEvidence::EdgeServer,
            DiagnosticEvidence::BlockPage,
            DiagnosticEvidence::ReferenceId,
        ]
    );
    assert_eq!(
        diagnostic.remediations,
        vec![
            DiagnosticRemediation::UseAuthenticatedHealthEndpoint,
            DiagnosticRemediation::AllowMonitorThroughEdgeRules,
        ]
    );
}

#[tokio::test]
async fn http_check_uses_cloudflare_documented_challenge_header() {
    let challenge_page = r#"<!DOCTYPE html><html lang="en-US"><head>
        <title>Just a moment...</title>
        <meta http-equiv="content-security-policy"
              content="default-src 'none'; script-src https://challenges.cloudflare.com">
        </head><body>Performing security verification</body></html>"#;
    let mut compressed = Vec::new();
    {
        let mut encoder = brotli::CompressorWriter::new(&mut compressed, 4096, 5, 22);
        encoder.write_all(challenge_page.as_bytes()).unwrap();
    }
    let app = Router::new().route(
        "/",
        get(move || {
            let compressed = compressed.clone();
            async move {
                (
                    StatusCode::FORBIDDEN,
                    [
                        (header::CONTENT_TYPE, "text/html; charset=UTF-8"),
                        (header::CONTENT_ENCODING, "br"),
                        (header::SERVER, "cloudflare"),
                        (header::HeaderName::from_static("cf-mitigated"), "challenge"),
                    ],
                    compressed,
                )
            }
        }),
    );
    let addr = spawn_router(app).await;
    let check = default_http_check(
        Url::parse(&format!("http://{addr}/")).unwrap(),
        ExpectedStatus::Exact(200),
    );

    let result = execute_http_check(Uuid::now_v7(), Uuid::nil(), &check, &test_client()).await;

    assert_eq!(result.status, CheckStatus::Down);
    assert_eq!(result.response_size, Some(challenge_page.len() as u32));
    let diagnostic = result.diagnostic.expect("Cloudflare challenge diagnosis");
    assert_eq!(diagnostic.confidence, DiagnosticConfidence::High);
    assert_eq!(diagnostic.provider, Some(EdgeProvider::Cloudflare));
    assert_eq!(
        diagnostic.evidence,
        vec![DiagnosticEvidence::ChallengeHeader]
    );
}

#[tokio::test]
async fn http_check_names_a_dead_origin_behind_cloudflare() {
    let error_page = include_str!("fixtures/cloudflare/tunnel_down_530.html");
    let app = Router::new().route(
        "/",
        get(move || async move {
            (
                StatusCode::from_u16(530).unwrap(),
                [
                    (header::CONTENT_TYPE, "text/html; charset=UTF-8"),
                    (header::SERVER, "cloudflare"),
                    (
                        header::HeaderName::from_static("cf-ray"),
                        "a340f0bcd922fd58-LCA",
                    ),
                ],
                error_page,
            )
        }),
    );
    let addr = spawn_router(app).await;
    let check = default_http_check(
        Url::parse(&format!("http://{addr}/")).unwrap(),
        ExpectedStatus::Exact(200),
    );

    let result = execute_http_check(Uuid::now_v7(), Uuid::nil(), &check, &test_client()).await;

    assert_eq!(result.status, CheckStatus::Down);
    let diagnostic = result.diagnostic.expect("origin-unreachable diagnosis");
    assert_eq!(diagnostic.kind, CheckDiagnosticKind::OriginTunnelDown);
    assert_eq!(diagnostic.confidence, DiagnosticConfidence::High);
    assert_eq!(diagnostic.provider, Some(EdgeProvider::Cloudflare));
    assert_eq!(
        diagnostic.evidence,
        vec![
            DiagnosticEvidence::EdgeServer,
            DiagnosticEvidence::ReferenceId,
            DiagnosticEvidence::OriginErrorCode,
        ]
    );
    assert_eq!(
        diagnostic.remediations,
        vec![
            DiagnosticRemediation::VerifyEdgeTunnel,
            DiagnosticRemediation::VerifyOriginReachable,
        ]
    );
    assert_eq!(
        diagnostic.summary(),
        "origin tunnel down behind the Cloudflare edge"
    );
}

/// A customer's own 502 must reach the operator unattributed.
#[tokio::test]
async fn http_check_leaves_a_relayed_origin_error_unattributed() {
    let app = Router::new().route(
        "/",
        get(|| async {
            (
                StatusCode::BAD_GATEWAY,
                [
                    (header::CONTENT_TYPE, "text/html; charset=UTF-8"),
                    (header::SERVER, "cloudflare"),
                    (
                        header::HeaderName::from_static("cf-ray"),
                        "a340f0bcd922fd58-LCA",
                    ),
                    (
                        header::HeaderName::from_static("cf-cache-status"),
                        "DYNAMIC",
                    ),
                ],
                "<!doctype html><h1>upstream said 502</h1>",
            )
        }),
    );
    let addr = spawn_router(app).await;
    let check = default_http_check(
        Url::parse(&format!("http://{addr}/")).unwrap(),
        ExpectedStatus::Exact(200),
    );

    let result = execute_http_check(Uuid::now_v7(), Uuid::nil(), &check, &test_client()).await;

    assert_eq!(result.status, CheckStatus::Down);
    assert!(result.diagnostic.is_none(), "{:?}", result.diagnostic);
}

#[tokio::test]
async fn http_check_uses_aws_waf_documented_challenge_header() {
    let app = Router::new().route(
        "/",
        get(|| async {
            (
                StatusCode::ACCEPTED,
                [(
                    header::HeaderName::from_static("x-amzn-waf-action"),
                    "challenge",
                )],
                "<!doctype html><script>run browser challenge</script>",
            )
        }),
    );
    let addr = spawn_router(app).await;
    let check = default_http_check(
        Url::parse(&format!("http://{addr}/")).unwrap(),
        ExpectedStatus::Exact(200),
    );

    let result = execute_http_check(Uuid::now_v7(), Uuid::nil(), &check, &test_client()).await;

    assert_eq!(result.status, CheckStatus::Down);
    assert_eq!(result.response_code, Some(202));
    assert_eq!(result.error.as_deref(), Some("unexpected status 202"));
    let diagnostic = result.diagnostic.expect("AWS WAF challenge diagnosis");
    assert_eq!(diagnostic.confidence, DiagnosticConfidence::High);
    assert_eq!(diagnostic.provider, Some(EdgeProvider::AwsWaf));
    assert_eq!(
        diagnostic.evidence,
        vec![DiagnosticEvidence::ChallengeHeader]
    );
}

#[tokio::test]
async fn http_check_requires_corroboration_for_vercel_challenge() {
    let app = Router::new().route(
        "/",
        get(|| async {
            (
                StatusCode::FORBIDDEN,
                [
                    (header::SERVER, "Vercel"),
                    (
                        header::HeaderName::from_static("x-vercel-mitigated"),
                        "challenge",
                    ),
                    (
                        header::HeaderName::from_static("x-vercel-challenge-token"),
                        "opaque-test-token",
                    ),
                ],
                "<!doctype html><title>Vercel Security Checkpoint</title>",
            )
        }),
    );
    let addr = spawn_router(app).await;
    let check = default_http_check(
        Url::parse(&format!("http://{addr}/")).unwrap(),
        ExpectedStatus::Exact(200),
    );

    let result = execute_http_check(Uuid::now_v7(), Uuid::nil(), &check, &test_client()).await;

    assert_eq!(result.status, CheckStatus::Down);
    assert_eq!(result.response_code, Some(403));
    let diagnostic = result.diagnostic.expect("corroborated Vercel challenge");
    assert_eq!(diagnostic.confidence, DiagnosticConfidence::Medium);
    assert_eq!(diagnostic.provider, Some(EdgeProvider::Vercel));
    assert_eq!(
        diagnostic.evidence,
        vec![
            DiagnosticEvidence::ChallengeHeader,
            DiagnosticEvidence::EdgeServer,
            DiagnosticEvidence::BlockPage,
        ]
    );
}

/// Seen in production: one region got `challenge`, the other `deny`, and only
/// the first was explained.
#[tokio::test]
async fn http_check_explains_a_vercel_hard_deny() {
    let app = Router::new().route(
        "/",
        get(|| async {
            (
                StatusCode::FORBIDDEN,
                [
                    (header::SERVER, "Vercel"),
                    (
                        header::HeaderName::from_static("x-vercel-mitigated"),
                        "deny",
                    ),
                ],
                "",
            )
        }),
    );
    let addr = spawn_router(app).await;
    let check = default_http_check(
        Url::parse(&format!("http://{addr}/")).unwrap(),
        ExpectedStatus::Exact(200),
    );

    let result = execute_http_check(Uuid::now_v7(), Uuid::nil(), &check, &test_client()).await;

    assert_eq!(result.status, CheckStatus::Down);
    assert_eq!(result.response_code, Some(403));
    assert_eq!(result.error.as_deref(), Some("unexpected status 403"));
    let diagnostic = result
        .diagnostic
        .expect("a deny is attributed from headers alone");
    assert_eq!(diagnostic.kind, CheckDiagnosticKind::AccessInterference);
    assert_eq!(diagnostic.provider, Some(EdgeProvider::Vercel));
    assert_eq!(diagnostic.confidence, DiagnosticConfidence::Medium);
    assert_eq!(
        diagnostic.evidence,
        vec![
            DiagnosticEvidence::MitigationHeader,
            DiagnosticEvidence::EdgeServer,
        ]
    );
    assert_eq!(
        diagnostic.remediations,
        vec![
            DiagnosticRemediation::UseAuthenticatedHealthEndpoint,
            DiagnosticRemediation::AllowMonitorThroughEdgeRules,
        ],
        "no browser challenge was issued, so no challenge-only advice"
    );
    assert_eq!(
        diagnostic.summary(),
        "possible access-policy block at the Vercel edge"
    );
}

#[tokio::test]
async fn http_check_does_not_infer_block_from_google_frontend_headers() {
    let app = Router::new().route(
        "/",
        get(|| async {
            (
                StatusCode::FORBIDDEN,
                [
                    (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                    (header::SERVER, "ESF"),
                    (header::CACHE_CONTROL, "no-cache, no-store, max-age=0"),
                    (
                        header::HeaderName::from_static("x-ua-compatible"),
                        "IE=edge",
                    ),
                    (
                        header::HeaderName::from_static("x-frame-options"),
                        "SAMEORIGIN",
                    ),
                ],
                "<!doctype html><html><head><title>Google Cloud</title></head></html>",
            )
        }),
    );
    let addr = spawn_router(app).await;
    let check = default_http_check(
        Url::parse(&format!("http://{addr}/")).unwrap(),
        ExpectedStatus::Exact(200),
    );

    let result = execute_http_check(Uuid::now_v7(), Uuid::nil(), &check, &test_client()).await;

    assert_eq!(result.status, CheckStatus::Down);
    assert_eq!(result.response_code, Some(403));
    assert!(result.diagnostic.is_none());
}

#[tokio::test]
async fn http_check_does_not_guess_provider_from_server_header_alone() {
    let app = Router::new().route(
        "/",
        get(|| async {
            (
                StatusCode::FORBIDDEN,
                [(header::SERVER, "AkamaiGHost")],
                r#"{"error":"account is disabled"}"#,
            )
        }),
    );
    let addr = spawn_router(app).await;
    let check = default_http_check(
        Url::parse(&format!("http://{addr}/")).unwrap(),
        ExpectedStatus::Exact(200),
    );

    let result = execute_http_check(Uuid::now_v7(), Uuid::nil(), &check, &test_client()).await;

    assert_eq!(result.status, CheckStatus::Down);
    assert!(result.diagnostic.is_none());
}

#[tokio::test]
async fn http_check_does_not_diagnose_a_configured_success_status() {
    let app = Router::new().route(
        "/",
        get(|| async {
            (
                StatusCode::FORBIDDEN,
                [("cf-mitigated", "challenge")],
                "browser verification required",
            )
        }),
    );
    let addr = spawn_router(app).await;
    let check = default_http_check(
        Url::parse(&format!("http://{addr}/")).unwrap(),
        ExpectedStatus::Exact(403),
    );

    let result = execute_http_check(Uuid::now_v7(), Uuid::nil(), &check, &test_client()).await;

    assert_eq!(result.status, CheckStatus::Up);
    assert!(result.error.is_none());
    assert!(result.diagnostic.is_none());
}

#[tokio::test]
async fn http_check_status_range_matches() {
    let app = Router::new().route("/", get(|| async { (StatusCode::ACCEPTED, "") }));
    let addr = spawn_router(app).await;

    let client = test_client();
    let url = Url::parse(&format!("http://{addr}/")).unwrap();
    let check = default_http_check(url, ExpectedStatus::Range { min: 200, max: 299 });

    let result = execute_http_check(Uuid::now_v7(), uuid::Uuid::nil(), &check, &client).await;

    assert_eq!(result.status, CheckStatus::Up);
    assert_eq!(result.response_code, Some(202));
}

#[tokio::test]
async fn http_check_body_match_failure_is_down() {
    let app = Router::new().route("/", get(|| async { "hello world" }));
    let addr = spawn_router(app).await;

    let client = test_client();
    let url = Url::parse(&format!("http://{addr}/")).unwrap();
    let mut check = default_http_check(url, ExpectedStatus::Exact(200));
    check.expected_body_contains = Some("goodbye".into());

    let result = execute_http_check(Uuid::now_v7(), uuid::Uuid::nil(), &check, &client).await;

    assert_eq!(result.status, CheckStatus::Down);
}

#[tokio::test]
async fn http_check_connection_refused_is_error() {
    let client = test_client();
    let url = Url::parse("http://127.0.0.1:1/").unwrap();
    let check = default_http_check(url, ExpectedStatus::Exact(200));

    let result = execute_http_check(Uuid::now_v7(), uuid::Uuid::nil(), &check, &client).await;

    assert_eq!(result.status, CheckStatus::Error);
    assert!(result.error.is_some());
    assert_eq!(result.remote_ip(), None, "nothing answered");
    let tried = result.network.map(|e| e.tried_ips).unwrap_or_default();
    assert_eq!(tried, vec![std::net::IpAddr::from([127, 0, 0, 1])]);
}

#[tokio::test]
async fn http_check_dns_failure_is_error() {
    let client = test_client_with_failing_dns();
    let url = Url::parse("http://nonexistent.invalid./").unwrap();
    let mut check = default_http_check(url, ExpectedStatus::Exact(200));
    check.timeout = Duration::from_millis(500);

    let started = std::time::Instant::now();
    let result = execute_http_check(Uuid::now_v7(), uuid::Uuid::nil(), &check, &client).await;
    let elapsed = started.elapsed();

    assert_eq!(result.status, CheckStatus::Error);
    assert!(result.error.is_some());
    assert!(result.response_code.is_none());
    assert!(
        elapsed < Duration::from_secs(1),
        "dns resolution should not escape request timeout: elapsed {elapsed:?}"
    );
}

#[tokio::test]
async fn http_check_total_timeout_is_error() {
    let app = Router::new().route(
        "/slow",
        get(|| async {
            tokio::time::sleep(Duration::from_secs(2)).await;
            "late"
        }),
    );
    let addr = spawn_router(app).await;

    let client = test_client();
    let url = Url::parse(&format!("http://{addr}/slow")).unwrap();
    let mut check = default_http_check(url, ExpectedStatus::Exact(200));
    check.timeout = Duration::from_millis(150);

    let started = std::time::Instant::now();
    let result = execute_http_check(Uuid::now_v7(), uuid::Uuid::nil(), &check, &client).await;
    let elapsed = started.elapsed();

    assert_eq!(result.status, CheckStatus::Error);
    // Connects fast, then the slow handler never replies in budget.
    assert_eq!(result.error.as_deref(), Some("no response"));
    assert!(result.connect_ms.is_some());
    assert!(result.ttfb_ms.is_none());
    assert!(
        elapsed < Duration::from_millis(500),
        "timeout not enforced: elapsed {elapsed:?}"
    );
}

#[tokio::test]
async fn http_check_head_with_gzip_content_encoding_is_up() {
    use axum::http::header;
    use axum::routing::head;
    let app = Router::new().route(
        "/h",
        head(|| async {
            (
                StatusCode::OK,
                [(header::CONTENT_ENCODING, "gzip")],
                axum::body::Body::empty(),
            )
        }),
    );
    let addr = spawn_router(app).await;

    let client = test_client();
    let url = Url::parse(&format!("http://{addr}/h")).unwrap();
    let mut check = default_http_check(url, ExpectedStatus::Exact(200));
    check.method = HttpMethod::Head;

    let result = execute_http_check(Uuid::now_v7(), uuid::Uuid::nil(), &check, &client).await;

    assert_eq!(result.status, CheckStatus::Up);
    assert_eq!(result.response_code, Some(200));
    assert_eq!(result.response_size, Some(0));
    assert!(result.error.is_none());
}

#[tokio::test]
async fn http_check_body_decode_error_preserves_response_code() {
    // Status line came back, body decode failed — response_code must persist.
    use axum::http::header;
    let app = Router::new().route(
        "/g",
        get(|| async {
            (
                StatusCode::OK,
                [(header::CONTENT_ENCODING, "gzip")],
                axum::body::Body::from(b"not actually gzipped".to_vec()),
            )
        }),
    );
    let addr = spawn_router(app).await;
    let client = test_client();
    let url = Url::parse(&format!("http://{addr}/g")).unwrap();
    let check = default_http_check(url, ExpectedStatus::Exact(200));

    let result = execute_http_check(Uuid::now_v7(), uuid::Uuid::nil(), &check, &client).await;

    assert_eq!(result.status, CheckStatus::Error);
    assert_eq!(result.error.as_deref(), Some("decode"));
    assert_eq!(result.response_code, Some(200));
    assert!(result.ttfb_ms.is_some());
}

#[tokio::test]
async fn http_check_body_decode_error_preserves_header_only_diagnostic() {
    let app = Router::new().route(
        "/challenge",
        get(|| async {
            (
                StatusCode::FORBIDDEN,
                [
                    (header::CONTENT_ENCODING, "gzip"),
                    (header::HeaderName::from_static("cf-mitigated"), "challenge"),
                ],
                axum::body::Body::from(b"not actually gzipped".to_vec()),
            )
        }),
    );
    let addr = spawn_router(app).await;
    let check = default_http_check(
        Url::parse(&format!("http://{addr}/challenge")).unwrap(),
        ExpectedStatus::Exact(200),
    );

    let result = execute_http_check(Uuid::now_v7(), Uuid::nil(), &check, &test_client()).await;

    assert_eq!(result.status, CheckStatus::Error);
    assert_eq!(result.error.as_deref(), Some("decode"));
    assert_eq!(result.response_code, Some(403));
    let diagnostic = result
        .diagnostic
        .expect("header-only diagnosis survives body failure");
    assert_eq!(diagnostic.confidence, DiagnosticConfidence::High);
    assert_eq!(diagnostic.provider, Some(EdgeProvider::Cloudflare));
    assert_eq!(
        diagnostic.evidence,
        vec![DiagnosticEvidence::ChallengeHeader]
    );
}

#[tokio::test]
async fn in_memory_sink_collects_results() {
    let app = Router::new().route("/", get(|| async { "ok" }));
    let addr = spawn_router(app).await;

    let client = test_client();
    let sink = Arc::new(InMemorySink::new());
    let url = Url::parse(&format!("http://{addr}/")).unwrap();
    let check = default_http_check(url, ExpectedStatus::Exact(200));

    let result = execute_http_check(Uuid::now_v7(), uuid::Uuid::nil(), &check, &client).await;
    sink.write_batch(&[result]).await.unwrap();

    assert_eq!(sink.len(), 1);
    assert_eq!(sink.snapshot()[0].status, CheckStatus::Up);
}

// ── Redirect following (regression: apex domains 301 to www/https) ──────────

use axum::http::header::LOCATION;
use axum::response::IntoResponse;

fn moved(code: StatusCode, to: &'static str) -> impl IntoResponse {
    (code, [(LOCATION, to)])
}

#[tokio::test]
async fn a_failure_after_a_redirect_names_the_hop_that_failed() {
    let app = Router::new().route(
        "/",
        get(|| async { moved(StatusCode::MOVED_PERMANENTLY, "/gone?token=s3cret&page=2") }),
    );
    let addr = spawn_router(app).await;

    let url = Url::parse(&format!("http://{addr}/")).unwrap();
    let mut check = default_http_check(url, ExpectedStatus::Exact(200));
    check.follow_redirects = true;
    check.max_redirects = 5;

    let result =
        execute_http_check(Uuid::now_v7(), uuid::Uuid::nil(), &check, &test_client()).await;

    assert_eq!(result.response_code, Some(404));
    let hop = result.network.and_then(|e| e.hop_url).expect("hop url");
    assert!(hop.starts_with(&format!("http://{addr}/gone?")), "{hop}");
    assert!(hop.contains("page=2"), "{hop}");
    assert!(!hop.contains("s3cret"), "{hop}");
}

#[tokio::test]
async fn a_failure_on_the_first_hop_names_no_hop() {
    let app = Router::new().route("/", get(|| async { StatusCode::NOT_FOUND }));
    let addr = spawn_router(app).await;

    let url = Url::parse(&format!("http://{addr}/")).unwrap();
    let result = execute_http_check(
        Uuid::now_v7(),
        uuid::Uuid::nil(),
        &default_http_check(url, ExpectedStatus::Exact(200)),
        &test_client(),
    )
    .await;

    assert_eq!(result.network.and_then(|e| e.hop_url), None);
}

#[tokio::test]
async fn http_check_follows_redirect_to_up() {
    let app = Router::new()
        .route(
            "/",
            get(|| async { moved(StatusCode::MOVED_PERMANENTLY, "/health") }),
        )
        .route("/health", get(|| async { "pong" }));
    let addr = spawn_router(app).await;

    let client = test_client();
    let url = Url::parse(&format!("http://{addr}/")).unwrap();
    let mut check = default_http_check(url, ExpectedStatus::Exact(200));
    check.follow_redirects = true;
    check.max_redirects = 5;

    let result = execute_http_check(Uuid::now_v7(), uuid::Uuid::nil(), &check, &client).await;

    assert_eq!(result.status, CheckStatus::Up);
    assert_eq!(result.response_code, Some(200));
    assert!(result.error.is_none());
}

#[tokio::test]
async fn http_check_redirect_not_followed_is_down() {
    let app = Router::new().route(
        "/",
        get(|| async { moved(StatusCode::MOVED_PERMANENTLY, "/health") }),
    );
    let addr = spawn_router(app).await;

    let client = test_client();
    let url = Url::parse(&format!("http://{addr}/")).unwrap();
    // default_http_check leaves follow_redirects = false.
    let check = default_http_check(url, ExpectedStatus::Exact(200));

    let result = execute_http_check(Uuid::now_v7(), uuid::Uuid::nil(), &check, &client).await;

    assert_eq!(result.status, CheckStatus::Down);
    assert_eq!(result.response_code, Some(301));
    assert_eq!(result.error.as_deref(), Some("unexpected status 301"));
}

#[tokio::test]
async fn http_check_follows_redirect_chain_within_budget() {
    let app = Router::new()
        .route("/a", get(|| async { moved(StatusCode::FOUND, "/b") }))
        .route(
            "/b",
            get(|| async { moved(StatusCode::SEE_OTHER, "/health") }),
        )
        .route("/health", get(|| async { "pong" }));
    let addr = spawn_router(app).await;

    let client = test_client();
    let url = Url::parse(&format!("http://{addr}/a")).unwrap();
    let mut check = default_http_check(url, ExpectedStatus::Exact(200));
    check.follow_redirects = true;
    check.max_redirects = 5;

    let result = execute_http_check(Uuid::now_v7(), uuid::Uuid::nil(), &check, &client).await;

    assert_eq!(result.status, CheckStatus::Up);
    assert_eq!(result.response_code, Some(200));
}

#[tokio::test]
async fn http_check_redirect_loop_hits_budget() {
    let app = Router::new()
        .route("/loop", get(|| async { moved(StatusCode::FOUND, "/loop") }))
        .route(
            "/anchor",
            get(|| async { moved(StatusCode::FOUND, "/anchor#top") }),
        );
    let addr = spawn_router(app).await;

    let client = test_client();
    for path in ["/loop", "/anchor"] {
        let url = Url::parse(&format!("http://{addr}{path}")).unwrap();
        let mut check = default_http_check(url, ExpectedStatus::Exact(200));
        check.follow_redirects = true;
        check.max_redirects = 2;

        let result = execute_http_check(Uuid::now_v7(), uuid::Uuid::nil(), &check, &client).await;

        assert_eq!(result.status, CheckStatus::Error, "{path}");
        assert_eq!(result.error.as_deref(), Some("redirect loop"), "{path}");
        assert_eq!(
            result.response_code,
            Some(302),
            "the 3xx that exceeded the cap must be preserved for diagnostics",
        );
    }
}

#[tokio::test]
async fn a_chain_across_different_urls_over_the_budget_is_too_many_redirects() {
    let app = Router::new()
        .route("/a", get(|| async { moved(StatusCode::FOUND, "/b") }))
        .route("/b", get(|| async { moved(StatusCode::FOUND, "/c") }))
        .route("/c", get(|| async { moved(StatusCode::FOUND, "/d") }))
        .route("/d", get(|| async { "pong" }));
    let addr = spawn_router(app).await;

    let url = Url::parse(&format!("http://{addr}/a")).unwrap();
    let mut check = default_http_check(url, ExpectedStatus::Exact(200));
    check.follow_redirects = true;
    check.max_redirects = 2;

    let result =
        execute_http_check(Uuid::now_v7(), uuid::Uuid::nil(), &check, &test_client()).await;

    assert_eq!(result.error.as_deref(), Some("too many redirects"));
    assert_eq!(result.response_code, Some(302));
}

#[tokio::test]
async fn http_check_307_preserves_method_and_body() {
    use axum::routing::post;

    let app = Router::new()
        .route(
            "/start",
            post(|| async { moved(StatusCode::TEMPORARY_REDIRECT, "/echo") }),
        )
        // GET would 405 here — proving the hop stayed POST.
        .route(
            "/echo",
            post(|body: String| async move { format!("echo:{body}") }),
        );
    let addr = spawn_router(app).await;

    let client = test_client();
    let url = Url::parse(&format!("http://{addr}/start")).unwrap();
    let mut check = default_http_check(url, ExpectedStatus::Exact(200));
    check.method = uptimepage::domain::HttpMethod::Post;
    check.body = Some("ping".to_string());
    check.expected_body_contains = Some("echo:ping".to_string());
    check.follow_redirects = true;
    check.max_redirects = 5;

    let result = execute_http_check(Uuid::now_v7(), uuid::Uuid::nil(), &check, &client).await;

    assert_eq!(result.status, CheckStatus::Up, "307 must keep POST + body");
    assert_eq!(result.response_code, Some(200));
}

#[tokio::test]
async fn http_check_strips_credentials_cross_origin() {
    // Foreign origin: 200 only when NO Authorization header arrived.
    let foreign = Router::new().route(
        "/secure",
        get(|headers: HeaderMap| async move {
            if headers.contains_key(axum::http::header::AUTHORIZATION) {
                (StatusCode::UNAUTHORIZED, "leaked")
            } else {
                (StatusCode::OK, "clean")
            }
        }),
    );
    let foreign_addr = spawn_router(foreign).await;

    let origin = Router::new().route(
        "/",
        get(move || async move {
            moved(
                StatusCode::FOUND,
                Box::leak(format!("http://{foreign_addr}/secure").into_boxed_str()),
            )
        }),
    );
    let origin_addr = spawn_router(origin).await;

    let client = test_client();
    let url = Url::parse(&format!("http://{origin_addr}/")).unwrap();
    let mut check = default_http_check(url, ExpectedStatus::Exact(200));
    check.bearer_token = Some("super-secret".to_string());
    check.expected_body_contains = Some("clean".to_string());
    check.follow_redirects = true;
    check.max_redirects = 5;

    let result = execute_http_check(Uuid::now_v7(), uuid::Uuid::nil(), &check, &client).await;

    assert_eq!(
        result.status,
        CheckStatus::Up,
        "bearer token must not cross to a foreign origin"
    );
}

/// Answers `/` with a 307 back to itself and a cookie until the cookie comes
/// back, then with `then`.
fn cookie_gate(then: StatusCode) -> Router {
    Router::new().route(
        "/",
        get(move |headers: HeaderMap| async move {
            let passed = headers
                .get_all(COOKIE)
                .iter()
                .any(|v| v.to_str().is_ok_and(|v| v.contains("gate=passed")));
            if passed {
                (then, "real page").into_response()
            } else {
                (
                    StatusCode::TEMPORARY_REDIRECT,
                    [
                        (LOCATION, "/"),
                        (SET_COOKIE, "gate=passed; Path=/; HttpOnly"),
                    ],
                )
                    .into_response()
            }
        }),
    )
}

fn following(url: String) -> HttpCheck {
    let mut check = default_http_check(Url::parse(&url).unwrap(), ExpectedStatus::Exact(200));
    check.follow_redirects = true;
    check.max_redirects = 5;
    check
}

#[tokio::test]
async fn a_cookie_gated_self_redirect_reaches_the_real_page() {
    let addr = spawn_router(cookie_gate(StatusCode::OK)).await;

    let check = following(format!("http://{addr}/"));
    let result = execute_http_check(Uuid::now_v7(), Uuid::nil(), &check, &test_client()).await;

    assert_eq!(result.status, CheckStatus::Up, "{:?}", result.error);
    assert_eq!(result.response_code, Some(200));
}

#[tokio::test]
async fn a_cycle_between_two_urls_is_a_loop() {
    let (addr, _) = spawn_cookie_site(vec![
        ("127.0.0.1/a", redirect_to("/b", &[])),
        ("127.0.0.1/b", redirect_to("/a", &[])),
        (
            "www.example.com/",
            redirect_to("http://example.com:{port}/", &[]),
        ),
        (
            "example.com/",
            redirect_to("http://www.example.com:{port}/", &[]),
        ),
    ])
    .await;
    let client = client_resolving(&["www.example.com", "example.com"]).await;

    for url in [
        format!("http://{addr}/a"),
        format!("http://www.example.com:{}/", addr.port()),
    ] {
        let result = execute_http_check(
            Uuid::now_v7(),
            Uuid::nil(),
            &following(url.clone()),
            &client,
        )
        .await;
        assert_eq!(result.error.as_deref(), Some("redirect loop"), "{url}");
    }
}

#[tokio::test]
async fn a_cycle_that_brings_a_new_cookie_back_is_not_yet_a_loop() {
    // `/b` sets the cookie, so the second visit to `/b` is a new request.
    let (addr, _) = spawn_cookie_site(vec![
        ("127.0.0.1/a", redirect_to("/b", &[])),
        ("127.0.0.1/b", redirect_to("/a", &["c=1"])),
    ])
    .await;

    for (budget, error) in [(2, "too many redirects"), (3, "redirect loop")] {
        let mut check = following(format!("http://{addr}/a"));
        check.max_redirects = budget;
        let result = execute_http_check(Uuid::now_v7(), Uuid::nil(), &check, &test_client()).await;
        assert_eq!(result.error.as_deref(), Some(error), "budget {budget}");
    }
}

#[tokio::test]
async fn a_gate_the_budget_ends_on_is_not_called_a_loop() {
    let app = cookie_gate(StatusCode::OK)
        .route("/start", get(|| async { moved(StatusCode::FOUND, "/") }));
    let addr = spawn_router(app).await;

    let mut check = following(format!("http://{addr}/start"));
    check.max_redirects = 1;
    let result = execute_http_check(Uuid::now_v7(), Uuid::nil(), &check, &test_client()).await;

    assert_eq!(result.error.as_deref(), Some("too many redirects"));
    assert_eq!(result.response_code, Some(307));
}

#[tokio::test]
async fn a_see_other_back_to_a_post_is_not_a_loop() {
    use axum::routing::post;

    let app = Router::new()
        .route(
            "/start",
            post(|| async { moved(StatusCode::TEMPORARY_REDIRECT, "/") }),
        )
        .route("/", post(|| async { moved(StatusCode::SEE_OTHER, "/") }));
    let addr = spawn_router(app).await;

    let mut check = following(format!("http://{addr}/start"));
    check.method = HttpMethod::Post;
    check.body = Some("ping".to_owned());
    check.max_redirects = 1;
    let result = execute_http_check(Uuid::now_v7(), Uuid::nil(), &check, &test_client()).await;

    assert_eq!(
        result.error.as_deref(),
        Some("too many redirects"),
        "the GET the 303 asks for was never sent"
    );
    assert_eq!(result.response_code, Some(303));
}

#[tokio::test]
async fn a_gate_that_never_accepts_its_cookie_is_a_loop() {
    let app = Router::new().route(
        "/",
        get(|| async {
            (
                StatusCode::TEMPORARY_REDIRECT,
                [(LOCATION, "/"), (SET_COOKIE, "gate=passed")],
            )
        }),
    );
    let addr = spawn_router(app).await;

    let result = execute_http_check(
        Uuid::now_v7(),
        Uuid::nil(),
        &following(format!("http://{addr}/")),
        &test_client(),
    )
    .await;

    assert_eq!(result.error.as_deref(), Some("redirect loop"));
}

#[tokio::test]
async fn a_cookie_gated_self_redirect_reports_the_real_status() {
    for (status, error) in [
        (StatusCode::SERVICE_UNAVAILABLE, "rate-limited 503"),
        (StatusCode::INTERNAL_SERVER_ERROR, "unexpected status 500"),
    ] {
        let addr = spawn_router(cookie_gate(status)).await;

        let check = following(format!("http://{addr}/"));
        let result = execute_http_check(Uuid::now_v7(), Uuid::nil(), &check, &test_client()).await;

        assert_eq!(result.response_code, Some(status.as_u16()));
        assert_eq!(result.error.as_deref(), Some(error));
    }
}

/// What a cookie site answers at one `host/path`.
struct Hop {
    status: StatusCode,
    location: Option<String>,
    set_cookies: Vec<String>,
}

fn redirect_to(location: impl Into<String>, set_cookies: &[&str]) -> Hop {
    Hop {
        status: StatusCode::FOUND,
        location: Some(location.into()),
        set_cookies: set_cookies.iter().map(|c| (*c).to_owned()).collect(),
    }
}

fn landing() -> Hop {
    Hop {
        status: StatusCode::OK,
        location: None,
        set_cookies: Vec::new(),
    }
}

/// Each request a cookie site saw: `host/path` and every `Cookie` header on it.
type Seen = Arc<Mutex<Vec<(String, Vec<String>)>>>;

/// Answers each `host/path` from `hops` and 404 otherwise. `{port}` in a
/// `Location` becomes the port the request came in on.
fn cookie_site(hops: Vec<(impl Into<String>, Hop)>) -> (Router, Seen) {
    let seen = Seen::default();
    let log = Arc::clone(&seen);
    let hops: Arc<HashMap<String, Hop>> =
        Arc::new(hops.into_iter().map(|(at, hop)| (at.into(), hop)).collect());
    let app = Router::new().fallback(move |uri: Uri, headers: HeaderMap| {
        let (hops, log) = (Arc::clone(&hops), Arc::clone(&log));
        async move {
            let authority: Authority = headers
                .get(HOST)
                .and_then(|h| h.to_str().ok())
                .or(uri.authority().map(Authority::as_str))
                .and_then(|a| a.parse().ok())
                .expect("request names its host");
            let at = format!("{}{}", authority.host(), uri.path());
            let cookies = headers
                .get_all(COOKIE)
                .iter()
                .map(|v| v.to_str().unwrap().to_owned())
                .collect();
            log.lock().unwrap().push((at.clone(), cookies));
            let Some(hop) = hops.get(&at) else {
                return StatusCode::NOT_FOUND.into_response();
            };
            let mut response = axum::http::Response::builder().status(hop.status);
            if let Some(location) = &hop.location {
                let port = authority.port_u16().unwrap_or_default().to_string();
                response = response.header(LOCATION, location.replace("{port}", &port));
            }
            for cookie in &hop.set_cookies {
                response = response.header(SET_COOKIE, cookie);
            }
            response.body(axum::body::Body::from("ok")).unwrap()
        }
    });
    (app, seen)
}

async fn spawn_cookie_site(hops: Vec<(impl Into<String>, Hop)>) -> (SocketAddr, Seen) {
    let (app, seen) = cookie_site(hops);
    (spawn_router(app).await, seen)
}

/// The `Cookie` headers of the first request to `at`.
fn cookies_at(seen: &Seen, at: &str) -> Vec<String> {
    seen.lock()
        .unwrap()
        .iter()
        .find(|(path, _)| path == at)
        .map(|(_, cookies)| cookies.clone())
        .unwrap_or_else(|| panic!("{at} was never requested"))
}

/// Points every name at loopback, so hops can change cookie host on one server.
async fn client_resolving(names: &[&str]) -> HttpClients {
    let records: Vec<_> = names
        .iter()
        .map(|name| (*name, Ipv4Addr::LOCALHOST, 60))
        .collect();
    test_client_with_dns(spawn_mock_dns(&records, false).await.addr)
}

async fn run(check: &HttpCheck, client: &HttpClients) -> uptimepage::domain::CheckResult {
    let result = execute_http_check(Uuid::now_v7(), Uuid::nil(), check, client).await;
    assert_eq!(result.status, CheckStatus::Up, "{:?}", result.error);
    result
}

#[tokio::test]
async fn every_set_cookie_on_a_redirect_is_stored_and_sent() {
    let (addr, seen) = spawn_cookie_site(vec![
        (
            "127.0.0.1/",
            redirect_to(
                "/read",
                &["a=1", "b=2; Expires=Wed, 21 Oct 2099 07:28:00 GMT", "c=3"],
            ),
        ),
        ("127.0.0.1/read", landing()),
    ])
    .await;

    run(&following(format!("http://{addr}/")), &test_client()).await;

    assert_eq!(cookies_at(&seen, "127.0.0.1/read"), ["a=1; b=2; c=3"]);
}

#[tokio::test]
async fn a_malformed_set_cookie_leaves_the_valid_one_alone() {
    let (addr, seen) = spawn_cookie_site(vec![
        (
            "127.0.0.1/",
            redirect_to("/read", &["no pair here", "ok=1", "=; Path"]),
        ),
        ("127.0.0.1/read", landing()),
    ])
    .await;

    run(&following(format!("http://{addr}/")), &test_client()).await;

    assert_eq!(cookies_at(&seen, "127.0.0.1/read"), ["ok=1"]);
}

#[tokio::test]
async fn a_cookie_stays_with_the_host_that_set_it() {
    let (addr, seen) = spawn_cookie_site(vec![
        (
            "a.example.com/",
            redirect_to("http://other.test:{port}/read", &["a=1"]),
        ),
        (
            "other.test/read",
            redirect_to("http://a.example.com:{port}/back", &[]),
        ),
        ("a.example.com/back", landing()),
    ])
    .await;
    let client = client_resolving(&["a.example.com", "other.test"]).await;

    let port = addr.port();
    run(&following(format!("http://a.example.com:{port}/")), &client).await;

    assert!(cookies_at(&seen, "other.test/read").is_empty());
    assert_eq!(cookies_at(&seen, "a.example.com/back"), ["a=1"]);
}

#[tokio::test]
async fn a_cookie_follows_its_host_to_another_port() {
    let (other_port, seen) = spawn_cookie_site(vec![("127.0.0.1/read", landing())]).await;
    let (addr, _) = spawn_cookie_site(vec![(
        "127.0.0.1/",
        redirect_to(format!("http://{other_port}/read"), &["a=1"]),
    )])
    .await;

    run(&following(format!("http://{addr}/")), &test_client()).await;

    assert_eq!(cookies_at(&seen, "127.0.0.1/read"), ["a=1"]);
}

#[tokio::test]
async fn a_domain_cookie_reaches_a_subdomain() {
    let (addr, seen) = spawn_cookie_site(vec![
        (
            "a.example.com/",
            redirect_to(
                "http://www.example.com:{port}/read",
                &["shared=1; Domain=example.com", "own=1"],
            ),
        ),
        ("www.example.com/read", landing()),
    ])
    .await;
    let client = client_resolving(&["a.example.com", "www.example.com"]).await;

    let port = addr.port();
    run(&following(format!("http://a.example.com:{port}/")), &client).await;

    assert_eq!(cookies_at(&seen, "www.example.com/read"), ["shared=1"]);
}

#[tokio::test]
async fn a_path_cookie_stays_under_its_path() {
    let (addr, seen) = spawn_cookie_site(vec![
        ("127.0.0.1/", redirect_to("/a/b", &["p=1; Path=/a"])),
        ("127.0.0.1/a/b", redirect_to("/b", &[])),
        ("127.0.0.1/b", landing()),
    ])
    .await;

    run(&following(format!("http://{addr}/")), &test_client()).await;

    assert_eq!(cookies_at(&seen, "127.0.0.1/a/b"), ["p=1"]);
    assert!(cookies_at(&seen, "127.0.0.1/b").is_empty());
}

#[tokio::test]
async fn a_secure_cookie_never_rides_a_plain_http_hop() {
    let client = client_resolving(&["a.example.com"]).await;
    // Loopback too: plain http there is not https.
    for host in ["a.example.com", "127.0.0.1"] {
        let read = format!("{host}/read");
        let (plain, seen) = spawn_cookie_site(vec![(read.clone(), landing())]).await;
        let (tls_site, _) = cookie_site(vec![(
            format!("{host}/"),
            redirect_to(
                format!("http://{host}:{}/read", plain.port()),
                &["s=1; Secure", "n=1"],
            ),
        )]);
        let tls = spawn_self_signed_tls_router(tls_site).await;

        let mut check = following(format!("https://{host}:{}/", tls.port()));
        check.verify_tls = false;
        run(&check, &client).await;

        assert_eq!(cookies_at(&seen, &read), ["n=1"], "{host}");
    }
}

#[tokio::test]
async fn a_cookie_for_a_public_suffix_is_refused() {
    let (addr, seen) = spawn_cookie_site(vec![
        (
            "www.example.co.uk/",
            redirect_to(
                "http://www.other.co.uk:{port}/read",
                &["icann=1; Domain=co.uk"],
            ),
        ),
        (
            "www.other.co.uk/read",
            redirect_to("http://a.github.io:{port}/", &[]),
        ),
        (
            "a.github.io/",
            redirect_to(
                "http://b.github.io:{port}/read",
                &["private=1; Domain=github.io"],
            ),
        ),
        ("b.github.io/read", landing()),
    ])
    .await;
    let client = client_resolving(&[
        "www.example.co.uk",
        "www.other.co.uk",
        "a.github.io",
        "b.github.io",
    ])
    .await;

    let port = addr.port();
    run(
        &following(format!("http://www.example.co.uk:{port}/")),
        &client,
    )
    .await;

    assert!(cookies_at(&seen, "www.other.co.uk/read").is_empty());
    assert!(cookies_at(&seen, "b.github.io/read").is_empty());
}

#[tokio::test]
async fn a_suffix_cookie_from_the_suffix_host_itself_stays_host_only() {
    let (addr, seen) = spawn_cookie_site(vec![
        (
            "github.io/",
            redirect_to("http://a.github.io:{port}/read", &["x=1; Domain=github.io"]),
        ),
        (
            "a.github.io/read",
            redirect_to("http://github.io:{port}/back", &[]),
        ),
        ("github.io/back", landing()),
    ])
    .await;
    let client = client_resolving(&["github.io", "a.github.io"]).await;

    let port = addr.port();
    run(&following(format!("http://github.io:{port}/")), &client).await;

    assert!(cookies_at(&seen, "a.github.io/read").is_empty());
    assert_eq!(cookies_at(&seen, "github.io/back"), ["x=1"]);
}

#[tokio::test]
async fn a_registrable_parent_domain_cookie_reaches_a_sibling() {
    let (addr, seen) = spawn_cookie_site(vec![
        (
            "www.example.co.uk/",
            redirect_to(
                "http://api.example.co.uk:{port}/read",
                &["p=1; Domain=example.co.uk"],
            ),
        ),
        ("api.example.co.uk/read", landing()),
    ])
    .await;
    let client = client_resolving(&["www.example.co.uk", "api.example.co.uk"]).await;

    let port = addr.port();
    run(
        &following(format!("http://www.example.co.uk:{port}/")),
        &client,
    )
    .await;

    assert_eq!(cookies_at(&seen, "api.example.co.uk/read"), ["p=1"]);
}

#[tokio::test]
async fn a_replaced_cookie_sends_its_new_value() {
    let (addr, seen) = spawn_cookie_site(vec![
        ("127.0.0.1/", redirect_to("/two", &["v=1"])),
        ("127.0.0.1/two", redirect_to("/read", &["v=2"])),
        ("127.0.0.1/read", landing()),
    ])
    .await;

    run(&following(format!("http://{addr}/")), &test_client()).await;

    assert_eq!(cookies_at(&seen, "127.0.0.1/two"), ["v=1"]);
    assert_eq!(cookies_at(&seen, "127.0.0.1/read"), ["v=2"]);
}

#[tokio::test]
async fn a_cookie_deleted_with_max_age_zero_is_not_sent() {
    let (addr, seen) = spawn_cookie_site(vec![
        ("127.0.0.1/", redirect_to("/two", &["v=1", "keep=1"])),
        ("127.0.0.1/two", redirect_to("/read", &["v=; Max-Age=0"])),
        ("127.0.0.1/read", landing()),
    ])
    .await;

    run(&following(format!("http://{addr}/")), &test_client()).await;

    assert_eq!(cookies_at(&seen, "127.0.0.1/two"), ["v=1; keep=1"]);
    assert_eq!(cookies_at(&seen, "127.0.0.1/read"), ["keep=1"]);
}

#[tokio::test]
async fn executions_never_share_a_jar() {
    let issued = Arc::new(AtomicUsize::new(0));
    let seen = Seen::default();
    let app = Router::new().fallback({
        let (issued, log) = (Arc::clone(&issued), Arc::clone(&seen));
        move |uri: Uri, headers: HeaderMap| {
            let (issued, log) = (Arc::clone(&issued), Arc::clone(&log));
            async move {
                let cookies = headers
                    .get_all(COOKIE)
                    .iter()
                    .map(|v| v.to_str().unwrap().to_owned())
                    .collect();
                log.lock().unwrap().push((uri.path().to_owned(), cookies));
                if uri.path() == "/read" {
                    return "ok".into_response();
                }
                // Holds concurrent runs at one point, between set and send.
                tokio::time::sleep(Duration::from_millis(100)).await;
                let run = issued.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::FOUND,
                    [
                        (LOCATION, "/read".to_owned()),
                        (SET_COOKIE, format!("run={run}")),
                    ],
                )
                    .into_response()
            }
        }
    });
    let addr = spawn_router(app).await;
    let client = test_client();
    let check = following(format!("http://{addr}/"));

    run(&check, &client).await;
    run(&check, &client).await;
    tokio::join!(run(&check, &client), run(&check, &client));

    let seen = seen.lock().unwrap();
    let firsts: Vec<_> = seen.iter().filter(|(path, _)| path == "/").collect();
    assert_eq!(firsts.len(), 4);
    assert!(
        firsts.iter().all(|(_, cookies)| cookies.is_empty()),
        "{seen:?}"
    );
    let mut reads: Vec<Vec<String>> = seen
        .iter()
        .filter(|(path, _)| path == "/read")
        .map(|(_, cookies)| cookies.clone())
        .collect();
    reads.sort();
    assert_eq!(reads, [["run=0"], ["run=1"], ["run=2"], ["run=3"]]);
}

#[tokio::test]
async fn a_configured_cookie_merges_with_the_jar_into_one_header() {
    for name in ["Cookie", "cookie", "COOKIE"] {
        let (addr, seen) = spawn_cookie_site(vec![
            (
                "a.example.com/",
                redirect_to(
                    "/same",
                    &["jar=1", "shared=jar", "site=1; Domain=example.com"],
                ),
            ),
            (
                "a.example.com/same",
                redirect_to("http://www.example.com:{port}/off", &[]),
            ),
            ("www.example.com/off", landing()),
        ])
        .await;
        let client = client_resolving(&["a.example.com", "www.example.com"]).await;

        let mut check = following(format!("http://a.example.com:{}/", addr.port()));
        check
            .headers
            .insert(name.to_owned(), "user=1; shared=user".to_owned());
        run(&check, &client).await;

        assert_eq!(cookies_at(&seen, "a.example.com/"), ["user=1; shared=user"]);
        assert_eq!(
            cookies_at(&seen, "a.example.com/same"),
            ["user=1; shared=user; jar=1; site=1"],
            "{name}"
        );
        assert_eq!(
            cookies_at(&seen, "www.example.com/off"),
            ["site=1"],
            "{name}"
        );
    }
}

#[tokio::test]
async fn same_name_cookies_on_different_paths_are_both_sent() {
    let (addr, seen) = spawn_cookie_site(vec![
        (
            "127.0.0.1/",
            redirect_to("/a/b", &["n=root; Path=/", "n=deep; Path=/a"]),
        ),
        ("127.0.0.1/a/b", landing()),
    ])
    .await;

    run(&following(format!("http://{addr}/")), &test_client()).await;

    assert_eq!(cookies_at(&seen, "127.0.0.1/a/b"), ["n=deep; n=root"]);
}

#[tokio::test]
async fn cookies_over_a_cap_are_skipped_and_the_check_completes() {
    let oversized = format!("big={}", "x".repeat(4096));
    let many: Vec<String> = (0..60).map(|i| format!("c{i}=1")).collect();
    let mut set: Vec<&str> = vec![&oversized];
    set.extend(many.iter().map(String::as_str));
    let (addr, seen) = spawn_cookie_site(vec![
        ("127.0.0.1/", redirect_to("/read", &set)),
        ("127.0.0.1/read", landing()),
    ])
    .await;

    run(&following(format!("http://{addr}/")), &test_client()).await;

    let kept: Vec<String> = (0..50).map(|i| format!("c{i}=1")).collect();
    assert_eq!(cookies_at(&seen, "127.0.0.1/read"), [kept.join("; ")]);

    let wide: Vec<String> = (0..3)
        .map(|i| format!("w{i}={}", "x".repeat(3500)))
        .collect();
    let wide_refs: Vec<&str> = wide.iter().map(String::as_str).collect();
    let (addr, seen) = spawn_cookie_site(vec![
        ("127.0.0.1/", redirect_to("/read", &wide_refs)),
        ("127.0.0.1/read", landing()),
    ])
    .await;

    run(&following(format!("http://{addr}/")), &test_client()).await;

    assert_eq!(
        cookies_at(&seen, "127.0.0.1/read"),
        [format!("{}; {}", wide[0], wide[1])],
        "the cookie that does not fit is left out"
    );
}

#[tokio::test]
async fn a_configured_cookie_over_the_header_cap_goes_out_unchanged() {
    let (addr, seen) = spawn_cookie_site(vec![
        ("127.0.0.1/", redirect_to("/read", &["jar=1"])),
        ("127.0.0.1/read", landing()),
    ])
    .await;
    let configured = format!("user={}", "x".repeat(9000));

    let mut check = following(format!("http://{addr}/"));
    check
        .headers
        .insert("Cookie".to_owned(), configured.clone());
    run(&check, &test_client()).await;

    assert_eq!(cookies_at(&seen, "127.0.0.1/read"), [configured]);
}

#[tokio::test]
async fn without_follow_redirects_no_cookie_is_added() {
    let (addr, seen) = spawn_cookie_site(vec![
        ("127.0.0.1/", redirect_to("/read", &["jar=1"])),
        ("127.0.0.1/read", landing()),
    ])
    .await;

    let mut check = default_http_check(
        Url::parse(&format!("http://{addr}/")).unwrap(),
        ExpectedStatus::Exact(200),
    );
    check
        .headers
        .insert("Cookie".to_owned(), "user=1".to_owned());
    let result = execute_http_check(Uuid::now_v7(), Uuid::nil(), &check, &test_client()).await;

    assert_eq!(result.error.as_deref(), Some("unexpected status 302"));
    assert_eq!(
        *seen.lock().unwrap(),
        [("127.0.0.1/".to_owned(), vec!["user=1".to_owned()])]
    );
}

#[tokio::test]
async fn configured_cookie_rows_go_out_as_one_header_without_any_redirect() {
    for (rows, sent) in [
        (
            &[("Cookie", "a=1;"), ("cookie", "b=2")][..],
            &["a=1; b=2"][..],
        ),
        (&[("Cookie", "a=1; "), ("cookie", ";")][..], &["a=1"][..]),
        (&[("Cookie", " ; ")][..], &[][..]),
    ] {
        let (addr, seen) = spawn_cookie_site(vec![("127.0.0.1/", landing())]).await;
        let mut check = default_http_check(
            Url::parse(&format!("http://{addr}/")).unwrap(),
            ExpectedStatus::Exact(200),
        );
        for (name, value) in rows {
            check
                .headers
                .insert((*name).to_owned(), (*value).to_owned());
        }
        run(&check, &test_client()).await;

        assert_eq!(cookies_at(&seen, "127.0.0.1/"), sent);
    }
}

#[tokio::test]
async fn a_chain_that_sets_no_cookie_sends_the_configured_cookie_as_before() {
    let (addr, seen) = spawn_cookie_site(vec![
        ("a.example.com/", redirect_to("/same", &[])),
        (
            "a.example.com/same",
            redirect_to("http://other.test:{port}/off", &[]),
        ),
        ("other.test/off", landing()),
    ])
    .await;
    let client = client_resolving(&["a.example.com", "other.test"]).await;

    let mut check = following(format!("http://a.example.com:{}/", addr.port()));
    check
        .headers
        .insert("Cookie".to_owned(), "u=1;v=2".to_owned());
    run(&check, &client).await;

    assert_eq!(cookies_at(&seen, "a.example.com/"), ["u=1;v=2"]);
    assert_eq!(cookies_at(&seen, "a.example.com/same"), ["u=1;v=2"]);
    assert!(cookies_at(&seen, "other.test/off").is_empty());
}

/// Regression for the tenant-isolation write bug: `worker::execute` (and the
/// per-protocol check fns it dispatches to) must stamp the *passed* org_id
/// onto the produced `CheckResult`. The live-CH `tenant_isolation_test` also
/// covers this but is `#[ignore]`d — this is the fast, CI-visible guard. A
/// distinct non-nil org is used so it can't pass by coincidence with a
/// defaulted/zeroed field.
#[tokio::test]
async fn execute_stamps_passed_org_id_on_result() {
    let app = Router::new().route("/health", get(|| async { "pong" }));
    let addr = spawn_router(app).await;

    let client = test_client();
    let url = Url::parse(&format!("http://{addr}/health")).unwrap();
    let check = default_http_check(url, ExpectedStatus::Exact(200));
    let spec = uptimepage::domain::CheckSpec::Http(check);

    let target_id = Uuid::now_v7();
    let org_id = Uuid::now_v7();
    let domain_expiry = common::test_domain_expiry_runtime();
    let deps = uptimepage::worker::WorkerDeps {
        http: &client,
        domain_expiry: &domain_expiry,
        flow: None,
    };
    let result = uptimepage::worker::execute(target_id, org_id, &spec, &deps).await;

    assert_eq!(result.status, CheckStatus::Up);
    assert_eq!(
        result.org_id, org_id,
        "worker::execute must thread the passed org_id onto the CheckResult"
    );
    assert_eq!(result.target_id, target_id);
}
