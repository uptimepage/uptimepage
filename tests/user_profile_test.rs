//! Integration coverage for the user-profile columns added in the
//! pre-mortem hardening pass: `signup_org_id` and `last_seen_at` (wired via
//! session touch).

use crate::common;

use chrono::{DateTime, Utc};
use common::{drop_test_db, fresh_test_db, open_test_pool};
use uptimepage::domain::UserId;
use uuid::Uuid;

use crate::common::MIGRATOR;

async fn fresh_pg() -> Option<(String, String)> {
    fresh_test_db("user_profile").await
}

async fn seed_user_with_org(pool: &sqlx::PgPool, email: &str, slug: &str) -> (Uuid, Uuid) {
    let (user_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO users (email, display_name, terms_version, privacy_version) \
         VALUES ($1, 'u', 'v1', 'v1') RETURNING id",
    )
    .bind(email)
    .fetch_one(pool)
    .await
    .unwrap();
    let (org_id,): (Uuid,) = sqlx::query_as(
        "WITH a AS (INSERT INTO accounts DEFAULT VALUES RETURNING id) \
         INSERT INTO organizations (slug, name, account_id) \
         SELECT $1, 'Test Org', a.id FROM a RETURNING id",
    )
    .bind(slug)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO memberships (user_id, org_id, role) VALUES ($1, $2, 'owner')")
        .bind(user_id)
        .bind(org_id)
        .execute(pool)
        .await
        .unwrap();
    (user_id, org_id)
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn get_signup_org_id_skips_soft_deleted_org() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let (user, org) = seed_user_with_org(&pool, "tomb@example.test", "tomb-org").await;
    sqlx::query("UPDATE users SET signup_org_id = $1 WHERE id = $2")
        .bind(org)
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE organizations SET deleted_at = now() WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();

    let resolved = uptimepage::storage::users::get_signup_org_id(&pool, UserId(user))
        .await
        .unwrap();
    assert!(
        resolved.is_none(),
        "soft-deleted signup org must not be returned as the anchor"
    );

    pool.close().await;
    drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn session_touch_bumps_users_last_seen_at() {
    use uptimepage::auth::session::{
        build_debounce_cache, hash_session_id, touch_last_used_debounced,
    };

    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let (user, _) = seed_user_with_org(&pool, "ls@example.test", "ls-org").await;
    let token = "raw-cookie-value-test";
    let id_hash = hash_session_id(token);
    common::seed_session(&pool, &id_hash, uptimepage::domain::UserId(user), None).await;

    let cache = build_debounce_cache();
    touch_last_used_debounced(&pool, &cache, &id_hash)
        .await
        .unwrap();

    let (seen,): (Option<DateTime<Utc>>,) =
        sqlx::query_as("SELECT last_seen_at FROM users WHERE id = $1")
            .bind(user)
            .fetch_one(&pool)
            .await
            .unwrap();
    let seen = seen.expect("last_seen_at bumped by session touch");
    assert!(
        seen > Utc::now() - chrono::Duration::seconds(5),
        "last_seen_at must be recent"
    );

    pool.close().await;
    drop_test_db(&name).await;
}
