//! Phase 4 (usage transparency) + Phase 5 (anti-abuse) coverage.
//!
//! Integration cases need Postgres (the `plans` table, `quota_events`,
//! memberships); they no-op when `DATABASE_URL` is unset, like the other
//! live-PG suites. They exist so a regression in the usage contract or the
//! abuse admission control fails CI, not production.

use crate::common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    body_json, build_test_app_with_pg_store, build_test_app_with_pg_store_anon, make_user,
    pg_pool_from_env, with_session,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uptimepage::domain::{OrgId, UserId};

fn http_target(name: &str, url: &str) -> Value {
    json!({
        "name": name,
        "check": {
            "type": "http",
            "url": url,
            "method": "GET",
            "timeout": 5000,
            "follow_redirects": false,
            "max_redirects": 0,
            "expected_status": { "kind": "exact", "value": 200 },
            "headers": {},
            "verify_tls": true
        },
        "interval": 180,
        "tags": []
    })
}

async fn post_target(app: &Router, name: &str, url: &str) -> axum::http::Response<Body> {
    app.clone()
        .oneshot(
            Request::post("/api/v1/targets")
                .header("content-type", "application/json")
                .body(Body::from(http_target(name, url).to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn abuse_blocked_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM quota_events WHERE event = 'abuse_blocked'")
        .fetch_one(pool)
        .await
        .unwrap_or(0)
}

/// Wait for the fire-and-forget audit insert to land.
async fn wait_for_abuse_rows(pool: &PgPool, more_than: i64) -> bool {
    for _ in 0..40 {
        if abuse_blocked_count(pool).await > more_than {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    false
}

async fn make_owner(pool: &PgPool, org: OrgId) -> UserId {
    let user = make_user(pool, "usage").await;
    sqlx::query("INSERT INTO memberships (user_id, org_id, role) VALUES ($1, $2, 'owner')")
        .bind(user.0)
        .bind(org.0)
        .execute(pool)
        .await
        .expect("seed owner membership");
    user
}

async fn make_signup_org(pool: &PgPool, user: UserId, prefix: &str) -> OrgId {
    let slug = format!("{prefix}-{}", uuid::Uuid::now_v7().simple());
    let slug = &slug[..slug.len().min(30)];
    let (id,): (uuid::Uuid,) = sqlx::query_as(
        "WITH a AS (INSERT INTO accounts DEFAULT VALUES RETURNING id) \
         INSERT INTO organizations (slug, name, account_id) \
         SELECT $1, 'Stranger Org', a.id FROM a RETURNING id",
    )
    .bind(slug)
    .fetch_one(pool)
    .await
    .expect("insert stranger org");
    sqlx::query("INSERT INTO memberships (user_id, org_id, role) VALUES ($1, $2, 'owner')")
        .bind(user.0)
        .bind(id)
        .execute(pool)
        .await
        .expect("seed stranger owner membership");
    OrgId(id)
}

/// The shipped `config/default.toml` `[abuse].url_patterns_denied` and the
/// compiled `AbuseConfig::default()` fallback must stay byte-identical — a
/// drift would mean "abuse protection depends on whether the toml loaded".
/// Pure-logic; always runs.
#[test]
fn shipped_abuse_patterns_match_compiled_default() {
    let cfg = uptimepage::config::AppConfig::load().expect("load config/default.toml");
    let compiled = uptimepage::config::AbuseConfig::default();
    assert_eq!(
        cfg.abuse.url_patterns_denied, compiled.url_patterns_denied,
        "config/default.toml [abuse].url_patterns_denied drifted from AbuseConfig::default()"
    );
}

// ── Report item: URL /\.git(/|$) → 400 ABUSE_BLOCKED ─────────────────────
#[tokio::test]
async fn git_dir_url_returns_400_abuse_blocked() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let before = abuse_blocked_count(&pool).await;
    let (app, _org) = build_test_app_with_pg_store(pool.clone(), |_| {}).await;

    let resp = post_target(&app, "recon", "https://example.com/.git/config").await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let b = body_json(resp).await;
    assert_eq!(b["error"]["code"], "ABUSE_BLOCKED");

    assert!(
        wait_for_abuse_rows(&pool, before).await,
        "quota_events should gain an abuse_blocked row"
    );
}

// ── Report item: domain status.betterstack.com → 400 DOMAIN_DENYLISTED ────
#[tokio::test]
async fn denylisted_domain_returns_400_domain_denylisted() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let before = abuse_blocked_count(&pool).await;
    let (app, _org) = build_test_app_with_pg_store(pool.clone(), |_| {}).await;

    // A sub-subdomain of the listed domain — exercises parent matching too.
    let resp = post_target(&app, "competitor", "https://eu.status.betterstack.com/").await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let b = body_json(resp).await;
    assert_eq!(b["error"]["code"], "DOMAIN_DENYLISTED");

    assert!(
        wait_for_abuse_rows(&pool, before).await,
        "quota_events should gain an abuse_blocked row for the domain block"
    );
}

#[tokio::test]
async fn clean_target_is_not_abuse_blocked() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, _org) = build_test_app_with_pg_store(pool, |_| {}).await;
    let resp = post_target(&app, "ok", "https://example.com/healthz").await;
    assert_eq!(resp.status(), StatusCode::CREATED);
}

// ── Report item: GET /usage returns accurate current/limit per quota ──────
#[tokio::test]
async fn org_usage_reports_accurate_counts_and_limits() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, org) = build_test_app_with_pg_store_anon(pool.clone(), |_| {}).await;
    let user = make_owner(&pool, org).await;
    let app = with_session(app, user, Some(org), None);

    assert_eq!(
        post_target(&app, "t1", "https://example.com/a")
            .await
            .status(),
        StatusCode::CREATED
    );
    assert_eq!(
        post_target(&app, "t2", "https://example.com/b")
            .await
            .status(),
        StatusCode::CREATED
    );

    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/orgs/{}/usage", org.0))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let b = body_json(resp).await;

    assert_eq!(b["plan"]["id"], "free");
    assert_eq!(b["quotas"]["max_targets"]["current"], 2);
    assert_eq!(b["quotas"]["max_targets"]["limit"], 20);
    // The seeded owner is the only member.
    assert_eq!(b["quotas"]["max_members"]["current"], 1);
    assert_eq!(b["quotas"]["max_members"]["limit"], 3);
    assert_eq!(b["quotas"]["max_pending_invitations"]["current"], 0);
    assert_eq!(b["policy"]["min_check_interval_secs"], 180);
    assert_eq!(b["policy"]["retention_days"], 30);
    assert_eq!(b["policy"]["raw_days"], 30);
    assert_eq!(b["rate_limits"]["api_writes_per_minute"], 600);
    assert_eq!(b["features"]["incident_narration_enabled"], true);
    assert_eq!(b["features"]["sms_alerts_enabled"], false);
}

