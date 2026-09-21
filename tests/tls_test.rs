mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::Version;
use axum::routing::get;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use uptimepage::domain::{CheckResult, CheckStatus, ExpectedStatus};
use uptimepage::http_client::H2_MAX_HEADER_LIST_SIZE;
use uptimepage::worker::execute_http_check;
use url::Url;
use uuid::Uuid;

use crate::common::{
    PRELOAD_HINT, default_http_check, link_header, router_with, spawn_self_signed_tls_router,
    spawn_truncated_chain_tls_router, test_client,
};

fn router() -> Router {
    Router::new().route("/", get(|| async { "ok" }))
}

#[tokio::test]
async fn verify_tls_false_accepts_self_signed() {
    let addr = spawn_self_signed_tls_router(router()).await;

    let result = probe(addr, false).await;

    assert_up(&result);
    // Per-phase timings populated for the breakdown chart: an HTTPS check
    // records a TLS-handshake phase (the bug in #31 left these always None).
    assert!(result.connect_ms.is_some(), "connect phase must be timed");
    assert!(result.tls_ms.is_some(), "tls handshake phase must be timed");
    assert!(result.ttfb_ms.is_some(), "ttfb must be timed");
}

#[tokio::test]
async fn verify_tls_true_rejects_self_signed() {
    let addr = spawn_self_signed_tls_router(router()).await;

    let result = probe(addr, true).await;

    assert_eq!(result.status, CheckStatus::Error);
    let err = result.error.expect("error message");
    // webpki calls this an unknown issuer either way; the probe reads the leaf
    // back to say which shape it is.
    assert_eq!(
        err, "certificate self-signed",
        "expected self-signed reason, got {err}"
    );
}

/// A leaf installed without its intermediate. Browsers hide this by fetching
/// the missing certificate over AIA, so the operator sees a padlock.
#[tokio::test]
async fn verify_tls_true_names_a_truncated_chain() {
    let addr = spawn_truncated_chain_tls_router(router()).await;
    let clients = test_client();
    let url = Url::parse(&format!("https://localhost:{}/", addr.port())).unwrap();
    let mut check = default_http_check(url, ExpectedStatus::Exact(200));
    check.verify_tls = true;

    let result = execute_http_check(Uuid::now_v7(), uuid::Uuid::nil(), &check, &clients).await;

    assert_eq!(result.status, CheckStatus::Error);
    let err = result.error.expect("error message");
    assert_eq!(
        err, "certificate chain incomplete",
        "expected truncated-chain reason, got {err}"
    );
}

/// Completes the TLS handshake, waits for the request bytes, then hands the
/// stream to `then`: what an edge does when it drops a request it dislikes.
async fn spawn_tls_server_that<F, Fut>(then: F) -> SocketAddr
where
    F: FnOnce(TlsStream<TcpStream>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).expect("gen cert");
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()));
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert.cert.der().clone()], key)
        .expect("server config");
    let acceptor = TlsAcceptor::from(Arc::new(config));

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.expect("accept");
        let mut tls = acceptor.accept(tcp).await.expect("tls accept");
        let mut buf = [0u8; 1024];
        let n = tls.read(&mut buf).await.expect("request head");
        assert!(n > 0, "client closed before sending the request");
        then(tls).await;
    });
    addr
}

async fn probe(addr: SocketAddr, verify_tls: bool) -> CheckResult {
    let clients = test_client();
    let url = Url::parse(&format!("https://localhost:{}/", addr.port())).unwrap();
    let mut check = default_http_check(url, ExpectedStatus::Exact(200));
    check.verify_tls = verify_tls;
    execute_http_check(Uuid::now_v7(), Uuid::nil(), &check, &clients).await
}

async fn probe_error(addr: SocketAddr) -> CheckResult {
    let result = probe(addr, false).await;
    assert_eq!(result.status, CheckStatus::Error);
    assert!(
        result.tls_ms.is_some(),
        "handshake completed, so it is timed"
    );
    assert!(result.ttfb_ms.is_none(), "no first byte ever arrived");
    result
}

#[tokio::test]
async fn reset_after_the_request_names_the_reset() {
    let addr = spawn_tls_server_that(|tls| async move {
        // Zero linger turns the close into an RST instead of a FIN.
        let (tcp, _) = tls.into_inner();
        socket2::SockRef::from(&tcp)
            .set_linger(Some(Duration::ZERO))
            .expect("linger");
        drop(tcp);
    })
    .await;

    let result = probe_error(addr).await;

    assert_eq!(result.error.as_deref(), Some("reset before response"));
}

#[tokio::test]
async fn close_before_headers_names_the_close() {
    let addr = spawn_tls_server_that(|mut tls| async move {
        tls.shutdown().await.expect("close_notify");
    })
    .await;

    let result = probe_error(addr).await;

    assert_eq!(result.error.as_deref(), Some("closed before response"));
}

fn assert_up(result: &CheckResult) {
    assert_eq!(
        result.status,
        CheckStatus::Up,
        "expected Up, got {:?} (error: {:?})",
        result.status,
        result.error
    );
    assert_eq!(result.response_code, Some(200));
}

#[tokio::test]
async fn h2_response_with_large_headers_is_up() {
    let headers = link_header(40 << 10);
    let addr = spawn_self_signed_tls_router(router_with(headers, Version::HTTP_2)).await;

    assert_up(&probe(addr, false).await);
}

#[tokio::test]
async fn h2_response_over_the_header_limit_names_the_probe() {
    // Just past the cap: further out h2 caps CONTINUATION frames instead.
    let headers = link_header(H2_MAX_HEADER_LIST_SIZE as usize + PRELOAD_HINT.len());
    let addr = spawn_self_signed_tls_router(router_with(headers, Version::HTTP_2)).await;

    let result = probe(addr, false).await;

    assert_eq!(result.status, CheckStatus::Error);
    assert_eq!(
        result.error.as_deref(),
        Some("h2 rejected locally: PROTOCOL_ERROR")
    );
}
