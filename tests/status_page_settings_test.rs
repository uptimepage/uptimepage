//! HTTP-level tests for the operator status-page settings endpoints.
//!
//! Regression cover for #25 (a rejected save must name the offending field so
//! the form can highlight it) and #26 (a normal save — including the folded-in
//! logo step — actually succeeds). Walks the real router via `oneshot`, so the
//! extractor wiring, status codes and JSON error contract are all exercised.
//!
//! Branding lives on a page under `PATCH /api/v1/status-pages/{id}` (the
//! `branding` sub-object); the logo has its own `/logo` endpoints.
//!
//! Live-PG ignored: the owner gate needs a Postgres pool. Run with
//! `DATABASE_URL` set (see CLAUDE.md) — unset is a no-op pass.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    body_json, build_saas_router_with_pg_cfg, json_request, make_user, unique_slug, with_session,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uptimepage::domain::{NewStatusPage, WriteSource};
use uptimepage::storage::{PgStatusPageStore, StatusPageStore, create_org_with_owner};

/// 1×1 RGBA PNG — `image::guess_format` accepts it and it is within every
/// dimension/size bound, so `upload_logo` stores it verbatim.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

async fn read(resp: axum::http::Response<Body>) -> (StatusCode, Value) {
    let st = resp.status();
    (st, body_json(resp).await)
}

