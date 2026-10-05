//! Magic-link route-presence gating. With `auth.enabled_methods` excluding
//! `"magic_link"` the request/verify paths are absent → 404. With the method
//! included the paths are mounted (downstream still fails because the in-mem
//! scaffold has no users, but the surface exists).

use crate::common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::build_test_app;
use tower::ServiceExt;

#[tokio::test]
async fn magic_link_request_is_404_when_disabled() {
    let app = build_test_app(|cfg| {
        cfg.auth.enabled_methods = vec!["github_oauth".into()];
    });
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/magic-link/request")
                .header("content-type", "application/json")
                .header("x-requested-with", "uptimepage")
                .body(Body::from(r#"{"email":"a@b.test"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn magic_link_verify_is_404_when_disabled() {
    let app = build_test_app(|cfg| {
        cfg.auth.enabled_methods = vec!["github_oauth".into()];
    });
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/auth/magic-link/verify?token=anything")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn magic_link_request_is_mounted_when_enabled() {
    let app = build_test_app(|cfg| {
        cfg.auth.enabled_methods = vec!["github_oauth".into(), "magic_link".into()];
    });
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/magic-link/request")
                .header("content-type", "application/json")
                .header("x-requested-with", "uptimepage")
                .body(Body::from(r#"{"email":"nobody@example.test"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    // No Postgres pool in the in-mem scaffold → handler short-circuits with an
    // internal error from `require_db()`. The point is the route resolves; it
    // is not 404. CSRF middleware lets it through because no session cookie
    // is set, and the handler is reached.
    assert_ne!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "route must be mounted when magic_link is enabled"
    );
}

#[tokio::test]
async fn magic_link_is_mounted_by_default() {
    // Default config ships magic_link in enabled_methods.
    let app = build_test_app(|_| {});
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/auth/magic-link/verify?token=anything")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn magic_link_code_is_404_when_disabled() {
    let app = build_test_app(|cfg| {
        cfg.auth.enabled_methods = vec!["github_oauth".into()];
    });
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/magic-link/code")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("code=4KP9RT"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn magic_link_code_is_mounted_when_enabled() {
    let app = build_test_app(|cfg| {
        cfg.auth.enabled_methods = vec!["github_oauth".into(), "magic_link".into()];
    });
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/magic-link/code")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("code=4KP9RT"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(resp.status(), StatusCode::NOT_FOUND);
}
