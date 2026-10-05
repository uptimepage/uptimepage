//! HTTP contract for `/api/v1/variables` + the save-time validation of `{{var}}`
//! references on monitor create. The load-bearing guarantees: a secret value
//! never appears in any response, a referenced variable can't be deleted, and a
//! monitor referencing an unknown variable is rejected at save.

use crate::common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    build_saas_router_with_pg_targets, build_test_app_with_owner, make_user, pg_pool_from_env,
    unique_slug, with_session,
};
use serde_json::{Value, json};
use tower::ServiceExt;
use uptimepage::storage::create_org_with_owner;

const SECRET: &str = "sk-super-secret-9f3a";

async fn send(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, String) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(body.map_or(Body::empty(), |v| Body::from(v.to_string())))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 8 << 20)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn header_var_target(name: &str, token: &str) -> Value {
    json!({
        "name": name,
        "check": {
            "type": "http", "url": "https://example.com/", "method": "GET",
            "timeout": 5000, "follow_redirects": false, "max_redirects": 0,
            "expected_status": { "kind": "exact", "value": 200 },
            "headers": { "x-api-key": token }, "verify_tls": true
        },
        "interval": 300
    })
}

#[tokio::test]
async fn secret_value_never_appears_in_responses() {
    let app = build_test_app_with_owner(|_| {});

    let (status, body) = send(
        &app,
        "POST",
        "/api/v1/variables",
        Some(json!({ "key": "api_key", "is_secret": true, "value": SECRET })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(
        !body.contains(SECRET),
        "secret leaked in create response: {body}"
    );
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(v["value"].is_null(), "secret value must be redacted");
    assert_eq!(v["is_secret"], true);
    assert_eq!(v["used_by"], 0);

    let (_, list) = send(&app, "GET", "/api/v1/variables", None).await;
    assert!(!list.contains(SECRET), "secret leaked in list: {list}");

    // Rotating keeps it redacted and out of the body.
    let id = v["id"].as_str().unwrap();
    let (status, body) = send(
        &app,
        "PATCH",
        &format!("/api/v1/variables/{id}"),
        Some(json!({ "value": "sk-rotated-xyz" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        !body.contains("sk-rotated-xyz"),
        "rotated secret leaked: {body}"
    );
}

#[tokio::test]
async fn plain_variable_value_is_visible() {
    let app = build_test_app_with_owner(|_| {});
    let (status, body) = send(
        &app,
        "POST",
        "/api/v1/variables",
        Some(json!({ "key": "base", "is_secret": false, "value": "api.example.com" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["value"], "api.example.com");
}

#[tokio::test]
async fn invalid_key_and_duplicate_are_rejected() {
    let app = build_test_app_with_owner(|_| {});

    let (status, body) = send(
        &app,
        "POST",
        "/api/v1/variables",
        Some(json!({ "key": "Bad-Key", "value": "x" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("INVALID_VARIABLE_KEY"), "{body}");

    let mk = || json!({ "key": "dup", "value": "x" });
    let (s1, _) = send(&app, "POST", "/api/v1/variables", Some(mk())).await;
    assert_eq!(s1, StatusCode::CREATED);
    let (s2, body) = send(&app, "POST", "/api/v1/variables", Some(mk())).await;
    assert_eq!(s2, StatusCode::CONFLICT);
    assert!(body.contains("VARIABLE_KEY_EXISTS"), "{body}");
}

#[tokio::test]
async fn monitor_referencing_unknown_variable_is_rejected_known_passes() {
    let app = build_test_app_with_owner(|_| {});

    // Unknown reference → 422 at save.
    let (status, body) = send(
        &app,
        "POST",
        "/api/v1/targets",
        Some(header_var_target("ghost", "{{ghost}}")),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body.contains("UNRESOLVED_VARIABLE"), "{body}");

    // Define it, then the same monitor saves.
    let (s, _) = send(
        &app,
        "POST",
        "/api/v1/variables",
        Some(json!({ "key": "ghost", "is_secret": true, "value": SECRET })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let (status, body) = send(
        &app,
        "POST",
        "/api/v1/targets",
        Some(header_var_target("ghost", "{{ghost}}")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn used_by_counts_and_delete_is_blocked_when_referenced() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "var-api").await;
    let org = create_org_with_owner(&pool, user, &unique_slug("var-api"), "O")
        .await
        .unwrap()
        .expect("org")
        .id;
    let app = with_session(
        build_saas_router_with_pg_targets(pool.clone()).await,
        user,
        Some(org),
        Some("var-api-session"),
    );

    let (s, body) = send(
        &app,
        "POST",
        "/api/v1/variables",
        Some(json!({ "key": "api_key", "is_secret": true, "value": SECRET })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    let id = serde_json::from_str::<Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // A monitor referencing it.
    let (s, body) = send(
        &app,
        "POST",
        "/api/v1/targets",
        Some(header_var_target("svc", "{{api_key}}")),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");

    // used_by reflects the reference, and delete is blocked.
    let (_, body) = send(&app, "GET", &format!("/api/v1/variables/{id}"), None).await;
    assert_eq!(body_json_value(&body)["used_by"], 1);
    let (s, body) = send(&app, "DELETE", &format!("/api/v1/variables/{id}"), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("VARIABLE_IN_USE"), "{body}");

    // An unreferenced variable deletes cleanly.
    let (_, body) = send(
        &app,
        "POST",
        "/api/v1/variables",
        Some(json!({ "key": "lonely", "value": "x" })),
    )
    .await;
    let lonely = body_json_value(&body)["id"].as_str().unwrap().to_string();
    let (s, _) = send(&app, "DELETE", &format!("/api/v1/variables/{lonely}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org.0)
        .execute(&pool)
        .await
        .ok();
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .ok();
}

fn body_json_value(s: &str) -> Value {
    serde_json::from_str(s).unwrap()
}