#[tokio::test]
async fn org_usage_requires_membership() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, org) = build_test_app_with_pg_store_anon(pool.clone(), |_| {}).await;
    // A user who is NOT a member of `org` — but is a member of their own
    // signup org. CurrentOrg requires an `active_org_id`; give them one
    // they own so the request reaches the org-not-found gate we're testing.
    let stranger = make_user(&pool, "stranger").await;
    let stranger_org = make_signup_org(&pool, stranger, "stranger-org").await;
    let app = with_session(app, stranger, Some(stranger_org), None);
    let resp = app
        .oneshot(
            Request::get(format!("/api/v1/orgs/{}/usage", org.0))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // Same gate as GET /orgs/{id}: a non-member must not learn it exists.
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── Report item: GET /me/usage api_tokens / owned_orgs current+limit ──────
#[tokio::test]
async fn me_usage_reports_tokens_and_owned_orgs() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    // The builder's own session is the caller here: layering a second session
    // over it leaves which identity the handler sees ambiguous, and this
    // endpoint answers strictly about the caller.
    let (app, _org) = build_test_app_with_pg_store(pool.clone(), |_| {}).await;

    let resp = app
        .oneshot(
            Request::get("/api/v1/me/usage")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let b = body_json(resp).await;

    assert_eq!(b["api_tokens"]["current"], 0);
    assert_eq!(b["api_tokens"]["limit"], 5);
    // The caller's own account holds the one seeded org, measured against the
    // allowance the fixture pinned on it.
    assert_eq!(b["owned_orgs"]["current"], 1);
    assert_eq!(b["owned_orgs"]["limit"], common::FIXTURE_MAX_ORGS);
}
