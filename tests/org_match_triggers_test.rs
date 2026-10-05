//! Schema-layer integration tests for the `006_org_match_triggers.up.sql`
//! belt-and-suspenders guards. These triggers refuse any tenant-child row
//! whose denormalised `org_id` doesn't match its parent's (or, for
//! `maintenance_window_components`, doesn't match its referenced target's).
//! The repository layer is supposed to enforce the same invariant from
//! Rust, but the triggers are the last line that catches a bypass — direct
//! SQL, a future repo path that forgets, a misuse via an admin tool.
//!
//! Skipped by default; runs under `--include-ignored` once `DATABASE_URL`
//! is set.

use crate::common;

use sqlx::PgPool;
use uuid::Uuid;

async fn seed_two_orgs(pool: &PgPool) -> (Uuid, Uuid) {
    let suffix = Uuid::new_v4().simple().to_string();
    let a: (Uuid,) = sqlx::query_as(
        "WITH a AS (INSERT INTO accounts DEFAULT VALUES RETURNING id) \
         INSERT INTO organizations (slug, name, account_id) \
         SELECT $1, 'a', a.id FROM a RETURNING id",
    )
    .bind(format!("orgtrig-a-{}", &suffix[..8]))
    .fetch_one(pool)
    .await
    .unwrap();
    let b: (Uuid,) = sqlx::query_as(
        "WITH a AS (INSERT INTO accounts DEFAULT VALUES RETURNING id) \
         INSERT INTO organizations (slug, name, account_id) \
         SELECT $1, 'b', a.id FROM a RETURNING id",
    )
    .bind(format!("orgtrig-b-{}", &suffix[..8]))
    .fetch_one(pool)
    .await
    .unwrap();
    (a.0, b.0)
}

async fn seed_target(pool: &PgPool, org: Uuid, name: &str) -> Uuid {
    let row: (Uuid,) = sqlx::query_as(
        r#"INSERT INTO targets (org_id, name, check_spec, interval_secs)
           VALUES ($1, $2, '{"type":"http","url":"https://example.com/"}'::jsonb, 60)
           RETURNING id"#,
    )
    .bind(org)
    .bind(name)
    .fetch_one(pool)
    .await
    .unwrap();
    row.0
}

async fn seed_incident(pool: &PgPool, org: Uuid, target: Uuid) -> Uuid {
    let row: (Uuid,) = sqlx::query_as(
        r#"INSERT INTO incidents (org_id, target_id, started_at, status_at_start)
           VALUES ($1, $2, now(), 'down')
           RETURNING id"#,
    )
    .bind(org)
    .bind(target)
    .fetch_one(pool)
    .await
    .unwrap();
    row.0
}

async fn seed_maintenance(pool: &PgPool, org: Uuid) -> Uuid {
    let row: (Uuid,) = sqlx::query_as(
        r#"INSERT INTO maintenance_windows (org_id, title, starts_at, ends_at)
           VALUES ($1, 't', now(), now() + INTERVAL '1 hour')
           RETURNING id"#,
    )
    .bind(org)
    .fetch_one(pool)
    .await
    .unwrap();
    row.0
}

async fn seed_status_page(pool: &PgPool, org: Uuid) -> Uuid {
    let slug = format!("sptrig{}", &Uuid::new_v4().simple().to_string()[..12]);
    let row: (Uuid,) = sqlx::query_as(
        r#"INSERT INTO status_pages (org_id, slug, name, enabled)
           VALUES ($1, $2, 'p', true)
           RETURNING id"#,
    )
    .bind(org)
    .bind(slug)
    .fetch_one(pool)
    .await
    .unwrap();
    row.0
}

async fn cleanup_orgs(pool: &PgPool, orgs: &[Uuid]) {
    let _ = sqlx::query("DELETE FROM organizations WHERE id = ANY($1)")
        .bind(orgs)
        .execute(pool)
        .await;
}

