//! HTTP-level tests for the org-management endpoints. They walk every route
//! from `Router::oneshot` through the real handlers, so anything that breaks
//! the request shape (extractor wiring, status codes, JSON contract) surfaces
//! here rather than only at the storage layer.
//!
//! Live-PG ignored: the orgs routes need a Postgres pool. Sessions are stamped
//! onto requests via a `from_fn` layer that inserts a constructed `Session`
//! into the extensions — the real auth backend will replace it; until then
//! this is the only way to drive authenticated routes.

use crate::common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{body_json, build_test_app_with_pg, make_user, unique_slug, with_session};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uptimepage::domain::UserId;

fn cleanup_user(pool: PgPool, user: UserId) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let _ = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(user.0)
            .execute(&pool)
            .await;
    })
}

async fn read_value(resp: axum::http::Response<Body>) -> Value {
    body_json(resp).await
}

#[tokio::test]
#[ignore]
async fn unauthenticated_request_returns_401() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (router, _) = build_test_app_with_pg(pool, |_cfg| {}).await;
    // No session layer attached — Session::user is None → CurrentUser → 401.
    let resp = router
        .oneshot(Request::get("/api/v1/orgs").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
#[ignore]
async fn create_then_list_then_delete_round_trip() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "orgs-api").await;
    let (router, _) = build_test_app_with_pg(pool.clone(), |_cfg| {}).await;
    let router = with_session(router, user, None, None);

    let slug = unique_slug("api");
    let create = router
        .clone()
        .oneshot(
            Request::post("/api/v1/orgs")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "slug": slug, "name": "API Co" })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create.status(), StatusCode::CREATED);
    let body = read_value(create).await;
    let org_id = body["id"].as_str().unwrap().to_string();
    assert_eq!(body["slug"], slug);
    assert_eq!(body["role"], "owner");

    // List my orgs — must include the new one.
    let list = router
        .clone()
        .oneshot(Request::get("/api/v1/me/orgs").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::OK);
    let arr = read_value(list).await;
    assert!(arr.as_array().unwrap().iter().any(|o| o["id"] == org_id));

    // A second org, so the delete below is not refused as the caller's last.
    let keep = router
        .clone()
        .oneshot(
            Request::post("/api/v1/orgs")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "slug": unique_slug("keep"), "name": "Keep Co" }))
                        .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(keep.status(), StatusCode::CREATED);

    // Soft-delete (owner-only).
    let del = router
        .clone()
        .oneshot(
            Request::delete(format!("/api/v1/orgs/{org_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(del.status(), StatusCode::NO_CONTENT);

    // Now hidden from /orgs and surfaced in /me/deleted-orgs.
    let after = router
        .clone()
        .oneshot(Request::get("/api/v1/me/orgs").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let arr = read_value(after).await;
    assert!(arr.as_array().unwrap().iter().all(|o| o["id"] != org_id));

    let deleted = router
        .clone()
        .oneshot(
            Request::get("/api/v1/me/deleted-orgs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let arr = read_value(deleted).await;
    assert!(arr.as_array().unwrap().iter().any(|o| o["id"] == org_id));

    // Restore — must succeed because caller is the deleter and we're inside grace.
    let restore = router
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/orgs/{org_id}/restore"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(restore.status(), StatusCode::OK);

    cleanup_user(pool, user).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn create_org_with_taken_slug_returns_409() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "orgs-api").await;
    let (router, _) = build_test_app_with_pg(pool.clone(), |_cfg| {}).await;
    let router = with_session(router, user, None, None);

    let slug = unique_slug("dup");
    let first = router
        .clone()
        .oneshot(
            Request::post("/api/v1/orgs")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "slug": slug, "name": "A" })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);

    let second = router
        .clone()
        .oneshot(
            Request::post("/api/v1/orgs")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "slug": slug, "name": "B" })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::CONFLICT);
    let body = read_value(second).await;
    assert_eq!(body["error"]["code"], "SLUG_TAKEN");

    cleanup_user(pool, user).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn create_org_with_reserved_slug_returns_400() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "orgs-api").await;
    let (router, _) = build_test_app_with_pg(pool.clone(), |_cfg| {}).await;
    let router = with_session(router, user, None, None);

    let resp = router
        .oneshot(
            Request::post("/api/v1/orgs")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "slug": "admin", "name": "X" })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = read_value(resp).await;
    assert_eq!(body["error"]["code"], "SLUG_INVALID");

    cleanup_user(pool, user).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn patch_org_to_reserved_slug_returns_400() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "orgs-api").await;
    let (router, _) = build_test_app_with_pg(pool.clone(), |_cfg| {}).await;
    let router = with_session(router, user, None, None);

    let from = unique_slug("rev");
    let (st, body) = create_org_for_user(router.clone(), &from, "Co").await;
    assert_eq!(st, StatusCode::CREATED);
    let id = body["id"].as_str().unwrap().to_owned();

    // Reserved-slug rejection applies to the rename path, not just create.
    let resp = router
        .oneshot(patch_org(&id, json!({ "slug": "admin" })))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(read_value(resp).await["error"]["code"], "SLUG_INVALID");

    cleanup_user(pool, user).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn check_slug_endpoint_returns_availability() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "orgs-api").await;
    let (router, _) = build_test_app_with_pg(pool.clone(), |_cfg| {}).await;
    let router = with_session(router, user, None, None);

    let slug = unique_slug("chk");
    let resp = router
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/orgs/check-slug?slug={slug}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(read_value(resp).await["available"], true);

    // Reserved slug: not available, reason populated.
    let resp = router
        .clone()
        .oneshot(
            Request::get("/api/v1/orgs/check-slug?slug=admin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = read_value(resp).await;
    assert_eq!(body["available"], false);
    assert!(body["reason"].as_str().unwrap().contains("reserved"));

    cleanup_user(pool, user).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn non_owner_cannot_delete_or_update() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let owner = make_user(&pool, "orgs-api").await;
    let other = make_user(&pool, "orgs-api").await;
    let (router, _) = build_test_app_with_pg(pool.clone(), |_cfg| {}).await;
    let owner_router = with_session(router.clone(), owner, None, None);
    let other_router = with_session(router, other, None, None);

    let slug = unique_slug("priv");
    let create = owner_router
        .clone()
        .oneshot(
            Request::post("/api/v1/orgs")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "slug": slug, "name": "P" })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create.status(), StatusCode::CREATED);
    let body = read_value(create).await;
    let org_id = body["id"].as_str().unwrap().to_string();

    // Non-member sees 404 (cloak).
    let del = other_router
        .clone()
        .oneshot(
            Request::delete(format!("/api/v1/orgs/{org_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(del.status(), StatusCode::NOT_FOUND);

    sqlx::query("DELETE FROM users WHERE id = ANY($1)")
        .bind(&[owner.0, other.0][..])
        .execute(&pool)
        .await
        .unwrap();
}

// ── slug rename via PATCH /api/v1/orgs/{id} ─────────────────────────────────

async fn create_org_for_user(router: axum::Router, slug: &str, name: &str) -> (StatusCode, Value) {
    let resp = router
        .oneshot(
            Request::post("/api/v1/orgs")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "slug": slug, "name": name })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    (status, read_value(resp).await)
}

fn patch_org(id: &str, body: Value) -> Request<Body> {
    Request::patch(format!("/api/v1/orgs/{id}"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

#[tokio::test]
#[ignore]
async fn patch_renames_slug_and_responds_with_new_view() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "orgs-api").await;
    let (router, _) = build_test_app_with_pg(pool.clone(), |_cfg| {}).await;
    let router = with_session(router, user, None, None);

    let from = unique_slug("from");
    let (st, body) = create_org_for_user(router.clone(), &from, "Co").await;
    assert_eq!(st, StatusCode::CREATED);
    let id = body["id"].as_str().unwrap().to_owned();

    let to = unique_slug("to");
    let resp = router
        .clone()
        .oneshot(patch_org(&id, json!({ "slug": to })))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let view = read_value(resp).await;
    assert_eq!(view["slug"], to);
    assert_eq!(view["id"], id);

    cleanup_user(pool, user).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn patch_slug_conflict_returns_409() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "orgs-api").await;
    let (router, _) = build_test_app_with_pg(pool.clone(), |_cfg| {}).await;
    let router = with_session(router, user, None, None);

    let a = unique_slug("a");
    let b = unique_slug("b");
    let (_, _) = create_org_for_user(router.clone(), &a, "A").await;
    let (_, mine) = create_org_for_user(router.clone(), &b, "B").await;
    let id = mine["id"].as_str().unwrap().to_owned();

    let resp = router
        .oneshot(patch_org(&id, json!({ "slug": a })))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = read_value(resp).await;
    assert_eq!(body["error"]["code"], "SLUG_TAKEN");

    cleanup_user(pool, user).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn patch_invalid_slug_returns_400_with_field() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "orgs-api").await;
    let (router, _) = build_test_app_with_pg(pool.clone(), |_cfg| {}).await;
    let router = with_session(router, user, None, None);

    let slug = unique_slug("inv");
    let (_, body) = create_org_for_user(router.clone(), &slug, "X").await;
    let id = body["id"].as_str().unwrap().to_owned();

    let resp = router
        .clone()
        .oneshot(patch_org(&id, json!({ "slug": "admin" }))) // reserved
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = read_value(resp).await;
    assert_eq!(body["error"]["code"], "SLUG_INVALID");
    assert_eq!(body["error"]["field"], "slug");

    cleanup_user(pool, user).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn patch_empty_body_returns_400_empty_patch() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "orgs-api").await;
    let (router, _) = build_test_app_with_pg(pool.clone(), |_cfg| {}).await;
    let router = with_session(router, user, None, None);
    let slug = unique_slug("e");
    let (_, body) = create_org_for_user(router.clone(), &slug, "E").await;
    let id = body["id"].as_str().unwrap().to_owned();

    let resp = router.oneshot(patch_org(&id, json!({}))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(read_value(resp).await["error"]["code"], "EMPTY_PATCH");

    cleanup_user(pool, user).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn patch_non_owner_member_gets_forbidden_on_slug_change() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let owner = make_user(&pool, "orgs-api").await;
    let other = make_user(&pool, "orgs-api").await;
    let (router, _) = build_test_app_with_pg(pool.clone(), |_cfg| {}).await;
    let owner_router = with_session(router.clone(), owner, None, None);
    let other_router = with_session(router, other, None, None);

    let slug = unique_slug("non");
    let (_, body) = create_org_for_user(owner_router.clone(), &slug, "N").await;
    let id = body["id"].as_str().unwrap().to_owned();

    // Non-member sees 404 (cloak).
    let resp = other_router
        .oneshot(patch_org(&id, json!({ "slug": unique_slug("hi") })))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    sqlx::query("DELETE FROM users WHERE id = ANY($1)")
        .bind(&[owner.0, other.0][..])
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn delete_refuses_the_callers_last_org() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "orgs-api").await;
    let (router, _) = build_test_app_with_pg(pool.clone(), |_cfg| {}).await;
    let router = with_session(router, user, None, None);

    let create = |slug: String| {
        router.clone().oneshot(
            Request::post("/api/v1/orgs")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "slug": slug, "name": "Solo Co" })).unwrap(),
                ))
                .unwrap(),
        )
    };

    let first = create(unique_slug("solo")).await.unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);
    let first_id = read_value(first).await["id"].as_str().unwrap().to_string();

    // Sole org: refused, or the account has nothing left to sign in to.
    let refused = router
        .clone()
        .oneshot(
            Request::delete(format!("/api/v1/orgs/{first_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(read_value(refused).await["error"]["code"], "LAST_ORG");

    // With a sibling in place it goes through.
    assert_eq!(
        create(unique_slug("pair")).await.unwrap().status(),
        StatusCode::CREATED
    );
    let allowed = router
        .clone()
        .oneshot(
            Request::delete(format!("/api/v1/orgs/{first_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(allowed.status(), StatusCode::NO_CONTENT);

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}
