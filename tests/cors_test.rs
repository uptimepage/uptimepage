use crate::common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::build_test_app;
use tower::ServiceExt;
use uptimepage::config::CorsConfig;

fn cors_cfg(origins: Vec<&str>, allow_any_origin: bool) -> CorsConfig {
    CorsConfig {
        enabled: true,
        allowed_origins: origins.into_iter().map(String::from).collect(),
        allowed_methods: vec!["GET".into(), "POST".into()],
        allow_any_origin,
    }
}

fn preflight(origin: &str, method: &str) -> Request<Body> {
    Request::builder()
        .method("OPTIONS")
        .uri("/api/v1/targets")
        .header(header::ORIGIN, origin)
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, method)
        .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "content-type")
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn cors_disabled_omits_acao_header() {
    let app = build_test_app(|cfg| cfg.api.cors = CorsConfig::default());
    let resp = app
        .oneshot(preflight("https://app.example.com", "POST"))
        .await
        .unwrap();
    assert!(
        resp.headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none()
    );
}

#[tokio::test]
async fn cors_enabled_returns_configured_origin() {
    let app = build_test_app(|cfg| {
        cfg.api.cors = cors_cfg(vec!["https://app.example.com"], false);
    });
    let resp = app
        .oneshot(preflight("https://app.example.com", "POST"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .unwrap(),
        "https://app.example.com"
    );
}

#[tokio::test]
async fn cors_rejects_unlisted_origin() {
    let app = build_test_app(|cfg| {
        cfg.api.cors = cors_cfg(vec!["https://app.example.com"], false);
    });
    let resp = app
        .oneshot(preflight("https://evil.example.com", "POST"))
        .await
        .unwrap();
    assert!(
        resp.headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none()
    );
}

#[tokio::test]
async fn cors_allow_any_origin_emits_wildcard() {
    let app = build_test_app(|cfg| cfg.api.cors = cors_cfg(vec![], true));
    let resp = app
        .oneshot(preflight("https://anywhere.example", "POST"))
        .await
        .unwrap();
    assert_eq!(
        resp.headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .unwrap(),
        "*"
    );
}

#[tokio::test]
#[should_panic(expected = "contains '*'")]
async fn cors_wildcard_in_allowed_origins_fails_fast() {
    let _ = build_test_app(|cfg| cfg.api.cors = cors_cfg(vec!["*"], false));
}

#[tokio::test]
#[should_panic(expected = "requires allowed_origins or allow_any_origin")]
async fn cors_enabled_without_origins_fails_fast() {
    let _ = build_test_app(|cfg| cfg.api.cors = cors_cfg(vec![], false));
}

#[tokio::test]
#[should_panic(expected = "mutually exclusive")]
async fn cors_any_origin_with_list_fails_fast() {
    let _ = build_test_app(|cfg| {
        cfg.api.cors = cors_cfg(vec!["https://app.example.com"], true);
    });
}
