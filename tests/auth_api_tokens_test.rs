//! Live-PG tests for Phase 4: API token issuance, prefix lookup with
//! deliberate collisions, argon2 verification, revoke, max_per_user.
//!
//! Run via:
//!     docker compose -f compose.dev.yml up -d postgres
//!     DATABASE_URL=postgres://monitor:monitor@localhost:5432/monitor \
//!         cargo test --test auth_api_tokens_test -- --ignored

mod common;

use common::make_user;
use uptimepage::auth::api_tokens;
use uptimepage::auth::scope::ScopeSet;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/postgres");

async fn fresh_pg() -> Option<(String, String)> {
    common::fresh_test_db("auth_tokens").await
}

async fn drop_pg(test_db: &str) {
    common::drop_test_db(test_db).await;
}

async fn open_pool(db_url: &str) -> sqlx::PgPool {
    common::open_test_pool(db_url).await
}

const PREFIX_LEN: usize = 16;

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn create_then_lookup_and_revoke_roundtrip() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let user = make_user(&pool, "u").await;
    let created = api_tokens::create(
        &pool,
        user,
        "CI",
        &ScopeSet::full_access(),
        None,
        None,
        PREFIX_LEN,
        1000,
    )
    .await
    .expect("create");
    assert!(created.token.starts_with(api_tokens::TOKEN_PREFIX));
    assert_eq!(created.token.len(), 8 + 43);
    assert_eq!(created.prefix.len(), PREFIX_LEN);

    let outcome = api_tokens::lookup_by_raw(&pool, &created.token, PREFIX_LEN, None)
        .await
        .expect("lookup");
    match outcome {
        api_tokens::LookupOutcome::Active(row) => {
            assert_eq!(row.id, created.id);
            assert_eq!(row.user_id.0, user.0);
        }
        other => panic!("lookup should match, got {other:?}"),
    }

    // Wrong-token returns Invalid (not the matching row), even if same prefix.
    let bogus = format!(
        "{}wrongwrongwrongwrongwrongwrongwrongwrong",
        &created.prefix
    );
    let bogus_outcome = api_tokens::lookup_by_raw(&pool, &bogus, PREFIX_LEN, None)
        .await
        .expect("lookup");
    assert!(matches!(bogus_outcome, api_tokens::LookupOutcome::Invalid));

    let removed = api_tokens::delete_for_user(&pool, user, created.id)
        .await
        .expect("delete");
    assert!(removed);

    let after = api_tokens::lookup_by_raw(&pool, &created.token, PREFIX_LEN, None)
        .await
        .expect("lookup");
    assert!(matches!(after, api_tokens::LookupOutcome::Invalid));

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn forced_prefix_collision_still_finds_correct_token() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let user = make_user(&pool, "u").await;
    // Issue real token, then manually plant a sibling row with the same
    // prefix but a hash for "wrong" so the lookup must disambiguate by
    // argon2-verify, not by prefix alone.
    let created = api_tokens::create(
        &pool,
        user,
        "real",
        &ScopeSet::full_access(),
        None,
        None,
        PREFIX_LEN,
        1000,
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO api_tokens (user_id, name, token_hash, token_prefix) \
         VALUES ($1, 'collider', '$argon2id$v=19$m=19456,t=2,p=1$YzhrXTk5SUVHWUMzbHkwTw$bogus', $2)",
    )
    .bind(user.0)
    .bind(&created.prefix)
    .execute(&pool)
    .await
    .unwrap();

    let outcome = api_tokens::lookup_by_raw(&pool, &created.token, PREFIX_LEN, None)
        .await
        .expect("lookup");
    match outcome {
        api_tokens::LookupOutcome::Active(row) => assert_eq!(row.id, created.id),
        other => panic!("must find the real token, got {other:?}"),
    }

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn count_excludes_expired_tokens_so_quota_reflects_live_set_only() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let user = make_user(&pool, "u").await;

    let t = api_tokens::create(
        &pool,
        user,
        "to-expire",
        &ScopeSet::full_access(),
        None,
        None,
        PREFIX_LEN,
        1000,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE api_tokens SET expires_at = now() - INTERVAL '1 hour' WHERE id = $1")
        .bind(t.id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        api_tokens::count_for_user(&pool, user).await.unwrap(),
        0,
        "expired token must not occupy the quota"
    );

    // Cap atomicity follows the same filter: with the cap at 1 and one
    // expired row present, a new create must succeed (the cap looks at
    // live tokens only).
    let _live = api_tokens::create(
        &pool,
        user,
        "fresh",
        &ScopeSet::full_access(),
        None,
        None,
        PREFIX_LEN,
        1,
    )
    .await
    .expect("expired tokens must not block a fresh create at cap=1");

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn count_for_user_grows_then_revoke_drops() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let user = make_user(&pool, "u").await;
    assert_eq!(api_tokens::count_for_user(&pool, user).await.unwrap(), 0);

    let t1 = api_tokens::create(
        &pool,
        user,
        "t1",
        &ScopeSet::full_access(),
        None,
        None,
        PREFIX_LEN,
        1000,
    )
    .await
    .unwrap();
    let _t2 = api_tokens::create(
        &pool,
        user,
        "t2",
        &ScopeSet::full_access(),
        None,
        None,
        PREFIX_LEN,
        1000,
    )
    .await
    .unwrap();
    assert_eq!(api_tokens::count_for_user(&pool, user).await.unwrap(), 2);

    let removed = api_tokens::delete_for_user(&pool, user, t1.id)
        .await
        .unwrap();
    assert!(removed);
    assert_eq!(api_tokens::count_for_user(&pool, user).await.unwrap(), 1);

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn rename_only_succeeds_for_owner() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let owner = make_user(&pool, "u").await;
    let other = make_user(&pool, "u").await;
    let tok = api_tokens::create(
        &pool,
        owner,
        "ci",
        &ScopeSet::full_access(),
        None,
        None,
        PREFIX_LEN,
        1000,
    )
    .await
    .unwrap();
    let foreign = api_tokens::rename_for_user(&pool, other, tok.id, "stolen")
        .await
        .unwrap();
    assert!(!foreign, "other user must not rename");
    let mine = api_tokens::rename_for_user(&pool, owner, tok.id, "renamed")
        .await
        .unwrap();
    assert!(mine);

    let listing = api_tokens::list_for_user(&pool, owner).await.unwrap();
    assert_eq!(listing.len(), 1);
    assert_eq!(listing[0].name, "renamed");

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn expired_token_lookup_returns_invalid() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let user = make_user(&pool, "u").await;
    let tok = api_tokens::create(
        &pool,
        user,
        "expiring",
        &ScopeSet::full_access(),
        None,
        None,
        PREFIX_LEN,
        1000,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE api_tokens SET expires_at = now() - INTERVAL '1 minute' WHERE id = $1")
        .bind(tok.id)
        .execute(&pool)
        .await
        .unwrap();
    let outcome = api_tokens::lookup_by_raw(&pool, &tok.token, PREFIX_LEN, None)
        .await
        .unwrap();
    assert!(matches!(outcome, api_tokens::LookupOutcome::Invalid));

    pool.close().await;
    drop_pg(&name).await;
}
