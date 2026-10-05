//! Coverage for acceptance criteria not already exercised by the other
//! integration tests in this directory.

use crate::common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::build_test_app_with_owner;
use serde_json::{Value, json};
use tower::ServiceExt;

fn app() -> axum::Router {
    build_test_app_with_owner(|_| {})
}

async fn body_json(resp: axum::http::Response<Body>) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 8 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn json_responses_include_utf8_charset() {
    let resp = app()
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(ct, "application/json; charset=utf-8");
}

#[tokio::test]
async fn empty_tags_endpoint_returns_empty_envelope() {
    let resp = app()
        .oneshot(Request::get("/api/v1/tags").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["items"].as_array().unwrap().len(), 0);
    assert_eq!(v["has_more"], false);
}

#[tokio::test]
async fn idempotency_key_replays_bulk_action() {
    let app = app();

    // Two targets to operate on
    let payload = json!({
        "name": "idem-a",
        "check": {
            "type": "http",
            "url": "http://example.com",
            "method": "GET",
            "timeout": 5000,
            "follow_redirects": false,
            "max_redirects": 0,
            "expected_status": { "kind": "exact", "value": 200 },
            "headers": {},
            "verify_tls": true
        },
        "interval": 60
    });
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v1/targets")
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let id = body_json(resp).await["id"].as_str().unwrap().to_string();

    let action = json!({"ids": [id], "action": {"type": "disable"}}).to_string();

    // First call: original execution
    let r1 = app
        .clone()
        .oneshot(
            Request::post("/api/v1/targets/bulk-action")
                .header("content-type", "application/json")
                .header("idempotency-key", "client-key-1")
                .body(Body::from(action.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    let body1 = body_json(r1).await;

    // Second call with same key + body: replay (response body identical)
    let r2 = app
        .clone()
        .oneshot(
            Request::post("/api/v1/targets/bulk-action")
                .header("content-type", "application/json")
                .header("idempotency-key", "client-key-1")
                .body(Body::from(action.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    let body2 = body_json(r2).await;
    assert_eq!(body1, body2);

    // Different key: fresh execution, but result body shape still valid.
    let r3 = app
        .oneshot(
            Request::post("/api/v1/targets/bulk-action")
                .header("content-type", "application/json")
                .header("idempotency-key", "client-key-2")
                .body(Body::from(action))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r3.status(), StatusCode::OK);
}

#[tokio::test]
async fn openapi_json_carries_charset_suffix() {
    let resp = app()
        .oneshot(
            Request::get("/api/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(ct, "application/json; charset=utf-8");
}

#[tokio::test]
async fn post_target_returns_location_header() {
    let app = app();
    let payload = json!({
        "name": "loc-test",
        "check": {
            "type": "http",
            "url": "http://example.com",
            "method": "GET",
            "timeout": 5000,
            "follow_redirects": false,
            "max_redirects": 0,
            "expected_status": { "kind": "exact", "value": 200 },
            "headers": {},
            "verify_tls": true
        },
        "interval": 60
    });
    let resp = app
        .oneshot(
            Request::post("/api/v1/targets")
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .expect("Location header on 201")
        .to_string();
    let cache_control = resp
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let v = body_json(resp).await;
    let id = v["id"].as_str().unwrap();
    assert_eq!(location, format!("/api/v1/targets/{id}"));
    assert_eq!(cache_control, "no-store");
}

#[tokio::test]
async fn cache_control_set_per_route_kind() {
    let app = app();

    let dash = app
        .clone()
        .oneshot(
            Request::get("/api/v1/dashboard/summary")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(dash.status(), StatusCode::OK);
    assert_eq!(
        dash.headers().get("cache-control").unwrap(),
        "private, max-age=5"
    );

    let list = app
        .clone()
        .oneshot(Request::get("/api/v1/targets").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::OK);
    assert_eq!(
        list.headers().get("cache-control").unwrap(),
        "private, max-age=10"
    );

    let health = app
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert!(
        health.headers().get("cache-control").is_none(),
        "health endpoints should not pin Cache-Control"
    );
}
