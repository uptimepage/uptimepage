mod common;

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::{StatusCode, Version, header};
use axum::routing::get;
use uptimepage::domain::{
    CheckDiagnosticKind, CheckStatus, DiagnosticConfidence, DiagnosticEvidence,
    DiagnosticRemediation, EdgeProvider, ExpectedStatus, HttpMethod,
};
use uptimepage::http_client::H1_MAX_HEADERS;
use uptimepage::storage::{InMemorySink, ResultSink};
use uptimepage::worker::execute_http_check;
use url::Url;
use uuid::Uuid;

use crate::common::{
    default_http_check, link_lines, router_with, spawn_router, test_client,
    test_client_with_failing_dns,
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
    let app = Router::new().route("/loop", get(|| async { moved(StatusCode::FOUND, "/loop") }));
    let addr = spawn_router(app).await;

    let client = test_client();
    let url = Url::parse(&format!("http://{addr}/loop")).unwrap();
    let mut check = default_http_check(url, ExpectedStatus::Exact(200));
    check.follow_redirects = true;
    check.max_redirects = 2;

    let result = execute_http_check(Uuid::now_v7(), uuid::Uuid::nil(), &check, &client).await;

    assert_eq!(result.status, CheckStatus::Error);
    assert_eq!(result.error.as_deref(), Some("too many redirects"));
    assert_eq!(
        result.response_code,
        Some(302),
        "the 3xx that exceeded the cap must be preserved for diagnostics",
    );
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
    use axum::http::HeaderMap;

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
