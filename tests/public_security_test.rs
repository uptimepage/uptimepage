//! Security tests for the unauthenticated public surface.
//!
//! Covers:
//!   * operator auth credentials supplied as cookies / Authorization headers
//!     never echo back into the response on public paths
//!   * `Cache-Control: no-store` is absent on every public response (it would
//!     defeat CDN caching)
//!   * The public surface never emits `Set-Cookie`, so a CDN's request key
//!     stays cookie-free
//!
//! Positive assertions (sensitive fields absent from JSON schema + responses,
//! `public, max-age=10, stale-while-revalidate=30` set on every public path)
//! live in `tests/public_api_contract_test.rs`.
//!
//! The Caddy-layer rate limit (60 req/min per IP via caddy-ratelimit plugin)
//! is exercised in the deployment harness, not here — the Rust process has no
//! rate-limit middleware on `/api/public/*` by design (Caddy owns that).

use crate::common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{body_text, build_test_app_with_web};
use tower::ServiceExt;

const FORGED_COOKIE: &str = "session=stolen-operator-session; admin=true";
const FORGED_AUTH: &str = "Basic YWRtaW46c3VwZXJzZWNyZXQ="; // admin:supersecret

const PUBLIC_PATHS: &[&str] = &[
    "/status",
    "/api/public/v1/status",
    "/api/public/v1/incidents",
    "/api/public/v1/incidents.rss",
    "/api/public/v1/maintenance",
];

#[tokio::test]
async fn public_responses_never_set_cookie() {
    let app = build_test_app_with_web(|_| {});
    for path in PUBLIC_PATHS {
        let resp = app
            .clone()
            .oneshot(Request::get(*path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(
            resp.status().is_success() || resp.status() == StatusCode::SERVICE_UNAVAILABLE,
            "{path}: unexpected status {}",
            resp.status()
        );
        let set_cookie = resp.headers().get_all(header::SET_COOKIE).iter().count();
        assert_eq!(
            set_cookie, 0,
            "{path}: leaked Set-Cookie ({set_cookie} headers)"
        );
    }
}

#[tokio::test]
async fn public_responses_omit_no_store_cache_directive() {
    // `no-store` would defeat CDN caching of the public surface. Assert
    // it is absent on every public response. A 5xx regression on these paths
    // is itself a bug — fail loudly instead of skipping.
    let app = build_test_app_with_web(|_| {});
    for path in PUBLIC_PATHS {
        let resp = app
            .clone()
            .oneshot(Request::get(*path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(
            resp.status().is_success(),
            "{path}: expected 2xx with NoopPublicSource, got {}",
            resp.status()
        );
        let cc = resp
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        assert!(
            !cc.contains("no-store"),
            "{path}: Cache-Control contains no-store ({cc})"
        );
    }
}

#[tokio::test]
async fn public_responses_do_not_echo_forged_auth_or_cookie() {
    // A misbehaving operator client (or curious attacker) presents stolen
    // credentials to a public endpoint. The handler must ignore them and must
    // not echo them — neither in the body nor in any response header.
    let app = build_test_app_with_web(|_| {});
    for path in PUBLIC_PATHS {
        let req = Request::get(*path)
            .header(header::COOKIE, FORGED_COOKIE)
            .header(header::AUTHORIZATION, FORGED_AUTH)
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        // Headers — sweep every value.
        for (name, value) in resp.headers().iter() {
            let raw = value.to_str().unwrap_or("");
            assert!(
                !raw.contains("stolen-operator-session"),
                "{path}: header {name} echoed forged cookie"
            );
            assert!(
                !raw.contains("YWRtaW46c3VwZXJzZWNyZXQ"),
                "{path}: header {name} echoed Authorization value"
            );
        }
        // Body — make sure neither the cookie nor the base64 credential
        // surfaces. (Status pages and JSON envelopes have no reason to
        // include either.)
        let body = body_text(resp).await;
        assert!(
            !body.contains("stolen-operator-session"),
            "{path}: body echoed forged cookie\n{body}"
        );
        assert!(
            !body.contains("YWRtaW46c3VwZXJzZWNyZXQ"),
            "{path}: body echoed Authorization value\n{body}"
        );
    }
}

// Positive-directive coverage (Cache-Control includes `public, max-age=10,
// stale-while-revalidate=30` on every public path) lives in
// `tests/public_api_contract_test.rs::public_endpoints_set_public_cache_headers`.
// Keep this file focused on the inverse — `no-store` absent + no credential
// echo — so the security surface area is testable independently.
