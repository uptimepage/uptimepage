//! The dispatch seam routes requests to the marketing or app router by
//! classified `Host`. These tests build the dispatch directly with
//! sentinel mini-routers so the assertion is on the routing decision,
//! not on any app-side handler.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::util::ServiceExt;

use uptimepage::domain::{OrgId, PageRef, StatusPageId};
use uptimepage::marketing::RouteByHost;
use uptimepage::request::custom_domains::{CustomDomainRow, CustomDomains};
use uptimepage::request::host::HostScheme;

fn sentinel(name: &'static str) -> Router {
    Router::new().fallback(move || async move { name })
}

fn dispatch() -> RouteByHost {
    dispatch_serving(&[])
}

fn dispatch_serving(domains: &[&str]) -> RouteByHost {
    let custom_domains = CustomDomains::new("example.com");
    custom_domains.install(
        domains
            .iter()
            .enumerate()
            .map(|(i, d)| CustomDomainRow {
                domain: (*d).into(),
                page: PageRef {
                    page: StatusPageId(uuid::Uuid::from_u128(i as u128 + 1)),
                    org: OrgId(uuid::Uuid::from_u128(i as u128 + 1000)),
                },
                slug: format!("page{i}"),
                activated: true,
            })
            .collect(),
    );
    RouteByHost {
        scheme: HostScheme::from_base_domain("example.com").unwrap(),
        custom_domains: Arc::new(custom_domains),
        marketing: sentinel("marketing"),
        app: sentinel("app"),
    }
}

async fn body_for(host: &str) -> (StatusCode, String) {
    body_for_path("/", host).await
}

async fn body_for_path(path: &str, host: &str) -> (StatusCode, String) {
    body_for_path_on(dispatch(), path, host).await
}

async fn body_for_path_on(dispatch: RouteByHost, path: &str, host: &str) -> (StatusCode, String) {
    let resp = dispatch
        .oneshot(
            Request::builder()
                .uri(path)
                .header(header::HOST, host)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("dispatch call");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        String::from_utf8(bytes.to_vec()).unwrap_or_default(),
    )
}

#[tokio::test]
async fn apex_goes_to_marketing() {
    let (s, b) = body_for("example.com").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b, "marketing");
}

#[tokio::test]
async fn www_goes_to_marketing() {
    let (_, b) = body_for("www.example.com").await;
    assert_eq!(b, "marketing");
}

#[tokio::test]
async fn app_goes_to_app() {
    let (_, b) = body_for("app.example.com").await;
    assert_eq!(b, "app");
}

#[tokio::test]
async fn tenant_slug_goes_to_app() {
    // The app router owns per-tenant routing internally via the
    // `StatusPageOrg` extractor — the dispatcher must hand off the
    // request without rewriting paths.
    let (_, b) = body_for("acme.example.com").await;
    assert_eq!(b, "app");
}

#[tokio::test]
async fn unknown_host_is_404ed_at_the_seam() {
    let (s, b) = body_for("totally.unrelated.example").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_ne!(b, "marketing");
    assert_ne!(b, "app");
}

#[tokio::test]
async fn empty_host_is_404ed_at_the_seam() {
    let (s, _) = body_for("").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_served_custom_domain_goes_to_app() {
    let d = dispatch_serving(&["status.acme.test"]);
    let (s, b) = body_for_path_on(d, "/", "status.acme.test").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b, "app");
}

#[tokio::test]
async fn a_custom_domain_the_snapshot_dropped_is_404ed() {
    let d = dispatch_serving(&["status.acme.test"]);
    let (s, b) = body_for_path_on(d, "/", "status.former.test").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_ne!(b, "app");
}

#[tokio::test]
async fn host_with_port_classifies_correctly() {
    let (_, b) = body_for("example.com:8080").await;
    assert_eq!(b, "marketing");
    let (_, b) = body_for("app.example.com:443").await;
    assert_eq!(b, "app");
}

#[tokio::test]
async fn healthz_bypasses_classification() {
    // `uptimepage:8080` classifies as Unknown — without the bypass
    // Caddy's active health check would land on the marketing router
    // (404) and mark the upstream down.
    let (_, b) = body_for_path("/healthz", "uptimepage:8080").await;
    assert_eq!(b, "app");
}

#[tokio::test]
async fn readyz_bypasses_classification() {
    let (_, b) = body_for_path("/readyz", "1.2.3.4").await;
    assert_eq!(b, "app");
}

#[tokio::test]
async fn non_health_path_on_unknown_host_is_404ed() {
    // The bypass is path-scoped — other paths on an Unknown host reach
    // neither router.
    let (s, b) = body_for_path("/", "uptimepage:8080").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_ne!(b, "app");
    assert_ne!(b, "marketing");
}
