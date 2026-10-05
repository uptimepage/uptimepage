//! CSRF guard tests.
//!
//! These check what the *middleware* does — not the full handler. A request
//! that passes CSRF still typically fails downstream (401 / 400 / 404) because
//! the test scaffold doesn't seed users/tokens. We only assert on the 403
//! `CSRF_PROTECTION` boundary.

use crate::common;

use axum::body::Body;
use axum::http::{Request, StatusCode, request::Builder};
use common::{build_test_app, build_test_app_with_web};
use tower::ServiceExt;

const SESSION_COOKIE: &str = "_sm_session=fakesession";

fn req(method: &str, path: &str) -> Builder {
    Request::builder().method(method).uri(path)
}

fn body_text(resp: axum::response::Response) -> String {
    futures::executor::block_on(async {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .unwrap();
        String::from_utf8_lossy(&bytes).to_string()
    })
}

#[tokio::test]
async fn get_request_is_never_csrf_rejected() {
    let app = build_test_app(|_| {});
    let resp = app
        .oneshot(
            req("GET", "/api/v1/targets")
                .header("cookie", SESSION_COOKIE)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn post_with_cookie_and_no_xrw_header_is_rejected() {
    let app = build_test_app(|_| {});
    let resp = app
        .oneshot(
            req("POST", "/api/v1/targets")
                .header("cookie", SESSION_COOKIE)
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = body_text(resp);
    assert!(
        body.contains("CSRF_PROTECTION"),
        "expected CSRF_PROTECTION code, got: {body}"
    );
}

#[tokio::test]
async fn post_with_cookie_and_xrw_header_passes_guard() {
    let app = build_test_app(|_| {});
    let resp = app
        .oneshot(
            req("POST", "/api/v1/targets")
                .header("cookie", SESSION_COOKIE)
                .header("x-requested-with", "uptimepage")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    // Guard passes; whatever the handler returns is fine — just not 403/CSRF.
    let status = resp.status();
    let body = body_text(resp);
    assert!(
        !body.contains("CSRF_PROTECTION"),
        "guard should have passed; got {status}: {body}"
    );
}

#[tokio::test]
async fn post_with_bearer_and_no_xrw_header_passes_guard() {
    let app = build_test_app(|_| {});
    let resp = app
        .oneshot(
            req("POST", "/api/v1/targets")
                .header("authorization", "Bearer sm_live_dummy_token")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_text(resp);
    assert!(
        !body.contains("CSRF_PROTECTION"),
        "bearer auth should skip CSRF, got: {body}"
    );
}

#[tokio::test]
async fn post_without_any_auth_skips_csrf_guard() {
    // No cookie, no bearer → CSRF guard does not apply (downstream returns
    // 401/whatever). The middleware exists to protect *authenticated* state
    // changes from cross-site forgery; without auth there is nothing to
    // forge against.
    let app = build_test_app(|_| {});
    let resp = app
        .oneshot(
            req("POST", "/api/v1/targets")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_text(resp);
    assert!(
        !body.contains("CSRF_PROTECTION"),
        "no auth → no CSRF reject expected, got: {body}"
    );
}

#[tokio::test]
async fn delete_with_cookie_and_no_xrw_header_is_rejected() {
    let app = build_test_app(|_| {});
    let resp = app
        .oneshot(
            req(
                "DELETE",
                "/api/v1/targets/00000000-0000-0000-0000-000000000001",
            )
            .header("cookie", SESSION_COOKIE)
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn web_router_post_with_cookie_is_csrf_protected() {
    // Lock-in: CSRF wraps the *merged* router (api + web), not just
    // the api half. `/` is a GET-only web route — a POST would 405,
    // but the outer CSRF layer must catch missing X-Requested-With
    // first and 403. If CSRF ever drifts back to api-only, this test
    // returns 405 and fails — preventing the silent-miss drift the
    // structural fix exists to prevent.
    let app = build_test_app_with_web(|_| {});
    let resp = app
        .oneshot(
            req("POST", "/")
                .header("cookie", SESSION_COOKIE)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = body_text(resp);
    assert!(
        body.contains("CSRF_PROTECTION"),
        "web router POST must be CSRF-gated, got: {body}"
    );
}

#[tokio::test]
async fn unrelated_cookie_does_not_trigger_csrf_guard() {
    // Cookie header is present but doesn't include the session cookie. The
    // middleware should treat the request as unauthenticated and skip.
    let app = build_test_app(|_| {});
    let resp = app
        .oneshot(
            req("POST", "/api/v1/targets")
                .header("cookie", "tracking=abc; theme=dark")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_text(resp);
    assert!(
        !body.contains("CSRF_PROTECTION"),
        "unrelated cookie should not trip CSRF guard, got: {body}"
    );
}
