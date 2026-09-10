//! Live-PG tests for GDPR data export, account deletion, and recovery.
//!
//! Run via:
//!     docker compose -f compose.dev.yml up -d postgres
//!     DATABASE_URL=postgres://monitor:monitor@localhost:5432/monitor \
//!         cargo +1.95 nextest run --test account_gdpr_test --run-ignored all
//!
//! Each test gets its own freshly-created database (migrations applied) so the
//! deletion/recovery transactions run against the real schema in isolation.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    body_json, build_test_app_with_pg, drop_test_db, fresh_test_db, open_test_pool, with_session,
};
use serde_json::json;
use tower::ServiceExt;
use uptimepage::api::error::codes;
use uptimepage::auth::account;
use uptimepage::error::AppError;
use uuid::Uuid;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/postgres");

async fn fresh_pg() -> Option<(String, String)> {
    fresh_test_db("gdpr").await
}

async fn drop_pg(test_db: &str) {
    drop_test_db(test_db).await;
}

async fn open_pool(db_url: &str) -> sqlx::PgPool {
    open_test_pool(db_url).await
}

async fn seed_user(pool: &sqlx::PgPool, email: &str, name: &str) -> Uuid {
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO users (email, display_name, terms_version, privacy_version) \
         VALUES ($1, $2, 'v1', 'v1') RETURNING id",
    )
    .bind(email)
    .bind(name)
    .fetch_one(pool)
    .await
    .unwrap();
    id
}

async fn seed_org(pool: &sqlx::PgPool, slug: &str, owner: Uuid) -> Uuid {
    let (id,): (Uuid,) = sqlx::query_as(
        "WITH a AS (INSERT INTO accounts DEFAULT VALUES RETURNING id) \
         INSERT INTO organizations (slug, name, account_id) \
         SELECT $1, $2, a.id FROM a RETURNING id",
    )
    .bind(slug)
    .bind(slug)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO memberships (user_id, org_id, role) VALUES ($1, $2, 'owner')")
        .bind(owner)
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn add_member(pool: &sqlx::PgPool, org: Uuid, user: Uuid, role: &str) {
    sqlx::query("INSERT INTO memberships (user_id, org_id, role) VALUES ($1, $2, $3)")
        .bind(user)
        .bind(org)
        .bind(role)
        .execute(pool)
        .await
        .unwrap();
}

async fn is_deleted(pool: &sqlx::PgPool, table: &str, id: Uuid) -> bool {
    let sql = format!("SELECT deleted_at IS NOT NULL FROM {table} WHERE id = $1");
    let (b,): (bool,) = sqlx::query_as(&sql).bind(id).fetch_one(pool).await.unwrap();
    b
}

async fn assert_user_deleted(pool: &sqlx::PgPool, user: Uuid, expected: bool) {
    assert_eq!(is_deleted(pool, "users", user).await, expected);
}

async fn assert_org_deleted(pool: &sqlx::PgPool, org: Uuid, expected: bool) {
    assert_eq!(is_deleted(pool, "organizations", org).await, expected);
}

// ---------------------------------------------------------------------------
// deletion blocking
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn deletion_blocked_when_solely_owning_org_with_other_members() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let owner = seed_user(&pool, "owner@example.test", "Owner").await;
    let other = seed_user(&pool, "member@example.test", "Member").await;
    let org = seed_org(&pool, "shared", owner).await;
    add_member(&pool, org, other, "member").await;

    let err = account::request_deletion(&pool, uptimepage::domain::UserId(owner), 30)
        .await
        .expect_err("must block");
    match err {
        AppError::UnprocessableDetails { code, details, .. } => {
            assert_eq!(code, codes::OWNS_SHARED_ORGS);
            let orgs = details["orgs"].as_array().expect("orgs array");
            assert_eq!(orgs.len(), 1);
            assert_eq!(orgs[0]["slug"], "shared");
        }
        other => panic!("expected OWNS_SHARED_ORGS, got {other:?}"),
    }

    // Nothing was mutated — the user and org are untouched.
    assert_user_deleted(&pool, owner, false).await;
    assert_org_deleted(&pool, org, false).await;

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn deletion_blocked_while_a_co_owned_org_bills_to_the_account() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let payer = seed_user(&pool, "payer@example.test", "Payer").await;
    let co_owner = seed_user(&pool, "co@example.test", "Co").await;
    let org = uptimepage::storage::create_org_with_owner(
        &pool,
        uptimepage::domain::UserId(payer),
        "billed",
        "Billed",
    )
    .await
    .unwrap()
    .unwrap()
    .id;
    add_member(&pool, org.0, co_owner, "owner").await;

    let err = account::request_deletion(&pool, uptimepage::domain::UserId(payer), 30)
        .await
        .expect_err("must block");
    match err {
        AppError::UnprocessableDetails { code, details, .. } => {
            assert_eq!(code, codes::OWNS_SHARED_ORGS);
            assert_eq!(details["orgs"][0]["slug"], "billed");
        }
        other => panic!("expected OWNS_SHARED_ORGS, got {other:?}"),
    }
    assert_user_deleted(&pool, payer, false).await;
    assert_org_deleted(&pool, org.0, false).await;

    // Once the org holds nobody else it goes down with the account.
    sqlx::query("DELETE FROM memberships WHERE org_id = $1 AND user_id = $2")
        .bind(org.0)
        .bind(co_owner)
        .execute(&pool)
        .await
        .unwrap();
    account::request_deletion(&pool, uptimepage::domain::UserId(payer), 30)
        .await
        .expect("deletes");
    assert_user_deleted(&pool, payer, true).await;
    assert_org_deleted(&pool, org.0, true).await;

    pool.close().await;
    drop_pg(&name).await;
}