#[tokio::test]
#[ignore]
async fn incident_updates_cross_org_insert_raises() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b) = seed_two_orgs(&pool).await;
    let target_a = seed_target(&pool, org_a, "ta").await;
    let incident_a = seed_incident(&pool, org_a, target_a).await;

    // Insert an incident_update tagged with org_b but pointing at org_a's
    // incident — must trip the BEFORE-INSERT trigger.
    let err = sqlx::query(
        r#"INSERT INTO incident_updates (org_id, incident_id, phase, message)
           VALUES ($1, $2, 'investigating', 'x')"#,
    )
    .bind(org_b)
    .bind(incident_a)
    .execute(&pool)
    .await
    .expect_err("cross-org incident_update must be rejected");
    assert!(
        err.to_string().contains("org_id mismatch"),
        "expected trigger exception, got: {err}"
    );

    cleanup_orgs(&pool, &[org_a, org_b]).await;
}

#[tokio::test]
#[ignore]
async fn maintenance_components_cross_org_parent_insert_raises() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b) = seed_two_orgs(&pool).await;
    let target_b = seed_target(&pool, org_b, "tb").await;
    let mw_a = seed_maintenance(&pool, org_a).await;

    // Row stamped org_b but references org_a's maintenance window — must
    // trip the parent-org trigger.
    let err = sqlx::query(
        r#"INSERT INTO maintenance_window_components (org_id, maintenance_id, target_id)
           VALUES ($1, $2, $3)"#,
    )
    .bind(org_b)
    .bind(mw_a)
    .bind(target_b)
    .execute(&pool)
    .await
    .expect_err("cross-org maintenance_window_components insert must be rejected");
    assert!(
        err.to_string().contains("org_id mismatch"),
        "expected parent-org trigger exception, got: {err}"
    );

    cleanup_orgs(&pool, &[org_a, org_b]).await;
}

#[tokio::test]
#[ignore]
async fn maintenance_components_cross_org_target_insert_raises() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b) = seed_two_orgs(&pool).await;
    let target_b = seed_target(&pool, org_b, "tb").await;
    let mw_a = seed_maintenance(&pool, org_a).await;

    // Row consistent with mw's org (a) but points at org_b's target — must
    // trip the new target-org trigger.
    let err = sqlx::query(
        r#"INSERT INTO maintenance_window_components (org_id, maintenance_id, target_id)
           VALUES ($1, $2, $3)"#,
    )
    .bind(org_a)
    .bind(mw_a)
    .bind(target_b)
    .execute(&pool)
    .await
    .expect_err("cross-org target reference must be rejected");
    assert!(
        err.to_string().contains("target_id"),
        "expected target-org trigger exception, got: {err}"
    );

    cleanup_orgs(&pool, &[org_a, org_b]).await;
}

#[tokio::test]
#[ignore]
async fn maintenance_components_target_swap_to_foreign_org_raises() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b) = seed_two_orgs(&pool).await;
    let target_a = seed_target(&pool, org_a, "ta").await;
    let target_b = seed_target(&pool, org_b, "tb").await;
    let mw_a = seed_maintenance(&pool, org_a).await;

    sqlx::query(
        r#"INSERT INTO maintenance_window_components (org_id, maintenance_id, target_id)
           VALUES ($1, $2, $3)"#,
    )
    .bind(org_a)
    .bind(mw_a)
    .bind(target_a)
    .execute(&pool)
    .await
    .unwrap();

    // Future code path that does a bare `UPDATE OF target_id` to swap to
    // another org's target. The broadened trigger now covers
    // `UPDATE OF target_id`; the target-org trigger refuses the new value.
    let err = sqlx::query(
        r#"UPDATE maintenance_window_components
              SET target_id = $1
            WHERE maintenance_id = $2"#,
    )
    .bind(target_b)
    .bind(mw_a)
    .execute(&pool)
    .await
    .expect_err("target_id swap to foreign org must be rejected");
    assert!(
        err.to_string().contains("target_id") || err.to_string().contains("org_id mismatch"),
        "expected trigger exception, got: {err}"
    );

    cleanup_orgs(&pool, &[org_a, org_b]).await;
}

