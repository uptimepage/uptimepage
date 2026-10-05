//! Signing in must not cancel a pending account deletion. The session a
//! soft-deleted user gets opens exactly two doors — the restore page and the
//! restore call — and nothing else.

use crate::common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt;
use uptimepage::auth::account;
use uptimepage::auth::session as session_store;
use uptimepage::config::AppConfig;
use uptimepage::storage::create_org_with_owner;

use crate::common::MIGRATOR;

/// The extractor under test reads the cookie, so an injected `Session` would
/// prove nothing.
async fn session_cookie(pool: &sqlx::PgPool, user: uptimepage::domain::UserId) -> String {
    let cfg = AppConfig::load().expect("config");
    let created = session_store::create(pool, &cfg.auth.session, user, None, None, None)
        .await
        .expect("session");
    format!("{}={}", cfg.auth.session.cookie_name, created.cookie_token)
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn signing_in_does_not_restore_but_the_restore_call_does() {
    let Some((db, name)) = common::fresh_test_db("restore_flow").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let user = common::make_user(&pool, "leaver").await;
    let org = create_org_with_owner(&pool, user, &common::unique_slug("a"), "Co")
        .await
        .unwrap()
        .unwrap();
    account::request_deletion(&pool, user, 30)
        .await
        .expect("deletion");

    let cookie = session_cookie(&pool, user).await;
    let (deleted_at,): (Option<chrono::DateTime<chrono::Utc>>,) =
        sqlx::query_as("SELECT deleted_at FROM users WHERE id = $1")
            .bind(user.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(deleted_at.is_some(), "a session must not undo the deletion");

    let (app, _) = common::build_test_app_with_pg_store_anon(pool.clone(), |_| {}).await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/account/restore")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let html = String::from_utf8_lossy(&body);
    assert!(
        html.contains("scheduled for permanent deletion"),
        "interstitial states what is pending"
    );

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/targets")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "a pending-deletion session is not a working session"
    );

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/me/restore")
                .header(header::COOKIE, &cookie)
                .header("x-requested-with", "uptimepage")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let (deleted_at,): (Option<chrono::DateTime<chrono::Utc>>,) =
        sqlx::query_as("SELECT deleted_at FROM users WHERE id = $1")
            .bind(user.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(deleted_at.is_none(), "restore clears the deletion");
    let (org_deleted,): (Option<chrono::DateTime<chrono::Utc>>,) =
        sqlx::query_as("SELECT deleted_at FROM organizations WHERE id = $1")
            .bind(org.id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(org_deleted.is_none(), "the org tombstone is lifted too");

    // Active again ⇒ no pending deletion ⇒ the restore door is shut.
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/me/restore")
                .header(header::COOKIE, &cookie)
                .header("x-requested-with", "uptimepage")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    common::drop_test_db(&name).await;
}

/// A user who signs in on two devices during the grace window and restores from
/// one must not be left with a permanently broken session on the other: those
/// sessions were minted with no active org (theirs was tombstoned), and
/// `active_org_id` is fixed at insert time.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn restore_repairs_sessions_minted_on_other_devices() {
    let Some((db, name)) = common::fresh_test_db("restore_devices").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let user = common::make_user(&pool, "twodevice").await;
    let org = create_org_with_owner(&pool, user, &common::unique_slug("a"), "Co")
        .await
        .unwrap()
        .unwrap();
    account::request_deletion(&pool, user, 30)
        .await
        .expect("deletion");

    let phone = session_cookie(&pool, user).await;
    let laptop = session_cookie(&pool, user).await;
    let (app, _) = common::build_test_app_with_pg_store_anon(pool.clone(), |_| {}).await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/me/restore")
                .header(header::COOKIE, &laptop)
                .header("x-requested-with", "uptimepage")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // The phone never called restore, but its session must still resolve an org.
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/targets")
                .header(header::COOKIE, &phone)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a session from another device must work after the restore"
    );

    let (orphans,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM sessions WHERE user_id = $1 AND active_org_id IS NULL",
    )
    .bind(user.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(orphans, 0, "every live session adopted the restored org");
    let _ = org;

    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn restore_page_is_closed_to_everyone_else() {
    let Some((db, name)) = common::fresh_test_db("restore_shut").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let user = common::make_user(&pool, "active").await;
    create_org_with_owner(&pool, user, &common::unique_slug("a"), "Co")
        .await
        .unwrap()
        .unwrap();
    let cookie = session_cookie(&pool, user).await;
    let (app, _) = common::build_test_app_with_pg_store_anon(pool.clone(), |_| {}).await;

    // A page, so both refusals redirect to the login form. Rendering
    // AppError's JSON envelope into a browser tab would also be "not OK".
    for req in [
        Request::builder()
            .uri("/account/restore")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
        Request::builder()
            .uri("/account/restore")
            .body(Body::empty())
            .unwrap(),
    ] {
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::SEE_OTHER,
            "no pending deletion must redirect, not render an error body"
        );
        assert_eq!(
            resp.headers().get(header::LOCATION).unwrap(),
            "/login",
            "redirect target"
        );
    }

    // Public by design: the signed-out end of the flow carries nothing but the
    // date the caller just set.
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/account/deleted")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    common::drop_test_db(&name).await;
}
