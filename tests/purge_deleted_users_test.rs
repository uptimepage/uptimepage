//! Live-PG tests for the user hard-purge half of the soft-delete worker.
//! Skipped by default; run under `--run-ignored` (or `--include-ignored`)
//! once `DATABASE_URL` is set. No ClickHouse needed — the user purge is
//! pure Postgres. Tests seed their own users (unique emails) and assert on
//! their own rows; the shared dev DB and the per-call `PURGE_BATCH_LIMIT`
//! mean the "purged" cases backdate far enough to sort first.

use crate::common;

use common::{make_user, unique_slug};
use uptimepage::domain::{OrgId, UserId};
use uptimepage::jobs::purge_deleted::purge_users_past_grace;
use uptimepage::storage::create_org_with_owner;
use uuid::Uuid;

const GRACE: u32 = 30;

/// Backdate `deleted_at` so the grace check fires without sleeping.
async fn soft_delete_backdated(pool: &sqlx::PgPool, user: UserId, days_ago: i64) {
    sqlx::query("UPDATE users SET deleted_at = now() - ($2::int * INTERVAL '1 day') WHERE id = $1")
        .bind(user.0)
        .bind(days_ago)
        .execute(pool)
        .await
        .expect("backdate user delete");
}

async fn user_exists(pool: &sqlx::PgPool, user: UserId) -> bool {
    let (e,): (bool,) = sqlx::query_as("SELECT EXISTS (SELECT 1 FROM users WHERE id = $1)")
        .bind(user.0)
        .fetch_one(pool)
        .await
        .unwrap();
    e
}

async fn count(pool: &sqlx::PgPool, sql: &str, user: UserId) -> i64 {
    let (n,): (i64,) = sqlx::query_as(sql)
        .bind(user.0)
        .fetch_one(pool)
        .await
        .unwrap();
    n
}

async fn cleanup_user(pool: &sqlx::PgPool, user: UserId) {
    let _ = sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(pool)
        .await;
}

async fn cleanup_org(pool: &sqlx::PgPool, org: OrgId) {
    let _ = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org.0)
        .execute(pool)
        .await;
}

/// T3 — a user still inside the grace window is never purged.
#[tokio::test]
#[ignore]
async fn user_within_grace_is_kept() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "purge-user").await;
    soft_delete_backdated(&pool, user, 29).await;

    purge_users_past_grace(&pool, GRACE).await.unwrap();

    assert!(
        user_exists(&pool, user).await,
        "user within grace must survive the purge"
    );
    cleanup_user(&pool, user).await;
}

/// T4 — a user past grace is hard-deleted (the sole gate is `deleted_at` age).
#[tokio::test]
#[ignore]
async fn user_past_grace_is_purged() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "purge-user").await;
    soft_delete_backdated(&pool, user, 365).await;

    // Count is racy on the shared DB (a parallel test's batch call may grab
    // this user first); the row-gone state is deterministic either way.
    purge_users_past_grace(&pool, GRACE).await.unwrap();
    assert!(
        !user_exists(&pool, user).await,
        "past-grace user row must be physically gone"
    );
}

/// T5 — a user who restored (deleted_at cleared) is not eligible: the
/// `deleted_at IS NOT NULL` predicate excludes them.
#[tokio::test]
#[ignore]
async fn recovered_user_is_not_purged() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    // "Restored" = deleted_at cleared. deleted_at IS NULL throughout, so no
    // concurrent purge tick can ever consider this user eligible (testing the
    // predicate, not a NOT-NULL window another test's batch could race into).
    let user = make_user(&pool, "purge-user").await;

    purge_users_past_grace(&pool, GRACE).await.unwrap();

    assert!(
        user_exists(&pool, user).await,
        "an active (un-deleted) user must never be purged"
    );
    cleanup_user(&pool, user).await;
}