#[tokio::test]
#[ignore]
async fn status_page_components_cross_org_parent_insert_raises() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b) = seed_two_orgs(&pool).await;
    let target_b = seed_target(&pool, org_b, "tb").await;
    let page_a = seed_status_page(&pool, org_a).await;

    // Row stamped org_b but references org_a's page — must trip the
    // parent-org trigger.
    let err = sqlx::query(
        r#"INSERT INTO status_page_components (org_id, status_page_id, target_id)
           VALUES ($1, $2, $3)"#,
    )
    .bind(org_b)
    .bind(page_a)
    .bind(target_b)
    .execute(&pool)
    .await
    .expect_err("cross-org status_page_components insert must be rejected");
    assert!(
        err.to_string().contains("org_id mismatch"),
        "expected parent-org trigger exception, got: {err}"
    );

    cleanup_orgs(&pool, &[org_a, org_b]).await;
}

#[tokio::test]
#[ignore]
async fn status_page_components_cross_org_target_insert_raises() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b) = seed_two_orgs(&pool).await;
    let target_b = seed_target(&pool, org_b, "tb").await;
    let page_a = seed_status_page(&pool, org_a).await;

    // Row consistent with the page's org (a) but curates org_b's target —
    // must trip the target-org trigger.
    let err = sqlx::query(
        r#"INSERT INTO status_page_components (org_id, status_page_id, target_id)
           VALUES ($1, $2, $3)"#,
    )
    .bind(org_a)
    .bind(page_a)
    .bind(target_b)
    .execute(&pool)
    .await
    .expect_err("cross-org target reference must be rejected");
    assert!(
        err.to_string().contains("target_id"),
        "expected target-org trigger exception, got: {err}"
    );

    cleanup_orgs(&pool, &[org_a, org_b]).await;
}

#[tokio::test]
#[ignore]
async fn status_page_components_target_swap_to_foreign_org_raises() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b) = seed_two_orgs(&pool).await;
    let target_a = seed_target(&pool, org_a, "ta").await;
    let target_b = seed_target(&pool, org_b, "tb").await;
    let page_a = seed_status_page(&pool, org_a).await;

    sqlx::query(
        r#"INSERT INTO status_page_components (org_id, status_page_id, target_id)
           VALUES ($1, $2, $3)"#,
    )
    .bind(org_a)
    .bind(page_a)
    .bind(target_a)
    .execute(&pool)
    .await
    .unwrap();

    // A bare `UPDATE OF target_id` that swaps the curated monitor to another
    // org's target. The broadened trigger covers `UPDATE OF target_id`; the
    // target-org trigger refuses the new value.
    let err = sqlx::query(
        r#"UPDATE status_page_components
              SET target_id = $1
            WHERE status_page_id = $2 AND target_id = $3"#,
    )
    .bind(target_b)
    .bind(page_a)
    .bind(target_a)
    .execute(&pool)
    .await
    .expect_err("target_id swap to foreign org must be rejected");
    assert!(
        err.to_string().contains("target_id") || err.to_string().contains("org_id mismatch"),
        "expected trigger exception, got: {err}"
    );

    cleanup_orgs(&pool, &[org_a, org_b]).await;
}

#[tokio::test]
#[ignore]
async fn same_org_inserts_succeed() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org_a, _org_b) = seed_two_orgs(&pool).await;
    let target_a = seed_target(&pool, org_a, "ta").await;
    let incident_a = seed_incident(&pool, org_a, target_a).await;
    let mw_a = seed_maintenance(&pool, org_a).await;
    let page_a = seed_status_page(&pool, org_a).await;

    sqlx::query(
        r#"INSERT INTO incident_updates (org_id, incident_id, phase, message)
           VALUES ($1, $2, 'investigating', 'baseline')"#,
    )
    .bind(org_a)
    .bind(incident_a)
    .execute(&pool)
    .await
    .expect("same-org incident_update must succeed");

    sqlx::query(
        r#"INSERT INTO maintenance_window_components (org_id, maintenance_id, target_id)
           VALUES ($1, $2, $3)"#,
    )
    .bind(org_a)
    .bind(mw_a)
    .bind(target_a)
    .execute(&pool)
    .await
    .expect("same-org maintenance_window_components must succeed");

    sqlx::query(
        r#"INSERT INTO status_page_components (org_id, status_page_id, target_id)
           VALUES ($1, $2, $3)"#,
    )
    .bind(org_a)
    .bind(page_a)
    .bind(target_a)
    .execute(&pool)
    .await
    .expect("same-org status_page_components must succeed");

    cleanup_orgs(&pool, &[org_a, _org_b]).await;
}