/// Owner-of-a-fresh-org router + a freshly-created page id. Creating the org
/// through the API grants the `owner` membership the settings routes require;
/// the page is the resource branding/logo edits target.
async fn owner_page(pool: PgPool) -> (axum::Router, String) {
    let user = make_user(&pool, "sp").await;
    let slug = unique_slug("sp");
    let org = create_org_with_owner(&pool, user, &slug, "SP Co")
        .await
        .unwrap()
        .expect("org");
    // Orgs are no longer auto-seeded a page; create the one these edits target.
    PgStatusPageStore::new(pool.clone())
        .create(
            org.id,
            NewStatusPage {
                slug: unique_slug("sp-page"),
                name: "SP Page".into(),
                enabled: true,
            },
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .expect("create page")
        .expect("slug free");
    // Logos now persist in Postgres (`page_assets`), not on disk; the router's
    // Pg pool is all the upload path needs.
    let router = build_saas_router_with_pg_cfg(pool, |_| {}).await;
    // Active org on the session is what the per-page routes resolve `org` from.
    let router = with_session(router, user, Some(org.id), Some(slug.as_str()));

    let (st, body) = read(
        router
            .clone()
            .oneshot(
                Request::get("/api/v1/status-pages")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "list pages: {body}");
    let id = body[0]["id"].as_str().expect("the created page").to_owned();
    (router, id)
}

fn patch(id: &str, payload: Value) -> Request<Body> {
    json_request("PATCH", &format!("/api/v1/status-pages/{id}"), payload)
}

const BOUNDARY: &str = "X-SP-BOUNDARY";

/// A `multipart/form-data` body with a single `file` part holding `bytes`.
fn logo_request(id: &str, bytes: &[u8]) -> Request<Body> {
    let mut body = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; \
         filename=\"l.png\"\r\nContent-Type: image/png\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    Request::post(format!("/api/v1/status-pages/{id}/logo"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body))
        .unwrap()
}

// ── #26: a normal save succeeds and round-trips ─────────────────────────────

#[tokio::test]
#[ignore]
async fn valid_save_persists_and_round_trips() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (router, id) = owner_page(pool).await;

    let (st, body) = read(
        router
            .clone()
            .oneshot(patch(
                &id,
                json!({
                    "enabled": true,
                    "branding": {
                        "public_display_name": "Acme Public",
                        "public_about": "All good.",
                        "public_brand_color": "#1A2B3C",
                        "public_show_powered_by": false,
                        "public_hide_from_search": true,
                        "public_website_url": "https://acme.example"
                    }
                }),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "branding patch: {body}");

    let (st, body) = read(
        router
            .oneshot(
                Request::get(format!("/api/v1/status-pages/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["enabled"], true);
    assert_eq!(body["public_display_name"], "Acme Public");
    assert_eq!(body["public_brand_color"], "#1a2b3c"); // lower-cased on write
    assert_eq!(body["public_hide_from_search"], true);
    assert_eq!(body["public_website_url"], "https://acme.example");
    // The powered-by toggle is pinned on: a false override is ignored and the
    // raw value clears to inherit the default.
    assert_eq!(body["show_powered_by"], true);
    assert!(body["public_show_powered_by"].is_null());
}

/// The form's default state (no display name / about set) must not read as a
/// spurious rejection — the heart of "doesn't work at all" in #26.
#[tokio::test]
#[ignore]
async fn blank_optionals_save_as_cleared_not_rejected() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (router, id) = owner_page(pool).await;
    let (st, body) = read(
        router
            .oneshot(patch(
                &id,
                json!({
                    "enabled": false,
                    "branding": {
                        "public_display_name": "",
                        "public_about": "",
                        "public_brand_color": "#3b82f6",
                        "public_show_powered_by": true
                    }
                }),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "blank optionals: {body}");
}

// ── #25: a rejected save names the offending field ──────────────────────────

async fn assert_field_rejection(branding: Value, expect_field: &str) {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (router, id) = owner_page(pool).await;
    let (st, body) = read(
        router
            .oneshot(patch(&id, json!({ "branding": branding })))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["error"]["code"], "BRANDING_INVALID");
    assert_eq!(
        body["error"]["field"], expect_field,
        "the form highlights `error.field`; got {body}"
    );
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()),
        "a human message must accompany the field"
    );
}

#[tokio::test]
#[ignore]
async fn bad_website_url_points_at_that_field() {
    assert_field_rejection(
        json!({ "public_brand_color": "#3b82f6", "public_website_url": "javascript:alert(1)" }),
        "public_website_url",
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn bad_brand_color_points_at_that_field() {
    assert_field_rejection(
        json!({ "public_brand_color": "not-a-color" }),
        "public_brand_color",
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn over_long_about_points_at_that_field() {
    assert_field_rejection(
        json!({ "public_brand_color": "#3b82f6", "public_about": "x".repeat(501) }),
        "public_about",
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn over_long_display_name_points_at_that_field() {
    assert_field_rejection(
        json!({ "public_brand_color": "#3b82f6", "public_display_name": "y".repeat(81) }),
        "public_display_name",
    )
    .await;
}

// ── #26: the logo step and the branding save are independent ─────────────────

#[tokio::test]
#[ignore]
async fn logo_upload_then_branding_patch_keeps_logo() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (router, id) = owner_page(pool).await;

    let (st, body) = read(
        router
            .clone()
            .oneshot(logo_request(&id, TINY_PNG))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "logo upload: {body}");
    assert!(body["logo_url"].as_str().is_some_and(|u| !u.is_empty()));

    // The branding PATCH must not disturb the just-uploaded logo (the editor
    // does logo-then-PATCH on a single Save).
    let (st, _) = read(
        router
            .clone()
            .oneshot(patch(
                &id,
                json!({ "enabled": true, "branding": { "public_brand_color": "#3b82f6" } }),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let (st, body) = read(
        router
            .oneshot(
                Request::get(format!("/api/v1/status-pages/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        body["logo_url"].as_str().is_some_and(|u| !u.is_empty()),
        "logo survived the branding save: {body}"
    );
}

/// An over-cap logo must come back as a clean `413 LOGO_TOO_LARGE`, not the
/// opaque `LOGO_MISSING` the body layer produced when its limit was only
/// 64 KiB over `max_logo_size_bytes` (the bug behind the operator-reported
/// "Error parsing multipart/form-data request").
#[tokio::test]
#[ignore]
async fn oversize_logo_is_clean_413_not_opaque() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (router, id) = owner_page(pool).await;

    // 1.2 MiB: over the 1 MiB cap but under the route body limit, so it
    // reaches the handler's own size check instead of being truncated.
    let mut oversized = TINY_PNG.to_vec();
    oversized.resize(1_258_291, 0);

    let (st, body) = read(router.oneshot(logo_request(&id, &oversized)).await.unwrap()).await;
    assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE, "body: {body}");
    assert_eq!(body["error"]["code"], "LOGO_TOO_LARGE");
}
