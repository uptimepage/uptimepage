//! The dispatch seam routes requests to the marketing or app router by
//! classified `Host`. These tests build the dispatch directly with
//! sentinel mini-routers so the assertion is on the routing decision,
//! not on any app-side handler.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::util::ServiceExt;

use uptimepage::marketing::RouteByHost;
use uptimepage::request::host::HostScheme;

fn sentinel(name: &'static str) -> Router {
    Router::new().fallback(move || async move { name })
}

fn dispatch() -> RouteByHost {
    RouteByHost {
        scheme: HostScheme::from_base_domain("example.com").unwrap(),
        marketing: sentinel("marketing"),
        app: sentinel("app"),
    }
}

async fn body_for(host: &str) -> (StatusCode, String) {
    body_for_path("/", host).await
}

async fn body_for_path(path: &str, host: &str) -> (StatusCode, String) {
    let resp = dispatch()
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
async fn unknown_host_goes_to_marketing_404() {
    // Garbage hosts must NOT fall through to a tenant; the dispatcher
    // routes them to marketing so the response is a branded 404 served
    // from the apex surface.
    let (_, b) = body_for("totally.unrelated.example").await;
    assert_eq!(b, "marketing");
}

#[tokio::test]
async fn empty_host_goes_to_marketing() {
    let (_, b) = body_for("").await;
    assert_eq!(b, "marketing");
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
async fn non_health_path_on_unknown_host_still_goes_to_marketing() {
    // The bypass is path-scoped — other paths on an Unknown host must
    // still hand off to the marketing 404, not the app surface.
    let (_, b) = body_for_path("/", "uptimepage:8080").await;
    assert_eq!(b, "marketing");
}