// ---------------------------------------------------------------------------
// delete then restore (re-auth) round-trip
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn deletion_then_restore_round_trip() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let user = seed_user(&pool, "solo@example.test", "Solo").await;
    let uid = uptimepage::domain::UserId(user);
    let org = seed_org(&pool, "solo-org", user).await;
    common::seed_session(&pool, "sess-1-hash", uid, None).await;
    sqlx::query(
        "INSERT INTO api_tokens (user_id, name, token_hash, token_prefix) \
         VALUES ($1, 't', 'h', 'p')",
    )
    .bind(user)
    .execute(&pool)
    .await
    .unwrap();

    let outcome = account::request_deletion(&pool, uid, 30)
        .await
        .expect("deletion succeeds");
    assert_eq!(outcome.email, "solo@example.test");

    // User + solo org soft-deleted; sessions/tokens gone; owner membership
    // kept (so re-auth can restore access).
    assert_user_deleted(&pool, user, true).await;
    assert_org_deleted(&pool, org, true).await;
    let (sessions,): (i64,) = sqlx::query_as("SELECT count(*) FROM sessions WHERE user_id = $1")
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(sessions, 0);
    let (tokens,): (i64,) = sqlx::query_as("SELECT count(*) FROM api_tokens WHERE user_id = $1")
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(tokens, 0);
    let (memberships,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM memberships WHERE user_id = $1")
            .bind(user)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(memberships, 1, "solo-org owner membership must be kept");

    // A second deletion of the now-soft-deleted account is rejected cleanly
    // by the `deleted_at IS NULL` guard.
    match account::request_deletion(&pool, uid, 30).await {
        Err(AppError::Conflict { code, .. }) => {
            assert_eq!(code, codes::ACCOUNT_ALREADY_DELETED);
        }
        other => panic!("expected ACCOUNT_ALREADY_DELETED, got {other:?}"),
    }

    account::restore_account(&pool, uid)
        .await
        .expect("restore succeeds")
        .expect("account was scheduled for deletion");
    assert_user_deleted(&pool, user, false).await;
    assert_org_deleted(&pool, org, false).await;

    pool.close().await;
    drop_pg(&name).await;
}

/// Recovering an account undoes *that* deletion, nothing older. An org the
/// user had already deleted on their own stays deleted: sweeping it back in
/// would hand the account capacity it had given up, past `max_orgs` and past
/// every pooled cap, without any of the checks `restore_org` runs.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn account_recovery_leaves_earlier_org_deletions_alone() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let user = seed_user(&pool, "two-orgs@example.test", "Two").await;
    let uid = uptimepage::domain::UserId(user);
    let earlier = seed_org(&pool, "given-up-org", user).await;
    let current = seed_org(&pool, "still-running-org", user).await;

    // The user deletes one org a week before closing the account.
    sqlx::query("UPDATE organizations SET deleted_at = now() - interval '7 days' WHERE id = $1")
        .bind(earlier)
        .execute(&pool)
        .await
        .unwrap();

    account::request_deletion(&pool, uid, 30)
        .await
        .expect("deletion succeeds");
    assert_org_deleted(&pool, current, true).await;

    let restored = account::restore_account(&pool, uid)
        .await
        .expect("restore succeeds")
        .expect("account was scheduled for deletion");

    assert_user_deleted(&pool, user, false).await;
    assert_org_deleted(&pool, current, false).await;
    assert_org_deleted(&pool, earlier, true).await;
    assert_eq!(
        restored.orgs,
        vec![current],
        "recovery reports only what this deletion took"
    );

    pool.close().await;
    drop_pg(&name).await;
}

