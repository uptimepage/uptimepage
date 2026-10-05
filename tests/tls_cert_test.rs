use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use rcgen::{CertificateParams, DnType, KeyPair, SanType};
use time::OffsetDateTime;
use uptimepage::domain::{CheckStatus, TlsCertCheck};
use uptimepage::worker::tls_cert::execute_tls_cert_check;
use uuid::Uuid;

use crate::common::test_client;

async fn spawn_tls_server_with_validity(days_remaining: i64) -> SocketAddr {
    use axum_server::tls_rustls::RustlsConfig;

    let _ = rustls::crypto::ring::default_provider().install_default();

    let key = KeyPair::generate().expect("keypair");
    let mut params = CertificateParams::default();
    params.subject_alt_names = vec![SanType::DnsName("localhost".try_into().unwrap())];
    params
        .distinguished_name
        .push(DnType::CommonName, "localhost");
    let now = OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::days(1);
    params.not_after = now + time::Duration::days(days_remaining);
    let cert = params.self_signed(&key).expect("self-signed cert");

    let cfg = RustlsConfig::from_pem(cert.pem().into_bytes(), key.serialize_pem().into_bytes())
        .await
        .expect("rustls config");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let addr = listener.local_addr().expect("local_addr");

    let router = Router::new().route("/", get(|| async { "ok" }));
    tokio::spawn(async move {
        axum_server::from_tcp_rustls(listener, cfg)
            .expect("rustls server")
            .serve(router.into_make_service())
            .await
            .expect("serve");
    });
    addr
}

fn make_check(addr: SocketAddr, warn: u32, critical: u32) -> TlsCertCheck {
    TlsCertCheck {
        host: "localhost".into(),
        port: addr.port(),
        server_name: None,
        warn_days: warn,
        critical_days: critical,
        timeout: Duration::from_secs(5),
    }
}

/// The shared probe read by both the monitor and the public SSL tool. Asserted
/// directly because the monitor only ever surfaces four of its fields, and the
/// tool renders the rest.
#[tokio::test]
async fn cert_probe_reads_the_leaf_a_server_presents() {
    let addr = spawn_tls_server_with_validity(45).await;
    let facts = uptimepage::security::cert_probe::connect_and_read(
        &[addr.ip()],
        addr.port(),
        "localhost",
        Duration::from_secs(5),
    )
    .await
    .expect("probe reads the certificate");

    assert_eq!(facts.san_dns_names, vec!["localhost".to_string()]);
    assert!(facts.name_matches, "the SAN covers the name we asked for");
    assert!(facts.self_signed, "the test server signs its own leaf");
    assert_eq!(facts.chain_len, 1);
    assert_eq!(facts.subject_common_name.as_deref(), Some("localhost"));
    assert!(
        (44..=45).contains(&facts.days_remaining),
        "expected ~45 days, got {}",
        facts.days_remaining
    );
    assert!(facts.not_before < facts.not_after);
}

/// A name the certificate does not carry must come back as a mismatch rather
/// than as a handshake failure: reading the wrong certificate is the answer.
#[tokio::test]
async fn cert_probe_reports_a_name_the_certificate_does_not_cover() {
    let addr = spawn_tls_server_with_validity(30).await;
    let facts = uptimepage::security::cert_probe::connect_and_read(
        &[addr.ip()],
        addr.port(),
        "not-the-name.example",
        Duration::from_secs(5),
    )
    .await
    .expect("probe still reads the certificate");

    assert!(!facts.name_matches);
    assert_eq!(facts.san_dns_names, vec!["localhost".to_string()]);
}

#[tokio::test]
async fn tls_cert_check_returns_up_for_valid_cert() {
    let addr = spawn_tls_server_with_validity(60).await;
    let result = execute_tls_cert_check(
        Uuid::now_v7(),
        uuid::Uuid::nil(),
        &make_check(addr, 14, 7),
        &test_client(),
    )
    .await;
    assert_eq!(result.status, CheckStatus::Up, "error={:?}", result.error);
    // Up results carry no diagnostic body — matches the convention used by
    // tcp_check / http_check.
    assert!(result.error.is_none());
}

#[tokio::test]
async fn tls_cert_check_returns_degraded_under_warn_days() {
    let addr = spawn_tls_server_with_validity(10).await;
    let result = execute_tls_cert_check(
        Uuid::now_v7(),
        uuid::Uuid::nil(),
        &make_check(addr, 14, 7),
        &test_client(),
    )
    .await;
    assert_eq!(result.status, CheckStatus::Degraded);
    let body: serde_json::Value = serde_json::from_str(result.error.as_deref().unwrap()).unwrap();
    assert!(body["days_remaining"].as_i64().unwrap() < 14);
    assert!(
        body["subject_common_name"]
            .as_str()
            .unwrap()
            .contains("localhost")
    );
}

#[tokio::test]
async fn tls_cert_check_returns_down_under_critical_days() {
    let addr = spawn_tls_server_with_validity(5).await;
    let result = execute_tls_cert_check(
        Uuid::now_v7(),
        uuid::Uuid::nil(),
        &make_check(addr, 14, 7),
        &test_client(),
    )
    .await;
    assert_eq!(result.status, CheckStatus::Down);
}

#[tokio::test]
async fn tls_cert_check_returns_down_when_expired() {
    let addr = spawn_tls_server_with_validity(-1).await;
    let result = execute_tls_cert_check(
        Uuid::now_v7(),
        uuid::Uuid::nil(),
        &make_check(addr, 14, 7),
        &test_client(),
    )
    .await;
    assert_eq!(result.status, CheckStatus::Down);
    let body: serde_json::Value = serde_json::from_str(result.error.as_deref().unwrap()).unwrap();
    assert!(body["days_remaining"].as_i64().unwrap() < 0);
}

#[tokio::test]
async fn tls_cert_check_handshake_failure_is_error() {
    // Bind a plain TCP listener that accepts but speaks no TLS.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let _ = listener.accept().await;
        }
    });
    let result = execute_tls_cert_check(
        Uuid::now_v7(),
        uuid::Uuid::nil(),
        &make_check(addr, 14, 7),
        &test_client(),
    )
    .await;
    assert_eq!(result.status, CheckStatus::Error);
}
