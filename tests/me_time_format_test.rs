//! Integration coverage for `/api/v1/me/time-format`. Asserts the GET / PATCH
//! pair persists to `users.time_format`, emits the `sm_time_format` cookie, and
//! rejects unknown variants at deserialise time.
//!
//! Run via:
//!     docker compose -f compose.dev.yml up -d postgres
//!     DATABASE_URL=postgres://monitor:monitor@localhost:5432/monitor \
//!         cargo +1.95 nextest run --test it --run-ignored all -E 'test(/^me_time_format_test::/)'

use crate::common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    body_json, build_test_app_with_pg, drop_test_db, fresh_test_db, json_request, open_test_pool,
    with_session,
};
use tower::ServiceExt;
use uptimepage::domain::UserId;
use uuid::Uuid;

use crate::common::MIGRATOR;

async fn fresh_pg() -> Option<(String, String)> {
    fresh_test_db("me_time_format").await
}

async fn seed_user(pool: &sqlx::PgPool, email: &str) -> Uuid {
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO users (email, display_name, terms_version, privacy_version) \
         VALUES ($1, 'u', 'v1', 'v1') RETURNING id",
    )
    .bind(email)
    .fetch_one(pool)
    .await
    .unwrap();
    id
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn me_time_format_get_patch_round_trip() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let user = seed_user(&pool, "clock@example.test").await;
    let (router, _) = build_test_app_with_pg(pool.clone(), |_| {}).await;
    let router = with_session(router, UserId(user), None, None);

    // Default is 'auto' (the new column's DEFAULT).
    let resp = router
        .clone()
        .oneshot(
            Request::get("/api/v1/me/time-format")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["time_format"], "auto");

    let resp = router
        .clone()
        .oneshot({
            let mut req = json_request(
                "PATCH",
                "/api/v1/me/time-format",
                serde_json::json!({"time_format": "24h"}),
            );
            req.headers_mut()
                .insert("x-requested-with", "uptimepage".parse().unwrap());
            req
        })
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let set_cookies: Vec<&str> = resp
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    assert!(
        set_cookies
            .iter()
            .any(|c| c.starts_with("sm_time_format=24h")),
        "Set-Cookie sm_time_format=24h missing from {set_cookies:?}"
    );
    let body = body_json(resp).await;
    assert_eq!(body["time_format"], "24h");

    let (stored,): (String,) = sqlx::query_as("SELECT time_format FROM users WHERE id = $1")
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, "24h");

    pool.close().await;
    drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn me_time_format_patch_rejects_unknown_variant() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let user = seed_user(&pool, "clock-bad@example.test").await;
    let (router, _) = build_test_app_with_pg(pool.clone(), |_| {}).await;
    let router = with_session(router, UserId(user), None, None);

    let resp = router
        .oneshot({
            let mut req = json_request(
                "PATCH",
                "/api/v1/me/time-format",
                serde_json::json!({"time_format": "36h"}),
            );
            req.headers_mut()
                .insert("x-requested-with", "uptimepage".parse().unwrap());
            req
        })
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let (stored,): (String,) = sqlx::query_as("SELECT time_format FROM users WHERE id = $1")
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, "auto", "DB must be untouched on bad input");

    pool.close().await;
    drop_test_db(&name).await;
}
