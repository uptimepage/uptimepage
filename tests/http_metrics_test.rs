//! Structural lock-in for the HTTP-metrics middleware. The middleware
//! doesn't mutate the response, so silently dropping it from
//! `apply_cross_cutting_layers` would not fail any behaviour-level test
//! — this one asserts the metric series actually fires AND the
//! cardinality-control branches (MatchedPath, health suppression,
//! status bucketing, method clamp, inflight Drop) all hold.
//!
//! Assertions live in one `#[tokio::test]` so they run sequentially in
//! a single process — `metrics::set_global_recorder` is one-shot per
//! process and parallel bodies would race on the gauge baseline.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{build_test_app, metric_value, metrics_handle};
use tower::ServiceExt;

fn req(method: &str, path: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .body(Body::empty())
        .unwrap()
}

fn gauge_value(rendered: &str, name: &str) -> f64 {
    metric_value(rendered, name)
        .unwrap_or_else(|| panic!("gauge {name} missing from render:\n{rendered}"))
}

#[tokio::test]
async fn middleware_pins_all_contracts() {
    let h = metrics_handle();
    let app = build_test_app(|_| {});

    let _ = app
        .clone()
        .oneshot(req("GET", "/api/v1/targets"))
        .await
        .unwrap();

    let rendered = h.render();
    assert!(
        rendered.contains("uptimepage_http_requests_total"),
        "counter series missing — middleware did not fire:\n{rendered}"
    );
    assert!(
        rendered.contains("route=\"/api/v1/targets\""),
        "MatchedPath route label missing on /api/v1/targets:\n{rendered}"
    );

    // Placeholder route: label must be the template, never the concrete
    // UUID — otherwise the series count grows with target_id at 100k+.
    let _ = app
        .clone()
        .oneshot(req(
            "GET",
            "/api/v1/targets/aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa/uptime",
        ))
        .await
        .unwrap();
    let rendered = h.render();
    assert!(
        rendered.contains("/api/v1/targets/{id}/uptime"),
        "placeholder route label missing — concrete UUIDs likely leaking into series:\n{rendered}"
    );
    assert!(
        !rendered.contains("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
        "concrete UUID leaked into a metric label — cardinality footgun:\n{rendered}"
    );

    let resp = app.clone().oneshot(req("GET", "/healthz")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let rendered = h.render();
    assert!(
        !rendered.contains("route=\"/healthz\""),
        "/healthz appeared in http_requests_total — suppression branch broke:\n{rendered}"
    );
    let _ = app.clone().oneshot(req("GET", "/readyz")).await.unwrap();
    let rendered = h.render();
    assert!(
        !rendered.contains("route=\"/readyz\""),
        "/readyz appeared in http_requests_total — readiness probe not suppressed:\n{rendered}"
    );

    // Status: bucketed by class, not literal. /api/v1/targets 401s
    // unauthenticated — must record as 4xx, never 401.
    assert!(
        rendered.contains("status=\"4xx\""),
        "status_class not bucketing 4xx — likely emitting raw code, unbounded cardinality:\n{rendered}"
    );
    assert!(
        !rendered.contains("status=\"401\""),
        "literal status code 401 leaked into label — bucketing broke:\n{rendered}"
    );

    // Method clamp: a sprayed `Method::from_bytes(b"AAAA…")` must not
    // open a new label series.
    let _ = app
        .clone()
        .oneshot(req("PROPFIND", "/api/v1/targets"))
        .await
        .unwrap();
    let rendered = h.render();
    assert!(
        rendered.contains("method=\"other\""),
        "PROPFIND request did not bucket under method=other — method_label clamp broken:\n{rendered}"
    );
    assert!(
        !rendered.contains("method=\"PROPFIND\""),
        "PROPFIND escaped the clamp — cardinality vector reopened:\n{rendered}"
    );

    let rendered = h.render();
    let inflight = gauge_value(&rendered, "uptimepage_http_responses_inflight");
    assert_eq!(
        inflight, 0.0,
        "inflight gauge leaked above baseline after sequential requests: {inflight}"
    );
}
