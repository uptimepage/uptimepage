mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::build_test_app;
use serde_json::Value;
use tower::ServiceExt;

fn app() -> axum::Router {
    build_test_app(|_| {})
}

async fn body_json(resp: axum::http::Response<Body>) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 8 << 20)
        .await
        .expect("collect body");
    serde_json::from_slice(&bytes).expect("valid json")
}

#[tokio::test]
async fn openapi_doc_is_openapi_3_1() {
    let resp = app()
        .oneshot(
            Request::get("/api/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let doc = body_json(resp).await;
    let version = doc["openapi"]
        .as_str()
        .expect("openapi version string present");
    assert!(
        version.starts_with("3.1"),
        "expected OpenAPI 3.1.x, got {version}"
    );
}

#[tokio::test]
async fn openapi_doc_lists_every_documented_path() {
    let resp = app()
        .oneshot(
            Request::get("/api/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let doc = body_json(resp).await;
    let paths = doc["paths"].as_object().expect("paths object present");
    for expected in [
        "/healthz",
        "/readyz",
        "/api/v1/targets",
        "/api/v1/targets/bulk",
        "/api/v1/targets/bulk-action",
        "/api/v1/targets/test",
        "/api/v1/targets/{id}",
        "/api/v1/targets/{id}/check-now",
        "/api/v1/targets/{id}/results",
        "/api/v1/targets/{id}/flow-steps",
        "/api/v1/targets/{id}/uptime",
        "/api/v1/targets/{id}/incidents",
        "/api/v1/tags",
        "/api/v1/dashboard/summary",
        "/api/v1/maintenance",
        "/api/v1/maintenance/{id}",
        "/api/v1/notification-channels",
        "/api/v1/notification-channels/test",
        "/api/v1/notification-channels/{id}",
        "/api/v1/notification-channels/{id}/test",
        "/api/v1/notification-channels/{id}/resend-verification",
        "/api/v1/incidents/{id}",
        "/api/v1/incidents/{id}/updates",
        "/api/v1/status-pages",
        "/api/v1/status-pages/{id}",
        "/api/v1/status-pages/{id}/components",
        "/api/v1/status-pages/{id}/components/{target_id}",
        "/api/v1/status-pages/{id}/components/reorder",
        "/api/v1/status-pages/{id}/logo",
        "/api/v1/me/api-tokens",
        "/api/v1/me/api-tokens/{id}",
        "/api/public/v1/status",
        "/api/public/v1/components/{id}/history",
        "/api/public/v1/incidents",
        "/api/public/v1/incidents/{id}",
        "/api/public/v1/incidents.rss",
        "/api/public/v1/maintenance",
    ] {
        assert!(paths.contains_key(expected), "missing path {expected}");
    }
}

#[tokio::test]
async fn public_endpoints_have_empty_security() {
    let resp = app()
        .oneshot(
            Request::get("/api/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let doc = body_json(resp).await;
    for path in [
        "/api/public/v1/status",
        "/api/public/v1/components/{id}/history",
        "/api/public/v1/incidents",
        "/api/public/v1/incidents/{id}",
        "/api/public/v1/incidents.rss",
        "/api/public/v1/maintenance",
    ] {
        let security = &doc["paths"][path]["get"]["security"];
        assert!(
            security.is_array() && security.as_array().unwrap().is_empty(),
            "{path}: security should be [] (no auth); got {security}",
        );
    }
}

#[test]
fn dump_openapi_spec_for_inspection() {
    use uptimepage::api::ApiDoc;
    use utoipa::OpenApi;
    let doc = ApiDoc::openapi().to_pretty_json().unwrap();
    std::fs::write("/tmp/openapi.json", &doc).unwrap();
}

#[tokio::test]
async fn swagger_ui_is_reachable() {
    let resp = app()
        .oneshot(Request::get("/docs").body(Body::empty()).unwrap())
        .await
        .unwrap();
    // SwaggerUi serves an index page at /docs or redirects to /docs/.
    assert!(
        resp.status().is_success() || resp.status().is_redirection(),
        "unexpected /docs status: {}",
        resp.status()
    );
}

/// Every request body refuses a key it does not name, so a setting under the
/// wrong name fails instead of vanishing. The published schema says so where
/// serde can carry the attribute; the rest is strict at the boundary through
/// the schema walk.
#[tokio::test]
async fn every_request_body_refuses_unknown_keys() {
    let resp = app()
        .oneshot(
            Request::get("/api/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let doc = body_json(resp).await;
    let schemas = &doc["components"]["schemas"];
    // Every body is strict at the boundary through the schema walk in
    // `api::strict`; this test keeps the published schema saying so where it
    // can. CheckSpec and ChannelConfig are stored shapes, so their serde
    // types cannot carry the attribute, and the two enums' schemas have no
    // place for it. new_endpoints_test covers all four at runtime.
    let tolerant = [
        "CheckSpec",
        "ChannelConfig",
        "RegionIncidentPolicy",
        "BulkAction",
    ];

    let mut lenient = Vec::new();
    for (path, item) in doc["paths"].as_object().unwrap() {
        for (method, op) in item.as_object().unwrap() {
            if !matches!(method.as_str(), "post" | "put" | "patch") {
                continue;
            }
            let Some(schema) =
                op["requestBody"]["content"]["application/json"]["schema"].as_object()
            else {
                continue;
            };
            let mut seen = Vec::new();
            walk(
                &Value::Object(schema.clone()),
                schemas,
                &tolerant,
                &mut seen,
                &format!("{method} {path}"),
                &mut lenient,
            );
        }
    }
    assert!(
        lenient.is_empty(),
        "bodies that still ignore unknown keys:\n{}",
        lenient.join("\n")
    );
}

fn walk(
    schema: &Value,
    schemas: &Value,
    tolerant: &[&str],
    seen: &mut Vec<String>,
    at: &str,
    lenient: &mut Vec<String>,
) {
    if let Some(reference) = schema["$ref"].as_str() {
        let name = reference.rsplit('/').next().unwrap();
        if tolerant.contains(&name) || seen.iter().any(|s| s == name) {
            return;
        }
        seen.push(name.to_string());
        return walk(
            &schemas[name],
            schemas,
            tolerant,
            seen,
            &format!("{at} > {name}"),
            lenient,
        );
    }
    if let Some(items) = schema.get("items") {
        return walk(items, schemas, tolerant, seen, at, lenient);
    }
    for key in ["oneOf", "anyOf", "allOf"] {
        if let Some(variants) = schema[key].as_array() {
            for v in variants {
                walk(v, schemas, tolerant, seen, at, lenient);
            }
            return;
        }
    }
    let Some(properties) = schema["properties"].as_object() else {
        return;
    };
    if schema["additionalProperties"] != Value::Bool(false) {
        lenient.push(at.to_string());
    }
    for (name, prop) in properties {
        walk(
            prop,
            schemas,
            tolerant,
            seen,
            &format!("{at}.{name}"),
            lenient,
        );
    }
}