/// T7 — the `users` FK cascade leaves no orphan rows in the dependent
/// tables after a hard purge.
#[tokio::test]
#[ignore]
async fn fk_cascade_leaves_no_orphans() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "purge-user").await;
    let org = create_org_with_owner(&pool, user, &unique_slug("casc"), "n")
        .await
        .unwrap()
        .unwrap();

    sqlx::query(
        "INSERT INTO oauth_identities (user_id, provider, provider_user_id) \
         VALUES ($1, 'github', $2)",
    )
    .bind(user.0)
    .bind(format!("gh-{}", Uuid::new_v4().simple()))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO api_tokens (user_id, name, token_hash, token_prefix) \
         VALUES ($1, 'k', 'h', $2)",
    )
    .bind(user.0)
    .bind(format!("tok-{}", Uuid::new_v4().simple()))
    .execute(&pool)
    .await
    .unwrap();
    common::seed_session(
        &pool,
        &format!("sess-hash-{}", Uuid::new_v4().simple()),
        user,
        None,
    )
    .await;
    sqlx::query(
        "INSERT INTO invitations \
            (org_id, inviter_id, email, role, token_hash, token_prefix, expires_at) \
         VALUES ($1, $2, $3, 'member', 'h', $4, now() + INTERVAL '7 days')",
    )
    .bind(org.id.0)
    .bind(user.0)
    .bind(format!("invitee-{}@x.example", Uuid::new_v4().simple()))
    .bind(format!("inv-{}", Uuid::new_v4().simple()))
    .execute(&pool)
    .await
    .unwrap();
    soft_delete_backdated(&pool, user, 365).await;
    purge_users_past_grace(&pool, GRACE).await.unwrap();
    assert!(!user_exists(&pool, user).await);

    for (table, sql) in [
        (
            "memberships",
            "SELECT count(*) FROM memberships WHERE user_id = $1",
        ),
        (
            "oauth_identities",
            "SELECT count(*) FROM oauth_identities WHERE user_id = $1",
        ),
        (
            "api_tokens",
            "SELECT count(*) FROM api_tokens WHERE user_id = $1",
        ),
        (
            "sessions",
            "SELECT count(*) FROM sessions WHERE user_id = $1",
        ),
        (
            "invitations",
            "SELECT count(*) FROM invitations WHERE inviter_id = $1",
        ),
    ] {
        assert_eq!(
            count(&pool, sql, user).await,
            0,
            "{table} must have no rows for the purged user"
        );
    }

    cleanup_org(&pool, org.id).await;
}

/// T8 — audit rows survive the purge with the actor nulled
/// (`ON DELETE SET NULL`), so credential-stuffing / audit history is not
/// erased when a user is forgotten.
#[tokio::test]
#[ignore]
async fn audit_rows_survive_with_nulled_actor() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "purge-user").await;
    let org = create_org_with_owner(&pool, user, &unique_slug("aud"), "n")
        .await
        .unwrap()
        .unwrap();

    let (la_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO login_attempts (user_id, method, success) \
         VALUES ($1, 'github_oauth', true) RETURNING id",
    )
    .bind(user.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    let (al_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO org_audit_log (org_id, actor_id, action) \
         VALUES ($1, $2, 'member.added') RETURNING id",
    )
    .bind(org.id.0)
    .bind(user.0)
    .fetch_one(&pool)
    .await
    .unwrap();

    soft_delete_backdated(&pool, user, 365).await;
    purge_users_past_grace(&pool, GRACE).await.unwrap();
    assert!(!user_exists(&pool, user).await);

    let (la_user,): (Option<Uuid>,) =
        sqlx::query_as("SELECT user_id FROM login_attempts WHERE id = $1")
            .bind(la_id)
            .fetch_one(&pool)
            .await
            .expect("login_attempts row must survive");
    assert!(
        la_user.is_none(),
        "login_attempts.user_id nulled, not deleted"
    );

    let (al_actor,): (Option<Uuid>,) =
        sqlx::query_as("SELECT actor_id FROM org_audit_log WHERE id = $1")
            .bind(al_id)
            .fetch_one(&pool)
            .await
            .expect("org_audit_log row must survive");
    assert!(
        al_actor.is_none(),
        "org_audit_log.actor_id nulled, not deleted"
    );

    let _ = sqlx::query("DELETE FROM login_attempts WHERE id = $1")
        .bind(la_id)
        .execute(&pool)
        .await;
    cleanup_org(&pool, org.id).await;
}
