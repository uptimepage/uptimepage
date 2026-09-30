//! Scope enforcement for API tokens: a read-only token can GET targets but is
//! denied writes with `INSUFFICIENT_SCOPE`; a full-access token can do both.
//!
//! Live-PG ignored: needs a Postgres pool. The scope check is in the
//! `Authorized<R>` extractor, in front of the unchanged per-org-id store.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{build_saas_router_with_pg_targets, make_user, unique_slug, with_session};
use serde_json::json;
use tower::ServiceExt;
use uptimepage::auth::api_tokens;
use uptimepage::auth::scope::ScopeSet;
use uptimepage::domain::{NewStatusPage, WriteSource};
use uptimepage::storage::{PgStatusPageStore, StatusPageStore, create_org_with_owner};

const PREFIX_LEN: usize = 16;

fn target_body() -> String {
    json!({
        "name": "scoped",
        "check": {
            "type": "http",
            "url": "https://example.com/",
            "method": "GET",
            "timeout": 5000,
            "follow_redirects": false,
            "max_redirects": 0,
            "expected_status": { "kind": "exact", "value": 200 },
            "headers": {},
            "verify_tls": true
        },
        "interval": 180,
        "enabled": true,
        "tags": []
    })
    .to_string()
}

/// Mint a token, then set its scopes directly (scoped creation is covered
/// elsewhere; here we just need a token carrying an exact set).
async fn token_with_scopes(
    pool: &sqlx::PgPool,
    user: uptimepage::domain::UserId,
    name: &str,
    scopes_json: &str,
) -> String {
    let t = api_tokens::create(
        pool,
        user,
        name,
        &ScopeSet::full_access(),
        None,
        None,
        PREFIX_LEN,
        1000,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE api_tokens SET scopes = $1::jsonb WHERE id = $2")
        .bind(scopes_json)
        .bind(t.id)
        .execute(pool)
        .await
        .unwrap();
    t.token
}

fn json_id(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body).unwrap()["id"]
        .as_str()
        .expect("id in response")
        .to_owned()
}

