//! Parameterised integration tests that run the same scenarios in both
//! path-based and subdomain public-routing modes. Catches regressions where
//! a handler implicitly relies on one mode and breaks the other.

use crate::common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{TenancyMode, build_test_app, build_test_app_with_web};
use rstest::rstest;
use tower::ServiceExt;

async fn status(app: axum::Router, path: &str) -> StatusCode {
    let resp = app
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .expect("oneshot");
    resp.status()
}

/// Targets list is a tenant-scoped endpoint. An anonymous request must always
/// be rejected with 401 — the store is org-scoped, the binary is SaaS-only,
/// and the public-routing mode (path vs subdomain) has nothing to do with the
/// operator auth model.
#[rstest]
#[case::path_based(TenancyMode::PathBased)]
#[case::subdomain(TenancyMode::Subdomain)]
#[tokio::test]
async fn list_targets_rejects_anonymous_in_both_modes(#[case] mode: TenancyMode) {
    let app = build_test_app(|cfg| mode.apply(cfg));
    assert_eq!(
        status(app, "/api/v1/targets").await,
        StatusCode::UNAUTHORIZED
    );
}

/// Dashboard summary is mounted in both modes. Anonymous → 401.
#[rstest]
#[case::path_based(TenancyMode::PathBased)]
#[case::subdomain(TenancyMode::Subdomain)]
#[tokio::test]
async fn dashboard_summary_rejects_anonymous_in_both_modes(#[case] mode: TenancyMode) {
    let app = build_test_app(|cfg| mode.apply(cfg));
    assert_eq!(
        status(app, "/api/v1/dashboard/summary").await,
        StatusCode::UNAUTHORIZED
    );
}

/// Health/readiness probes are tenancy-agnostic. They must respond identically
/// regardless of public-routing mode.
#[rstest]
#[case::path_based(TenancyMode::PathBased)]
#[case::subdomain(TenancyMode::Subdomain)]
#[tokio::test]
async fn health_endpoints_respond_in_both_modes(#[case] mode: TenancyMode) {
    let app = build_test_app_with_web(|cfg| mode.apply(cfg));
    assert_eq!(status(app.clone(), "/healthz").await, StatusCode::OK);
    // /readyz is OK or 503 depending on backend state; only assert wired.
    let s = status(app, "/readyz").await;
    assert_ne!(s, StatusCode::NOT_FOUND, "readyz must be mounted");
}