/// An already-active account reports "nothing to restore", not an error.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn restore_active_account_is_noop() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let user = seed_user(&pool, "active@example.test", "Active").await;
    let uid = uptimepage::domain::UserId(user);
    let org = seed_org(&pool, "active-org", user).await;

    let restored = account::restore_account(&pool, uid)
        .await
        .expect("restore is a no-op");
    assert!(
        restored.is_none(),
        "an active account has nothing to restore"
    );
    assert_user_deleted(&pool, user, false).await;
    assert_org_deleted(&pool, org, false).await;

    pool.close().await;
    drop_pg(&name).await;
}

/// The restore and the purge both take the per-user delete lock, so a restore
/// cannot run while a purge holds it (the race that wiped data into a
/// 'restored' account). Hold the lock on one connection and assert the restore
/// blocks until it's released.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn restore_blocks_while_user_delete_lock_held() {
    use uptimepage::storage::locks::{advisory_xact_lock, user_delete_lock_key};

    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let user = seed_user(&pool, "race@example.test", "Race").await;
    let uid = uptimepage::domain::UserId(user);
    seed_org(&pool, "race-org", user).await;
    account::request_deletion(&pool, uid, 30)
        .await
        .expect("deletion succeeds");

    // Holder takes the lock and keeps its tx open.
    let mut holder = pool.begin().await.unwrap();
    advisory_xact_lock(&mut *holder, &user_delete_lock_key(uid))
        .await
        .unwrap();

    // While held, the restore must not make progress.
    let blocked = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        account::restore_account(&pool, uid),
    )
    .await;
    assert!(blocked.is_err(), "restore ran while delete lock was held");

    // Release, then the restore succeeds.
    holder.commit().await.unwrap();
    account::restore_account(&pool, uid)
        .await
        .unwrap()
        .expect("account was scheduled for deletion");
    assert_user_deleted(&pool, user, false).await;

    pool.close().await;
    drop_pg(&name).await;
}

// ---------------------------------------------------------------------------
// data export: redaction + cross-user exclusion
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn data_export_redacts_credentials_and_excludes_other_emails() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let owner = seed_user(&pool, "exporter@example.test", "Exporter").await;
    let other = seed_user(&pool, "coworker-secret@example.test", "Coworker").await;
    let org = seed_org(&pool, "export-org", owner).await;
    add_member(&pool, org, other, "member").await;

    // Target carrying credentials, stored at rest (no KEK in the test state →
    // plaintext-at-rest, the case redaction must also cover).
    let check_spec = json!({
        "type": "http",
        "url": "https://example.test/healthz",
        "method": "GET",
        "basic_auth": ["admin", "hunter2"],
        "bearer_token": "super-secret-token"
    });
    sqlx::query(
        "INSERT INTO targets (org_id, name, check_spec, interval_secs) \
         VALUES ($1, 'api', $2, 60)",
    )
    .bind(org)
    .bind(&check_spec)
    .execute(&pool)
    .await
    .unwrap();

    let (router, _) = build_test_app_with_pg(pool.clone(), |_cfg| {}).await;
    let router = with_session(router, uptimepage::domain::UserId(owner), None, None);

    let resp = router
        .oneshot(
            Request::get("/api/v1/me/data-export")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;

    let owned = &body["owned_orgs"];
    assert_eq!(owned.as_array().unwrap().len(), 1);
    let target = &owned[0]["targets"][0];
    assert_eq!(target["check"]["basic_auth"], json!("***"));
    assert_eq!(target["check"]["bearer_token"], json!("***"));

    // Co-member appears by name + role only.
    let members = owned[0]["members"].as_array().unwrap();
    assert!(members.iter().any(|m| m["display_name"] == "Coworker"));
    for m in members {
        assert!(m.get("email").is_none(), "member rows must not carry email");
    }

    // The raw secrets and the other user's email must not appear anywhere in
    // the serialized export.
    let dump = serde_json::to_string(&body).unwrap();
    assert!(!dump.contains("hunter2"), "basic_auth secret leaked");
    assert!(!dump.contains("super-secret-token"), "bearer token leaked");
    assert!(
        !dump.contains("coworker-secret@example.test"),
        "third-party email leaked"
    );

    pool.close().await;
    drop_pg(&name).await;
}