async fn send(
    router: &axum::Router,
    method: &str,
    path: &str,
    token: &str,
    org: &str,
    body: Option<String>,
) -> (StatusCode, String) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("x-uptimepage-org", org);
    let body = match body {
        Some(j) => {
            builder = builder.header("content-type", "application/json");
            Body::from(j)
        }
        None => Body::empty(),
    };
    let resp = router
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// POST without a Bearer header — auth comes from a session stamped on the
/// router. Token-management endpoints are browser-only, so this is the path
/// the UI takes; `CurrentOrg` resolves from the session's active org.
async fn send_as_browser(
    router: &axum::Router,
    method: &str,
    path: &str,
    body: Option<String>,
) -> (StatusCode, String) {
    let mut builder = Request::builder().method(method).uri(path);
    let body = match body {
        Some(j) => {
            builder = builder.header("content-type", "application/json");
            Body::from(j)
        }
        None => Body::empty(),
    };
    let resp = router
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn read_only_token_gets_but_cannot_write_targets() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };

    let user = make_user(&pool, "scope").await;
    let slug = unique_slug("scope");
    create_org_with_owner(&pool, user, &slug, "Scope")
        .await
        .unwrap()
        .expect("org");

    // Read-only token: create (defaults to full_access) then narrow the scopes
    // directly — the scoped-creation API arrives in a later phase.
    let ro = api_tokens::create(
        &pool,
        user,
        "ro",
        &ScopeSet::full_access(),
        None,
        None,
        PREFIX_LEN,
        1000,
    )
    .await
    .unwrap();
    sqlx::query(r#"UPDATE api_tokens SET scopes = '["targets:read"]'::jsonb WHERE id = $1"#)
        .bind(ro.id)
        .execute(&pool)
        .await
        .unwrap();

    // Full-access token (default scopes).
    let fa = api_tokens::create(
        &pool,
        user,
        "fa",
        &ScopeSet::full_access(),
        None,
        None,
        PREFIX_LEN,
        1000,
    )
    .await
    .unwrap();

    // Token scoped to a DIFFERENT resource — proves the read gate discriminates
    // by resource, not just read-vs-write.
    let wrong = api_tokens::create(
        &pool,
        user,
        "wrong",
        &ScopeSet::full_access(),
        None,
        None,
        PREFIX_LEN,
        1000,
    )
    .await
    .unwrap();
    sqlx::query(r#"UPDATE api_tokens SET scopes = '["channels:read"]'::jsonb WHERE id = $1"#)
        .bind(wrong.id)
        .execute(&pool)
        .await
        .unwrap();

    let router = build_saas_router_with_pg_targets(pool.clone()).await;

    // Read-only token: listing is allowed.
    let (status, _) = send(&router, "GET", "/api/v1/targets", &ro.token, &slug, None).await;
    assert_eq!(status, StatusCode::OK, "read-only token must GET targets");

    // Channels-only token: cannot even GET targets.
    let (status, body) = send(&router, "GET", "/api/v1/targets", &wrong.token, &slug, None).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "channels-only token must not read targets"
    );
    assert!(
        body.contains("INSUFFICIENT_SCOPE"),
        "expected INSUFFICIENT_SCOPE, got: {body}"
    );

    // The targets-only token also lacks channels:read — cross-resource gate.
    let (status, body) = send(
        &router,
        "GET",
        "/api/v1/notification-channels",
        &ro.token,
        &slug,
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "targets-only token must not read channels"
    );
    assert!(
        body.contains("INSUFFICIENT_SCOPE"),
        "expected INSUFFICIENT_SCOPE, got: {body}"
    );

    // Read-only token: creating is denied with INSUFFICIENT_SCOPE.
    let (status, body) = send(
        &router,
        "POST",
        "/api/v1/targets",
        &ro.token,
        &slug,
        Some(target_body()),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "read-only token must not create targets"
    );
    assert!(
        body.contains("INSUFFICIENT_SCOPE"),
        "expected INSUFFICIENT_SCOPE, got: {body}"
    );

    // Full-access token: creating succeeds.
    let (status, body) = send(
        &router,
        "POST",
        "/api/v1/targets",
        &fa.token,
        &slug,
        Some(target_body()),
    )
    .await;
    assert!(
        status.is_success(),
        "full-access token must create targets, got {status}: {body}"
    );
}

/// Org-bound token (P2): pinned to org A, it operates on A with or without the
/// header, and is rejected when the header names a *different* member org.
#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn org_bound_token_is_pinned_to_its_org() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };

    let user = make_user(&pool, "bind").await;
    let slug_a = unique_slug("bind-a");
    let slug_b = unique_slug("bind-b");
    let org_a = create_org_with_owner(&pool, user, &slug_a, "A")
        .await
        .unwrap()
        .expect("org A");
    create_org_with_owner(&pool, user, &slug_b, "B")
        .await
        .unwrap()
        .expect("org B");

    // Full-access token bound to org A.
    let tok = api_tokens::create(
        &pool,
        user,
        "bound",
        &ScopeSet::full_access(),
        None,
        None,
        PREFIX_LEN,
        1000,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE api_tokens SET org_id = $1 WHERE id = $2")
        .bind(org_a.id.0)
        .bind(tok.id)
        .execute(&pool)
        .await
        .unwrap();

    let router = build_saas_router_with_pg_targets(pool.clone()).await;

    // Header naming the bound org → allowed.
    let (status, _) = send(&router, "GET", "/api/v1/targets", &tok.token, &slug_a, None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "bound token must act on its own org"
    );

    // Header naming a different member org → 403 ORG_HEADER_MISMATCH.
    let (status, body) = send(&router, "GET", "/api/v1/targets", &tok.token, &slug_b, None).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "bound token must not address a different org"
    );
    assert!(
        body.contains("ORG_HEADER_MISMATCH"),
        "expected ORG_HEADER_MISMATCH, got: {body}"
    );

    // Seed a target into org A (via the binding) so the no-header read below
    // can prove it resolved to A *specifically*, not just "some valid org".
    let (status, _) = send(
        &router,
        "POST",
        "/api/v1/targets",
        &tok.token,
        &slug_a,
        Some(target_body()),
    )
    .await;
    assert!(
        status.is_success(),
        "bound token must create in its own org"
    );

    // No header → the binding implies org A: the read must return A's target.
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/targets")
                .header("authorization", format!("Bearer {}", tok.token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "bound token with no header must imply its org"
    );
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&body);
    assert!(
        body.contains("scoped"),
        "no-header read must resolve to org A (its seeded target), got: {body}"
    );
}

