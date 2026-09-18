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
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn healthz_returns_ok() {
    let resp = app()
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["status"], "ok");
}

#[tokio::test]
async fn readyz_returns_ready_with_inmemory_store() {
    let resp = app()
        .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["status"], "ready");
}

#[tokio::test]
async fn list_targets_empty() {
    let resp = app()
        .oneshot(Request::get("/api/v1/targets").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["items"].as_array().unwrap().len(), 0);
    assert_eq!(v["has_more"], false);
    assert_eq!(v["limit"], 50);
    assert_eq!(v["offset"], 0);
}

#[tokio::test]
async fn regions_catalog_endpoint_returns_shape() {
    let resp = app()
        .oneshot(Request::get("/api/v1/regions").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    // In-memory store has no region catalog; the contract is a `regions` array.
    assert_eq!(v["regions"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn create_then_get_then_delete_target() {
    let app = app();
    let payload = json!({
        "name": "ex",
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
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = body_json(resp).await;
    let id = created["id"].as_str().unwrap().to_string();

    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/targets/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let got = body_json(resp).await;
    assert_eq!(got["name"], "ex");

    let resp = app
        .clone()
        .oneshot(
            Request::delete(format!("/api/v1/targets/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = app
        .oneshot(
            Request::get(format!("/api/v1/targets/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_unknown_target_is_404() {
    let id = uuid::Uuid::now_v7();
    let resp = app()
        .oneshot(
            Request::get(format!("/api/v1/targets/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn bulk_create_rejects_empty() {
    let resp = app()
        .oneshot(
            Request::post("/api/v1/targets/bulk")
                .header("content-type", "application/json")
                .body(Body::from("[]"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

fn tcp_target(name: &str) -> Value {
    json!({
        "name": name,
        "check": {"type": "tcp", "host": "db.example.com", "port": 5432, "timeout": 1000},
        "interval": 60
    })
}

#[tokio::test]
async fn create_without_owner_is_owned_by_the_caller() {
    let created = post_and_body(app(), tcp_target("db")).await;
    assert_eq!(
        created["owner_user_id"],
        json!(common::test_user_id().0.to_string())
    );
}

#[tokio::test]
async fn create_with_null_owner_stays_unowned() {
    let mut payload = tcp_target("db");
    payload["owner_user_id"] = Value::Null;
    let created = post_and_body(app(), payload).await;
    assert_eq!(created["owner_user_id"], Value::Null);
}

#[tokio::test]
async fn bulk_create_owner_default_follows_each_item() {
    let mut unowned = tcp_target("b");
    unowned["owner_user_id"] = Value::Null;
    let resp = app()
        .oneshot(common::json_request(
            "POST",
            "/api/v1/targets/bulk",
            json!([tcp_target("a"), unowned]),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let items = body_json(resp).await;
    assert_eq!(
        items[0]["owner_user_id"],
        json!(common::test_user_id().0.to_string())
    );
    assert_eq!(items[1]["owner_user_id"], Value::Null);
}

/// The bulk INSERT omits the column, so a dropped follow-up write leaves the
/// monitor on the default quorum without saying so.
#[tokio::test]
async fn bulk_create_keeps_each_items_region_policy() {
    let app = app();
    let item = json!({
        "name": "bulk-policy",
        "check": {
            "type": "http",
            "url": "https://example.com/",
            "method": "GET",
            "timeout": 5000,
            "follow_redirects": false,
            "max_redirects": 0,
            "expected_status": { "kind": "exact", "value": 200 },
            "headers": {},
            "verify_tls": true
        },
        "interval": 60,
        "region_policy": "any"
    });
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v1/targets/bulk")
                .header("content-type", "application/json")
                .body(Body::from(json!([item]).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = body_json(resp).await;
    let id = created[0]["id"].as_str().unwrap().to_string();

    let resp = app
        .oneshot(
            Request::get(format!("/api/v1/targets/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["region_policy"], "any");
}

fn ssrf_payload(url: &str) -> Value {
    json!({
        "name": "ssrf-attempt",
        "check": {
            "type": "http",
            "url": url,
            "method": "GET",
            "timeout": 5000,
            "follow_redirects": false,
            "max_redirects": 0,
            "expected_status": { "kind": "exact", "value": 200 },
            "headers": {},
            "verify_tls": true
        },
        "interval": 60,
        "tags": []
    })
}

async fn post_target(payload: Value) -> StatusCode {
    app()
        .oneshot(
            Request::post("/api/v1/targets")
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn ssrf_rejects_loopback_ipv4_literal() {
    assert_eq!(
        post_target(ssrf_payload("http://127.0.0.1/")).await,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn ssrf_rejects_aws_metadata_literal() {
    assert_eq!(
        post_target(ssrf_payload("http://169.254.169.254/latest/meta-data/")).await,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn ssrf_rejects_private_rfc1918_literal() {
    assert_eq!(
        post_target(ssrf_payload("http://10.0.0.1/")).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        post_target(ssrf_payload("http://192.168.1.1/")).await,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn ssrf_rejects_loopback_ipv6_literal() {
    assert_eq!(
        post_target(ssrf_payload("http://[::1]/")).await,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn ssrf_allows_public_hostname() {
    assert_eq!(
        post_target(ssrf_payload("http://example.com/")).await,
        StatusCode::CREATED
    );
}

#[tokio::test]
async fn ssrf_rejects_tcp_loopback_literal() {
    let payload = json!({
        "name": "tcp-loopback",
        "check": {
            "type": "tcp",
            "host": "127.0.0.1",
            "port": 22,
            "timeout": 5000
        },
        "interval": 60,
        "tags": []
    });
    assert_eq!(post_target(payload).await, StatusCode::BAD_REQUEST);
}

fn http_with_auth(name: &str, auth_field: &str, auth_value: Value) -> Value {
    let mut payload = json!({
        "name": name,
        "check": {
            "type": "http",
            "url": "http://example.com/",
            "method": "GET",
            "timeout": 5000,
            "follow_redirects": false,
            "max_redirects": 0,
            "expected_status": { "kind": "exact", "value": 200 },
            "headers": {},
            "verify_tls": true
        },
        "interval": 60,
        "tags": []
    });
    payload["check"][auth_field] = auth_value;
    payload
}

fn http_with_basic_auth() -> Value {
    http_with_auth("with-basic", "basic_auth", json!(["alice", "s3cret"]))
}

fn http_with_bearer() -> Value {
    http_with_auth("with-bearer", "bearer_token", json!("tok.en.value"))
}

async fn post_and_body(app: axum::Router, payload: Value) -> Value {
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
    body_json(resp).await
}

#[tokio::test]
async fn redacts_basic_auth_in_create_response() {
    let body = post_and_body(app(), http_with_basic_auth()).await;
    assert_eq!(body["check"]["basic_auth"], json!(["***", "***"]));
}

#[tokio::test]
async fn redacts_bearer_token_in_create_response() {
    let body = post_and_body(app(), http_with_bearer()).await;
    assert_eq!(body["check"]["bearer_token"], json!("***"));
}

#[tokio::test]
async fn redacts_basic_auth_in_get_response() {
    let app = app();
    let created = post_and_body(app.clone(), http_with_basic_auth()).await;
    let id = created["id"].as_str().unwrap();
    let resp = app
        .oneshot(
            Request::get(format!("/api/v1/targets/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_json(resp).await;
    assert_eq!(body["check"]["basic_auth"], json!(["***", "***"]));
}

#[tokio::test]
async fn rejects_redaction_sentinel_in_basic_auth() {
    let mut payload = http_with_basic_auth();
    payload["check"]["basic_auth"] = json!(["***", "***"]);
    assert_eq!(post_target(payload).await, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn rejects_redaction_sentinel_in_bearer_token() {
    let mut payload = http_with_bearer();
    payload["check"]["bearer_token"] = json!("***");
    assert_eq!(post_target(payload).await, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn rejects_verify_tls_false_with_basic_auth_over_https() {
    let mut payload = http_with_basic_auth();
    payload["check"]["url"] = json!("https://example.com/");
    payload["check"]["verify_tls"] = json!(false);
    assert_eq!(post_target(payload).await, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn rejects_verify_tls_false_with_bearer_over_https() {
    let mut payload = http_with_bearer();
    payload["check"]["url"] = json!("https://example.com/");
    payload["check"]["verify_tls"] = json!(false);
    assert_eq!(post_target(payload).await, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn accepts_verify_tls_true_with_credentials_over_https() {
    let mut payload = http_with_basic_auth();
    payload["check"]["url"] = json!("https://example.com/");
    payload["check"]["verify_tls"] = json!(true);
    assert_eq!(post_target(payload).await, StatusCode::CREATED);
}

fn tls_cert_payload(host: &str, warn: u32, critical: u32) -> Value {
    json!({
        "name": "cert-check",
        "check": {
            "type": "tls_cert",
            "host": host,
            "port": 443,
            "warn_days": warn,
            "critical_days": critical,
            "timeout": 5000
        },
        "interval": 86400,
        "tags": []
    })
}

#[tokio::test]
async fn create_tls_cert_target() {
    let resp = app()
        .oneshot(
            Request::post("/api/v1/targets")
                .header("content-type", "application/json")
                .body(Body::from(
                    tls_cert_payload("example.com", 14, 7).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["check"]["type"], "tls_cert");
    assert_eq!(body["check"]["host"], "example.com");
}

#[tokio::test]
async fn create_tls_cert_rejects_warn_le_critical() {
    assert_eq!(
        post_target(tls_cert_payload("example.com", 5, 7)).await,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn create_tls_cert_rejects_port_zero() {
    let mut payload = tls_cert_payload("example.com", 14, 7);
    payload["check"]["port"] = json!(0);
    assert_eq!(post_target(payload).await, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_tls_cert_rejects_loopback_ipv4_literal() {
    let payload = tls_cert_payload("127.0.0.1", 14, 7);
    assert_eq!(post_target(payload).await, StatusCode::BAD_REQUEST);
}

fn domain_expiry_payload(domain: &str, warn: u32, critical: u32) -> Value {
    json!({
        "name": "domain-check",
        "check": {
            "type": "domain_expiry",
            "domain": domain,
            "warn_days": warn,
            "critical_days": critical,
            "timeout": 10000
        },
        "interval": 86400,
        "tags": []
    })
}

#[tokio::test]
async fn create_domain_expiry_target() {
    let resp = app()
        .oneshot(
            Request::post("/api/v1/targets")
                .header("content-type", "application/json")
                .body(Body::from(
                    domain_expiry_payload("example.com", 30, 7).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["check"]["type"], "domain_expiry");
    assert_eq!(body["check"]["domain"], "example.com");
}

#[tokio::test]
async fn create_domain_expiry_rejects_bare_label() {
    assert_eq!(
        post_target(domain_expiry_payload("example", 30, 7)).await,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn create_domain_expiry_rejects_degenerate_dot_inputs() {
    for bad in [".", ".a", "a.", "..", " "] {
        assert_eq!(
            post_target(domain_expiry_payload(bad, 30, 7)).await,
            StatusCode::BAD_REQUEST,
            "expected 400 for {bad:?}"
        );
    }
}

#[tokio::test]
async fn create_domain_expiry_rejects_warn_le_critical() {
    assert_eq!(
        post_target(domain_expiry_payload("example.com", 7, 7)).await,
        StatusCode::BAD_REQUEST
    );
}

fn target_payload_with_alerts(alerts: Value) -> Value {
    json!({
        "name": "with-alerts",
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
        "tags": [],
        "alerts": alerts
    })
}

#[tokio::test]
async fn rejects_alerts_binding_to_unknown_channel() {
    // A binding must reference a channel the org owns. With no channel
    // created, any channel_id is unknown → 400. (The positive round-trip,
    // which needs the channel CRUD API to seed a real channel, is covered
    // in the Phase 3 notification-channel suite.)
    let payload = target_payload_with_alerts(json!([
        { "channel_id": "00000000-0000-0000-0000-0000000000aa" }
    ]));
    assert_eq!(post_target(payload).await, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn create_target_without_alerts_defaults_empty() {
    let resp = app()
        .oneshot(
            Request::post("/api/v1/targets")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "name": "no-alerts",
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
                        "tags": []
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["alerts"], json!([]));
}

#[tokio::test]
async fn rejects_zero_alert_confirmations() {
    // Alerting after zero failures is meaningless; rejected structurally.
    let mut payload = target_payload_with_alerts(json!([]));
    payload["alert_confirmations"] = json!(0);
    assert_eq!(post_target(payload).await, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn accepts_verify_tls_false_without_credentials() {
    let payload = json!({
        "name": "no-creds-self-signed",
        "check": {
            "type": "http",
            "url": "https://example.com/",
            "method": "GET",
            "timeout": 5000,
            "follow_redirects": false,
            "max_redirects": 0,
            "expected_status": { "kind": "exact", "value": 200 },
            "headers": {},
            "verify_tls": false
        },
        "interval": 60,
        "tags": []
    });
    assert_eq!(post_target(payload).await, StatusCode::CREATED);
}

/// The monitor keeps its id across a PATCH, so a swapped kind would pile a
/// second kind's results onto the first one's history.
#[tokio::test]
async fn patch_refuses_a_check_of_another_kind() {
    let app = app();
    let id = post_and_body(app.clone(), tcp_target("db")).await["id"]
        .as_str()
        .unwrap()
        .to_string();

    let path = format!("/api/v1/targets/{id}");
    let swapped = json!({ "check": ssrf_payload("https://example.com/")["check"] });
    let resp = app
        .clone()
        .oneshot(common::json_request("PATCH", &path, swapped))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let err = body_json(resp).await["error"].clone();
    assert_eq!(err["code"], "CHECK_KIND_IMMUTABLE");
    assert_eq!(err["field"], "check.type");

    let same_kind = json!({
        "check": {"type": "tcp", "host": "db.example.com", "port": 5433, "timeout": 1000}
    });
    let resp = app
        .oneshot(common::json_request("PATCH", &path, same_kind))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["check"]["type"], "tcp");
    assert_eq!(body["check"]["port"], 5433);
}
