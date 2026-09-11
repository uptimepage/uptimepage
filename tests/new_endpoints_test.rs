mod common;

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

fn http_target_payload(name: &str) -> Value {
    json!({
        "name": name,
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
        "interval": 60,
        "tags": ["prod"]
    })
}

async fn create_target(app: &axum::Router, name: &str) -> String {
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v1/targets")
                .header("content-type", "application/json")
                .body(Body::from(http_target_payload(name).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let v = body_json(resp).await;
    v["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn bulk_action_disable_reports_partial_success() {
    let app = app();
    let id_a = create_target(&app, "a").await;
    let id_b = create_target(&app, "b").await;
    let missing = uuid::Uuid::now_v7().to_string();

    let payload = json!({
        "ids": [id_a, id_b, missing],
        "action": { "type": "disable" }
    });
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v1/targets/bulk-action")
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["succeeded"].as_array().unwrap().len(), 2);
    assert_eq!(v["failed"].as_array().unwrap().len(), 1);
    assert_eq!(v["failed"][0]["code"], "TARGET_NOT_FOUND");
    assert_eq!(v["failed"][0]["id"], missing);
}

#[tokio::test]
async fn bulk_action_rejects_empty_ids() {
    let payload = json!({"ids": [], "action": {"type": "delete"}});
    let resp = app()
        .oneshot(
            Request::post("/api/v1/targets/bulk-action")
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = body_json(resp).await;
    assert_eq!(v["error"]["code"], "BULK_EMPTY");
}

#[tokio::test]
async fn bulk_action_tag_add_then_remove() {
    let app = app();
    let id = create_target(&app, "tag-test").await;

    let add = json!({
        "ids": [id],
        "action": { "type": "tag_add", "tags": ["fresh"] }
    });
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v1/targets/bulk-action")
                .header("content-type", "application/json")
                .body(Body::from(add.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/targets/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let v = body_json(resp).await;
    let tags: Vec<&str> = v["tags"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t.as_str())
        .collect();
    assert!(tags.contains(&"fresh"));

    let remove = json!({
        "ids": [id],
        "action": { "type": "tag_remove", "tags": ["fresh"] }
    });
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v1/targets/bulk-action")
                .header("content-type", "application/json")
                .body(Body::from(remove.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .oneshot(
            Request::get(format!("/api/v1/targets/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let v = body_json(resp).await;
    let tags: Vec<&str> = v["tags"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t.as_str())
        .collect();
    assert!(!tags.contains(&"fresh"));
}

#[tokio::test]
async fn tags_endpoint_reports_aggregate_counts() {
    let app = app();
    create_target(&app, "a").await;
    create_target(&app, "b").await;

    let resp = app
        .oneshot(Request::get("/api/v1/tags").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    let items = v["items"].as_array().unwrap();
    let prod = items
        .iter()
        .find(|t| t["name"] == "prod")
        .expect("prod tag present");
    assert_eq!(prod["count"], 2);
}

#[tokio::test]
async fn check_now_rejects_unknown_target() {
    let id = uuid::Uuid::now_v7();
    let resp = app()
        .oneshot(
            Request::post(format!("/api/v1/targets/{id}/check-now"))
                .header("content-type", "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let v = body_json(resp).await;
    assert_eq!(v["error"]["code"], "TARGET_NOT_FOUND");
}

#[tokio::test]
async fn test_endpoint_rejects_ssrf_target() {
    let payload = json!({
        "check": {
            "type": "http",
            "url": "http://127.0.0.1/",
            "method": "GET",
            "timeout": 5000,
            "follow_redirects": false,
            "max_redirects": 0,
            "expected_status": { "kind": "exact", "value": 200 },
            "headers": {},
            "verify_tls": true
        }
    });
    let app = build_test_app_with_owner(|cfg| {
        cfg.security.allow_private_targets = false;
    });
    let resp = app
        .oneshot(
            Request::post("/api/v1/targets/test")
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = body_json(resp).await;
    assert_eq!(v["error"]["code"], "SSRF_BLOCKED");
}

#[tokio::test]
async fn test_endpoint_unavailable_without_live_agent() {
    let payload = json!({
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
        }
    });
    let resp = app()
        .oneshot(
            Request::post("/api/v1/targets/test")
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body_json(resp).await["error"]["code"], "PROBE_UNAVAILABLE");
}

#[tokio::test]
async fn test_endpoint_with_explicit_region_unavailable_without_live_agent() {
    let payload = json!({
        "region": "apac-sg",
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
        }
    });
    let resp = app()
        .oneshot(
            Request::post("/api/v1/targets/test")
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let v = body_json(resp).await;
    assert_eq!(v["error"]["code"], "PROBE_UNAVAILABLE");
    assert!(
        v["error"]["message"].as_str().unwrap().contains("apac-sg"),
        "503 names the requested region: {v}"
    );
}

#[tokio::test]
async fn check_now_unavailable_without_live_agent() {
    let app = app();
    let id = create_target(&app, "cn").await;
    let resp = app
        .oneshot(
            Request::post(format!("/api/v1/targets/{id}/check-now"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body_json(resp).await["error"]["code"], "PROBE_UNAVAILABLE");
}

#[tokio::test]
async fn dashboard_summary_returns_zero_filled_for_empty_fleet() {
    let resp = app()
        .oneshot(
            Request::get("/api/v1/dashboard/summary")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["targets"]["total"], 0);
    assert_eq!(v["last_24h"]["checks_total"], 0);
}

#[tokio::test]
async fn incidents_endpoint_returns_envelope() {
    let app = app();
    let id = create_target(&app, "no-results-yet").await;
    let resp = app
        .oneshot(
            Request::get(format!("/api/v1/targets/{id}/incidents"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["items"].as_array().unwrap().len(), 0);
    assert_eq!(v["has_more"], false);
}

async fn post_json(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    (status, body_json(resp).await)
}

/// No region catalog in the memory store, so a 422 proves the field is read, not dropped.
#[tokio::test]
async fn create_refuses_a_region_it_cannot_serve() {
    let app = app();
    let mut payload = http_target_payload("pinned");
    payload["regions"] = json!(["eu-nowhere"]);
    let (status, v) = post_json(&app, "/api/v1/targets", payload).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["error"]["code"], "REGION_INVALID");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("eu-nowhere"),
        "{v}"
    );
}

#[tokio::test]
async fn create_refuses_regions_on_a_heartbeat() {
    let app = app();
    let payload = json!({
        "name": "cron",
        "check": { "type": "heartbeat", "period": 300000, "grace": 300000 },
        "interval": 60,
        "regions": ["eu-nowhere"]
    });
    let (status, v) = post_json(&app, "/api/v1/targets", payload).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["error"]["code"], "REGION_INVALID");
}

#[tokio::test]
async fn bulk_create_vets_each_items_regions() {
    let app = app();
    let mut pinned = http_target_payload("pinned");
    pinned["regions"] = json!(["eu-nowhere"]);
    let (status, v) = post_json(
        &app,
        "/api/v1/targets/bulk",
        json!([http_target_payload("plain"), pinned]),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["error"]["code"], "REGION_INVALID");

    let resp = app
        .oneshot(Request::get("/api/v1/targets").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let v = body_json(resp).await;
    assert_eq!(v["items"].as_array().unwrap().len(), 0, "{v}");
}

async fn send_json(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    (status, body_json(resp).await)
}

fn assert_unknown_key(status: StatusCode, v: &Value, key: &str) {
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["error"]["code"], "INVALID_JSON", "{v}");
    let message = v["error"]["message"].as_str().unwrap();
    assert!(message.contains(&format!("unknown field `{key}`")), "{v}");
}

#[tokio::test]
async fn create_refuses_a_key_it_does_not_know() {
    let app = app();
    let mut payload = http_target_payload("typo");
    payload["region"] = json!(["eu-frankfurt"]);
    let (status, v) = send_json(&app, "POST", "/api/v1/targets", payload).await;
    assert_unknown_key(status, &v, "region");

    let id = create_target(&app, "typo-patch").await;
    let (status, v) = send_json(
        &app,
        "PATCH",
        &format!("/api/v1/targets/{id}"),
        json!({ "regions": ["eu-frankfurt"] }),
    )
    .await;
    assert_unknown_key(status, &v, "regions");

    let mut payload = http_target_payload("binding-knob");
    payload["alerts"] = json!([{ "channel_id": uuid::Uuid::nil(), "after_failures": 3 }]);
    let (status, v) = send_json(&app, "POST", "/api/v1/targets", payload).await;
    assert_unknown_key(status, &v, "alerts[0].after_failures");

    let mut payload = http_target_payload("policy-knob");
    payload["region_policy"] = json!({ "count": 2, "mode": "count" });
    let (status, v) = send_json(&app, "POST", "/api/v1/targets", payload).await;
    assert_unknown_key(status, &v, "region_policy.mode");

    let (status, v) = send_json(
        &app,
        "POST",
        "/api/v1/targets/bulk-action",
        json!({ "ids": [id], "action": { "type": "enable", "tags": ["prod"] } }),
    )
    .await;
    assert_unknown_key(status, &v, "action.tags");
}

/// The stored shapes nested in a body cannot carry the serde attribute, so
/// the boundary checks them against the schema instead.
#[tokio::test]
async fn a_key_inside_the_check_is_refused_too() {
    let app = app();

    let mut payload = http_target_payload("nested");
    payload["check"]["timeuot"] = json!(5000);
    let (status, v) = send_json(&app, "POST", "/api/v1/targets", payload).await;
    assert_unknown_key(status, &v, "check.timeuot");
    let message = v["error"]["message"].as_str().unwrap();
    assert!(message.contains("`timeout`"), "{v}");

    let mut payload = http_target_payload("nested-status");
    payload["check"]["expected_status"] =
        json!({ "kind": "range", "value": { "min": 200, "max": 299, "mode": "x" } });
    let (status, v) = send_json(&app, "POST", "/api/v1/targets", payload).await;
    assert_unknown_key(status, &v, "check.expected_status.value.mode");

    let (status, v) = send_json(
        &app,
        "POST",
        "/api/v1/targets",
        json!({
            "name": "flow",
            "interval": 300,
            "check": {
                "type": "flow",
                "start_url": "https://example.com/login",
                "steps": [
                    { "op": "click", "selector": "#go" },
                    { "op": "assert_url", "contains": "/home", "selctor": "#x" }
                ],
                "timeout": 30000,
                "step_timeout": 5000,
                "verify_tls": true
            }
        }),
    )
    .await;
    assert_unknown_key(status, &v, "check.steps[1].selctor");

    // A header name is a map key, not a field: anything goes there.
    let mut payload = http_target_payload("headers");
    payload["check"]["headers"] = json!({ "X-Anything": "1" });
    let (status, _) = send_json(&app, "POST", "/api/v1/targets", payload).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn a_key_inside_a_channel_config_is_refused_with_its_provider_in_mind() {
    let app = app();
    let (status, v) = send_json(
        &app,
        "POST",
        "/api/v1/notification-channels",
        json!({
            "name": "sms",
            "config": {
                "type": "sms", "provider": "vonage", "to": "+15551234567", "from": "+15557654321",
                "api_key": "k", "api_secret": "s", "api_secrte": "typo"
            }
        }),
    )
    .await;
    assert_unknown_key(status, &v, "config.api_secrte");
    let message = v["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("`api_secret`") && !message.contains("`auth_token`"),
        "{v}"
    );
}

/// The walk runs on a value, which keeps the last of two equal keys; the
/// decode runs on the bytes so the repeat is still refused.
#[tokio::test]
async fn a_repeated_key_is_refused() {
    let resp = app()
        .oneshot(
            Request::post("/api/v1/targets")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"name":"a","name":"b","interval":60,"check":{"type":"tcp","host":"h","port":1,"timeout":1000}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let v = body_json(resp).await;
    assert_eq!(v["error"]["code"], "INVALID_JSON", "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("duplicate field `name`"),
        "{v}"
    );
}

#[tokio::test]
async fn a_body_without_a_json_content_type_is_refused() {
    let resp = app()
        .oneshot(
            Request::post("/api/v1/targets")
                .header("content-type", "text/plain")
                .body(Body::from(http_target_payload("x").to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let v = body_json(resp).await;
    assert_eq!(v["error"]["code"], "INVALID_CONTENT_TYPE", "{v}");
}

#[tokio::test]
async fn a_body_that_is_not_json_gets_the_error_envelope() {
    let resp = app()
        .oneshot(
            Request::post("/api/v1/targets")
                .header("content-type", "application/json")
                .body(Body::from("{not json"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = body_json(resp).await;
    assert_eq!(v["error"]["code"], "INVALID_JSON", "{v}");
}
