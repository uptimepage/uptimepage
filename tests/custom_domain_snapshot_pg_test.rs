//! Snapshot membership must match `find_public_status_page_by_slug`: enabled,
//! not held, owning org not soft-deleted, plus verified and a plan that sells
//! one. Drift either way is a live bug.
//!
//! Live PG only; no-ops without `DATABASE_URL`.

mod common;

use chrono::Utc;
use common::{make_user, pg_pool_from_env, unique_slug};
use sqlx::PgPool;
use uptimepage::request::custom_domains::CustomDomains;
use uptimepage::storage::status_pages::load_verified_custom_domains;
use uuid::Uuid;

struct Page {
    org: Uuid,
    page: Uuid,
    domain: String,
}

async fn verified_page_on(pool: &PgPool, plan: &str) -> Page {
    let user = make_user(pool, "cdom").await;
    let (account,): (Uuid,) = sqlx::query_as(
        "INSERT INTO accounts (owner_user_id, plan_id) VALUES ($1, $2) \
         ON CONFLICT (owner_user_id) WHERE owner_user_id IS NOT NULL \
         DO UPDATE SET plan_id = excluded.plan_id RETURNING id",
    )
    .bind(user.0)
    .bind(plan)
    .fetch_one(pool)
    .await
    .expect("account");
    let (org,): (Uuid,) = sqlx::query_as(
        "INSERT INTO organizations (slug, name, account_id) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(unique_slug("cdom"))
    .bind("Custom Domain Org")
    .bind(account)
    .fetch_one(pool)
    .await
    .expect("org");
    let domain = format!("status.{}.test", unique_slug("cd"));
    let (page,): (Uuid,) = sqlx::query_as(
        "INSERT INTO status_pages (org_id, slug, name, enabled, custom_domain, \
                                   custom_domain_verified_at) \
         VALUES ($1, $2, 'Status', true, $3, now()) RETURNING id",
    )
    .bind(org)
    .bind(unique_slug("cdpage"))
    .bind(&domain)
    .fetch_one(pool)
    .await
    .expect("status page");
    Page { org, page, domain }
}

async fn snapshot_holds(pool: &PgPool, domain: &str) -> bool {
    let rows = load_verified_custom_domains(pool)
        .await
        .expect("load custom domains");
    let domains = CustomDomains::new("example.com");
    domains.install(rows);
    domains.lookup(domain).is_some()
}

async fn published(pool: &PgPool, p: &Page) -> Option<String> {
    let rows = load_verified_custom_domains(pool)
        .await
        .expect("load custom domains");
    let domains = CustomDomains::new("example.com");
    domains.install(rows);
    domains
        .published(uptimepage::domain::StatusPageId(p.page))
        .map(|d| d.to_string())
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn a_verified_page_on_a_plan_that_sells_one_is_served() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let p = verified_page_on(&pool, "team").await;
    assert!(snapshot_holds(&pool, &p.domain).await);
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn an_unverified_page_is_not_served() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let p = verified_page_on(&pool, "team").await;
    sqlx::query("UPDATE status_pages SET custom_domain_verified_at = NULL WHERE id = $1")
        .bind(p.page)
        .execute(&pool)
        .await
        .expect("unverify");
    assert!(!snapshot_holds(&pool, &p.domain).await);
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn a_held_page_is_not_served() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let p = verified_page_on(&pool, "team").await;
    sqlx::query("UPDATE status_pages SET plan_hold_at = $2 WHERE id = $1")
        .bind(p.page)
        .bind(Utc::now())
        .execute(&pool)
        .await
        .expect("hold");
    assert!(!snapshot_holds(&pool, &p.domain).await);
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn a_soft_deleted_org_is_not_served() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let p = verified_page_on(&pool, "team").await;
    sqlx::query("UPDATE organizations SET deleted_at = $2 WHERE id = $1")
        .bind(p.org)
        .bind(Utc::now())
        .execute(&pool)
        .await
        .expect("soft delete");
    assert!(!snapshot_holds(&pool, &p.domain).await);
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn a_disabled_page_is_not_served() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let p = verified_page_on(&pool, "team").await;
    sqlx::query("UPDATE status_pages SET enabled = false WHERE id = $1")
        .bind(p.page)
        .execute(&pool)
        .await
        .expect("disable");
    assert!(!snapshot_holds(&pool, &p.domain).await);
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn a_plan_that_does_not_sell_one_is_not_served() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let p = verified_page_on(&pool, "free").await;
    assert!(!snapshot_holds(&pool, &p.domain).await);
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn verification_serves_and_activation_publishes() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let p = verified_page_on(&pool, "team").await;
    assert!(snapshot_holds(&pool, &p.domain).await);
    assert_eq!(published(&pool, &p).await, None);

    sqlx::query("UPDATE status_pages SET custom_domain_activated_at = now() WHERE id = $1")
        .bind(p.page)
        .execute(&pool)
        .await
        .expect("activate");
    assert_eq!(published(&pool, &p).await.as_deref(), Some(&*p.domain));
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn activation_without_verification_is_refused_by_the_schema() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let p = verified_page_on(&pool, "team").await;
    let err = sqlx::query(
        "UPDATE status_pages \
         SET custom_domain_verified_at = NULL, custom_domain_activated_at = now() \
         WHERE id = $1",
    )
    .bind(p.page)
    .execute(&pool)
    .await
    .expect_err("activation without verification must be refused");
    assert!(
        err.to_string().contains("custom_domain_activation"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn the_column_refuses_a_non_canonical_or_malformed_domain() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let p = verified_page_on(&pool, "team").await;
    for bad in [
        "STATUS.acme.test",
        "status.acme.test.",
        "status..acme.test",
        "-lead.acme.test",
        "trail-.acme.test",
        "status_page.acme.test",
        "stätus.acme.test",
        "localhost",
    ] {
        let err = sqlx::query("UPDATE status_pages SET custom_domain = $2 WHERE id = $1")
            .bind(p.page)
            .bind(bad)
            .execute(&pool)
            .await
            .expect_err(&format!("{bad} must be refused"));
        assert!(
            err.to_string().contains("custom_domain_canonical"),
            "{bad}: unexpected error: {err}"
        );
    }
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn the_column_accepts_a_canonical_domain_including_punycode() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let p = verified_page_on(&pool, "team").await;
    let unique = unique_slug("x");
    for good in [
        format!("xn--sttus-hra.{unique}.test"),
        format!("a-b.c-d.{unique}.example"),
    ] {
        sqlx::query("UPDATE status_pages SET custom_domain = $2 WHERE id = $1")
            .bind(p.page)
            .bind(&good)
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("{good} must be accepted: {e}"));
    }
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn a_disabled_page_stops_publishing_its_custom_domain() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let p = verified_page_on(&pool, "team").await;
    sqlx::query("UPDATE status_pages SET custom_domain_activated_at = now() WHERE id = $1")
        .bind(p.page)
        .execute(&pool)
        .await
        .expect("activate");
    assert_eq!(published(&pool, &p).await.as_deref(), Some(&*p.domain));

    for (label, sql) in [
        (
            "disabled",
            "UPDATE status_pages SET enabled = false WHERE id = $1",
        ),
        (
            "held",
            "UPDATE status_pages SET plan_hold_at = now() WHERE id = $1",
        ),
    ] {
        let p2 = verified_page_on(&pool, "team").await;
        sqlx::query("UPDATE status_pages SET custom_domain_activated_at = now() WHERE id = $1")
            .bind(p2.page)
            .execute(&pool)
            .await
            .expect("activate");
        sqlx::query(sql)
            .bind(p2.page)
            .execute(&pool)
            .await
            .expect(label);
        assert_eq!(
            published(&pool, &p2).await,
            None,
            "a {label} page must not publish its custom domain"
        );
        assert!(!snapshot_holds(&pool, &p2.domain).await, "{label}");
    }
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn the_domain_is_normalised_the_same_way_on_load_as_on_lookup() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let p = verified_page_on(&pool, "team").await;
    for spelling in [
        p.domain.clone(),
        p.domain.to_uppercase(),
        format!("{}.", p.domain),
        format!("{}:443", p.domain),
    ] {
        assert!(snapshot_holds(&pool, &spelling).await, "{spelling}");
    }
}
