//! Extractor unit tests for `CurrentOrg`. Asserts the SaaS-strict shape: no
//! ambient org, no fallback. Missing session → 401. Missing `active_org_id`
//! on the session → 401. Live-DB branches (`is_active_member`) are exercised
//! by the integration suite.

mod common;

use axum::extract::FromRequestParts;
use axum::http::Request;
use uptimepage::error::AppError;
use uptimepage::request::auth::CurrentOrg;

#[tokio::test]
async fn no_session_returns_unauthorized() {
    let state = common::build_test_app_state(|_| {});
    let (mut parts, _) = Request::builder().uri("/").body(()).unwrap().into_parts();
    let err = CurrentOrg::from_request_parts(&mut parts, &state)
        .await
        .expect_err("missing session must reject");
    assert!(
        matches!(err, AppError::Unauthorized),
        "expected Unauthorized, got {err:?}"
    );
}

#[tokio::test]
async fn db_none_with_no_session_still_yields_unauthorized() {
    let state = common::build_test_app_state(|_| {});
    assert!(state.db.is_none(), "fixture must keep db = None");
    let (mut parts, _) = Request::builder().uri("/").body(()).unwrap().into_parts();
    let err = CurrentOrg::from_request_parts(&mut parts, &state)
        .await
        .expect_err("db=None + no session must reject");
    assert!(matches!(err, AppError::Unauthorized));
}