/// status_page scope-gating (#1): a `status_page:read` token reads settings; a
/// token without it (targets-only) is denied `INSUFFICIENT_SCOPE`.
#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn status_page_settings_require_status_page_scope() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };

    let user = make_user(&pool, "sp").await;
    let slug = unique_slug("sp");
    let org = create_org_with_owner(&pool, user, &slug, "SP")
        .await
        .unwrap()
        .expect("org");

    // A page to gate on — scope checks sit in front of the per-page store.
    let page = PgStatusPageStore::new(pool.clone())
        .create(
            org.id,
            NewStatusPage {
                slug: unique_slug("sp-page"),
                name: "SP".into(),
                enabled: true,
            },
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .expect("create page")
        .expect("within page cap");
    let path = format!("/api/v1/status-pages/{}", page.id);

    let reader = api_tokens::create(
        &pool,
        user,
        "sp-read",
        &ScopeSet::full_access(),
        None,
        None,
        PREFIX_LEN,
        1000,
    )
    .await
    .unwrap();
    sqlx::query(r#"UPDATE api_tokens SET scopes = '["status_page:read"]'::jsonb WHERE id = $1"#)
        .bind(reader.id)
        .execute(&pool)
        .await
        .unwrap();

    let wrong = api_tokens::create(
        &pool,
        user,
        "sp-wrong",
        &ScopeSet::full_access(),
        None,
        None,
        PREFIX_LEN,
        1000,
    )
    .await
    .unwrap();
    sqlx::query(r#"UPDATE api_tokens SET scopes = '["targets:read"]'::jsonb WHERE id = $1"#)
        .bind(wrong.id)
        .execute(&pool)
        .await
        .unwrap();

    let router = build_saas_router_with_pg_targets(pool.clone()).await;

    let (status, body) = send(&router, "GET", &path, &reader.token, &slug, None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "status_page:read token must read settings, got: {body}"
    );

    let (status, body) = send(&router, "GET", &path, &wrong.token, &slug, None).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "token without status_page:read must be denied"
    );
    assert!(
        body.contains("INSUFFICIENT_SCOPE"),
        "expected INSUFFICIENT_SCOPE, got: {body}"
    );

    // Write gate: a read-only status-page token cannot PATCH settings.
    let patch = r#"{"enabled":false}"#.to_string();
    let (status, body) = send(
        &router,
        "PATCH",
        &path,
        &reader.token,
        &slug,
        Some(patch.clone()),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "status_page:read must not write settings"
    );
    assert!(
        body.contains("INSUFFICIENT_SCOPE"),
        "expected INSUFFICIENT_SCOPE on write, got: {body}"
    );

    // A status_page:write token may PATCH (write ⇒ read).
    let writer = api_tokens::create(
        &pool,
        user,
        "sp-write",
        &ScopeSet::full_access(),
        None,
        None,
        PREFIX_LEN,
        1000,
    )
    .await
    .unwrap();
    sqlx::query(r#"UPDATE api_tokens SET scopes = '["status_page:write"]'::jsonb WHERE id = $1"#)
        .bind(writer.id)
        .execute(&pool)
        .await
        .unwrap();
    let (status, body) = send(&router, "PATCH", &path, &writer.token, &slug, Some(patch)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "status_page:write must update settings, got: {body}"
    );

    // Destructive logo removal needs status_page:delete, not :write.
    let logo = format!("/api/v1/status-pages/{}/logo", page.id);
    let (status, body) = send(&router, "DELETE", &logo, &writer.token, &slug, None).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "write must not delete logo: {body}"
    );
    assert!(body.contains("INSUFFICIENT_SCOPE"), "{body}");
    let deleter = token_with_scopes(&pool, user, "sp-del", r#"["status_page:delete"]"#).await;
    let (status, body) = send(&router, "DELETE", &logo, &deleter, &slug, None).await;
    assert!(
        status.is_success(),
        "status_page:delete must remove logo, got {status}: {body}"
    );
}

/// P3 creation API: scopes required + validated, expiry capped, org binding
/// membership-checked, and a valid request echoes scopes/org/expiry back.
#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn token_creation_validates_scopes_org_and_expiry() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };

    let user = make_user(&pool, "create").await;
    sqlx::query("UPDATE users SET email_verified_at = now() WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
    let slug = unique_slug("create");
    let org = create_org_with_owner(&pool, user, &slug, "Create")
        .await
        .unwrap()
        .expect("org")
        .id;

    // Token management is browser-only; drive create through a stamped session.
    let router = build_saas_router_with_pg_targets(pool.clone()).await;
    let router = with_session(router, user, Some(org), Some("create-sess"));
    let url = "/api/v1/me/api-tokens";

    // Empty scopes → 422.
    let (status, body) = send_as_browser(
        &router,
        "POST",
        url,
        Some(r#"{"name":"x","scopes":[]}"#.into()),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body.contains("INVALID_SCOPES"), "{body}");

    // Unknown scope → 422.
    let (status, body) = send_as_browser(
        &router,
        "POST",
        url,
        Some(r#"{"name":"x","scopes":["bogus:read"]}"#.into()),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body.contains("INVALID_SCOPES"), "{body}");

    // Expiry past the cap → 422.
    let (status, body) = send_as_browser(
        &router,
        "POST",
        url,
        Some(r#"{"name":"x","scopes":["targets:read"],"expires_in_days":400}"#.into()),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body.contains("INVALID_EXPIRY"), "{body}");

    // org_slug the caller is not a member of → 403.
    let other = make_user(&pool, "other").await;
    let other_slug = unique_slug("other");
    create_org_with_owner(&pool, other, &other_slug, "Other")
        .await
        .unwrap()
        .expect("other org");
    let (status, body) = send_as_browser(
        &router,
        "POST",
        url,
        Some(format!(
            r#"{{"name":"x","scopes":["targets:read"],"org_slug":"{other_slug}"}}"#
        )),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // Valid scoped + org-bound + expiring token → 201, echoes the inputs back.
    let (status, body) = send_as_browser(
        &router,
        "POST",
        url,
        Some(format!(
            r#"{{"name":"scoped","scopes":["targets:read"],"org_slug":"{slug}","expires_in_days":30}}"#
        )),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(body.contains("targets:read"), "{body}");
    assert!(body.contains(&format!("\"org\":\"{slug}\"")), "{body}");
    assert!(body.contains("expires_at"), "{body}");

    // The issued token actually carries what was stored: targets:read works,
    // a write is denied — proving scopes/binding persisted, not just echoed.
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let scoped = created["token"].as_str().expect("token in create response");
    let (status, _) = send(&router, "GET", "/api/v1/targets", scoped, &slug, None).await;
    assert_eq!(status, StatusCode::OK, "issued read token must GET targets");
    let (status, body) = send(
        &router,
        "POST",
        "/api/v1/targets",
        scoped,
        &slug,
        Some(target_body()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("INSUFFICIENT_SCOPE"), "{body}");

    // Zero-day expiry → 422.
    let (status, body) = send_as_browser(
        &router,
        "POST",
        url,
        Some(r#"{"name":"x","scopes":["targets:read"],"expires_in_days":0}"#.into()),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body.contains("INVALID_EXPIRY"), "{body}");

    // Unbound (no org_slug, no expiry) → 201.
    let (status, body) = send_as_browser(
        &router,
        "POST",
        url,
        Some(r#"{"name":"unbound","scopes":["channels:read"]}"#.into()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

/// Account administration is browser-session only: a Bearer API token — even
/// full-access — cannot mint tokens (escaping its scopes via an unrestricted
/// sibling), delete the account, or manage orgs. These endpoints read the
/// session cookie, never `AuthContext`, so a token has no session and 401s.
#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn api_token_cannot_reach_account_admin() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };

    let user = make_user(&pool, "noesc").await;
    sqlx::query("UPDATE users SET email_verified_at = now() WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
    let slug = unique_slug("noesc");
    let org = create_org_with_owner(&pool, user, &slug, "NoEsc")
        .await
        .unwrap()
        .expect("org")
        .id;
    let fa = api_tokens::create(
        &pool,
        user,
        "fa",
        &ScopeSet::full_access(),
        None,
        None,
        PREFIX_LEN,
        1000,
    )
    .await
    .unwrap();
    let router = build_saas_router_with_pg_targets(pool.clone()).await;

    // Every browser-only mutation must 401 for a Bearer token, whatever its
    // scopes — (method, path, body) per endpoint class.
    let cases: [(&str, String, Option<String>); 4] = [
        (
            "POST",
            "/api/v1/me/api-tokens".into(),
            Some(r#"{"name":"sibling","scopes":["full_access"]}"#.into()),
        ),
        ("DELETE", "/api/v1/me".into(), None),
        (
            "POST",
            "/api/v1/orgs".into(),
            Some(r#"{"slug":"x","name":"X"}"#.into()),
        ),
        ("DELETE", format!("/api/v1/orgs/{org}"), None),
    ];
    for (method, path, body) in cases {
        let (status, b) = send(&router, method, &path, &fa.token, &slug, body).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "token must not reach {method} {path}: {b}"
        );
    }
}

/// `targets:delete` and `targets:execute` are independent of `targets:write`:
/// a write-only token can neither delete nor run a check.
#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn targets_delete_and_execute_are_separate_from_write() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };

    let user = make_user(&pool, "split").await;
    let slug = unique_slug("split");
    create_org_with_owner(&pool, user, &slug, "Split")
        .await
        .unwrap()
        .expect("org");
    let fa = api_tokens::create(
        &pool,
        user,
        "fa",
        &ScopeSet::full_access(),
        None,
        None,
        PREFIX_LEN,
        1000,
    )
    .await
    .unwrap();
    let router = build_saas_router_with_pg_targets(pool.clone()).await;

    let (st, b) = send(
        &router,
        "POST",
        "/api/v1/targets",
        &fa.token,
        &slug,
        Some(target_body()),
    )
    .await;
    assert!(st.is_success(), "{b}");
    let id1 = json_id(&b);
    let (st, b) = send(
        &router,
        "POST",
        "/api/v1/targets",
        &fa.token,
        &slug,
        Some(target_body()),
    )
    .await;
    assert!(st.is_success(), "{b}");
    let id2 = json_id(&b);

    let write = token_with_scopes(&pool, user, "w", r#"["targets:write"]"#).await;
    let (st, b) = send(
        &router,
        "DELETE",
        &format!("/api/v1/targets/{id1}"),
        &write,
        &slug,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "write must not delete: {b}");
    assert!(b.contains("INSUFFICIENT_SCOPE"), "{b}");
    let (st, b) = send(
        &router,
        "POST",
        &format!("/api/v1/targets/{id1}/check-now"),
        &write,
        &slug,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "write must not run a check: {b}");
    assert!(b.contains("INSUFFICIENT_SCOPE"), "{b}");

    let del = token_with_scopes(&pool, user, "d", r#"["targets:delete"]"#).await;
    let (st, b) = send(
        &router,
        "DELETE",
        &format!("/api/v1/targets/{id1}"),
        &del,
        &slug,
        None,
    )
    .await;
    assert!(
        st.is_success(),
        "delete-scoped token must delete, got {st}: {b}"
    );

    let exec = token_with_scopes(&pool, user, "x", r#"["targets:execute"]"#).await;
    let (st, b) = send(
        &router,
        "POST",
        &format!("/api/v1/targets/{id2}/check-now"),
        &exec,
        &slug,
        None,
    )
    .await;
    assert_ne!(
        st,
        StatusCode::FORBIDDEN,
        "execute token must pass the scope gate: {b}"
    );
    assert!(!b.contains("INSUFFICIENT_SCOPE"), "{b}");

    // channels:write alone cannot send a test — channels:execute is required.
    let chan = json!({
        "name": "ops",
        "config": { "type": "slack", "webhook_url": "https://hooks.slack.com/services/T/B/X" },
        "enabled": true
    })
    .to_string();
    let (st, b) = send(
        &router,
        "POST",
        "/api/v1/notification-channels",
        &fa.token,
        &slug,
        Some(chan),
    )
    .await;
    assert!(st.is_success(), "{b}");
    let cid = json_id(&b);
    let cwrite = token_with_scopes(&pool, user, "cw", r#"["channels:write"]"#).await;
    let (st, b) = send(
        &router,
        "POST",
        &format!("/api/v1/notification-channels/{cid}/test"),
        &cwrite,
        &slug,
        None,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::FORBIDDEN,
        "channels:write must not send a test: {b}"
    );
    assert!(b.contains("INSUFFICIENT_SCOPE"), "{b}");

    // Choosing whom a test pings is a write, so channels:execute alone fires
    // only the stored ping.
    let cexec = token_with_scopes(&pool, user, "ce", r#"["channels:execute"]"#).await;
    let edit = json!({ "config": { "type": "slack_app", "mention": "@here" } }).to_string();
    let (st, b) = send(
        &router,
        "POST",
        &format!("/api/v1/notification-channels/{cid}/test"),
        &cexec,
        &slug,
        Some(edit),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::FORBIDDEN,
        "channels:execute must not test an unsaved ping: {b}"
    );
    assert!(b.contains("INSUFFICIENT_SCOPE"), "{b}");

    // channels:write cannot delete a channel; channels:delete can.
    let path = format!("/api/v1/notification-channels/{cid}");
    let (st, b) = send(&router, "DELETE", &path, &cwrite, &slug, None).await;
    assert_eq!(
        st,
        StatusCode::FORBIDDEN,
        "channels:write must not delete: {b}"
    );
    assert!(b.contains("INSUFFICIENT_SCOPE"), "{b}");
    let cdel = token_with_scopes(&pool, user, "cd", r#"["channels:delete"]"#).await;
    let (st, b) = send(&router, "DELETE", &path, &cdel, &slug, None).await;
    assert!(
        st.is_success(),
        "channels:delete must delete, got {st}: {b}"
    );
}

#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn token_on_org_mutation_gets_session_required() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };

    let user = make_user(&pool, "session-req").await;
    let slug = unique_slug("sreq");
    let org = create_org_with_owner(&pool, user, &slug, "SReq")
        .await
        .unwrap()
        .expect("org");
    let token = token_with_scopes(&pool, user, "fa", r#"["full_access"]"#).await;

    let router = build_saas_router_with_pg_targets(pool.clone()).await;
    let path = format!("/api/v1/orgs/{}", org.id.0);
    let (status, body) = send(
        &router,
        "PATCH",
        &path,
        &token,
        &slug,
        Some(json!({ "name": "Renamed" }).to_string()),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert!(body.contains("SESSION_REQUIRED"), "{body}");
}
