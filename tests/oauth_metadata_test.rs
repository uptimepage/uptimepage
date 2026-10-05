//! OAuth discovery metadata + the RFC 9728 `WWW-Authenticate` challenge. No DB
//! needed — these read config only.

use crate::common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{body_json, build_test_app_with_web};
use tower::ServiceExt;

const ISSUER: &str = "https://app.test.example";
const RESOURCE: &str = "https://mcp.test.example/mcp";

fn oauth_app() -> axum::Router {
    build_test_app_with_web(|cfg| {
        cfg.mcp.enabled = true;
        cfg.mcp.oauth_enabled = true;
        cfg.mcp.resource_uri = RESOURCE.into();
        cfg.auth.public_base_url = ISSUER.into();
    })
}

async fn get(app: axum::Router, path: &str) -> axum::http::Response<Body> {
    app.oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn authorization_server_metadata() {
    let resp = get(oauth_app(), "/.well-known/oauth-authorization-server").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert_eq!(j["issuer"], ISSUER);
    assert_eq!(
        j["authorization_endpoint"],
        format!("{ISSUER}/oauth/authorize")
    );
    assert_eq!(j["token_endpoint"], format!("{ISSUER}/oauth/token"));
    assert_eq!(
        j["registration_endpoint"],
        format!("{ISSUER}/oauth/register")
    );
    assert_eq!(j["code_challenge_methods_supported"][0], "S256");
    assert_eq!(j["grant_types_supported"][0], "authorization_code");
    assert_eq!(j["response_types_supported"][0], "code");
    assert_eq!(j["token_endpoint_auth_methods_supported"][0], "none");
}

#[tokio::test]
async fn protected_resource_metadata_both_paths() {
    for path in [
        "/.well-known/oauth-protected-resource",
        "/.well-known/oauth-protected-resource/mcp",
    ] {
        let resp = get(oauth_app(), path).await;
        assert_eq!(resp.status(), StatusCode::OK, "path {path}");
        let j = body_json(resp).await;
        assert_eq!(j["resource"], RESOURCE);
        assert_eq!(j["authorization_servers"][0], ISSUER);
        assert_eq!(j["bearer_methods_supported"][0], "header");
    }
}

#[tokio::test]
async fn unauthenticated_mcp_points_at_resource_metadata() {
    let resp = oauth_app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let wa = resp
        .headers()
        .get("www-authenticate")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        wa.contains(
            "resource_metadata=\"https://mcp.test.example/.well-known/oauth-protected-resource\""
        ),
        "challenge must point at the protected-resource metadata, got: {wa}"
    );
    assert!(
        wa.contains("scope="),
        "challenge should advertise scope, got: {wa}"
    );
}

#[tokio::test]
async fn oauth_endpoints_absent_when_disabled() {
    // resource_uri set but oauth_enabled = false → AS endpoints not mounted.
    let app = build_test_app_with_web(|cfg| {
        cfg.mcp.enabled = true;
        cfg.mcp.oauth_enabled = false;
        cfg.mcp.resource_uri = RESOURCE.into();
    });
    let resp = get(app, "/.well-known/oauth-authorization-server").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
