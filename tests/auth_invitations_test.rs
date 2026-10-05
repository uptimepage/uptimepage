//! Live-PG tests for Phase 5: invitation issuance/lookup/accept/decline/expiry.
//!
//! Run via:
//!     docker compose -f compose.dev.yml up -d postgres
//!     DATABASE_URL=postgres://monitor:monitor@localhost:5432/monitor \
//!         cargo nextest run --test it --run-ignored only -E 'test(/^auth_invitations_test::/)'

use crate::common;

use uptimepage::auth::invitations;
use uptimepage::domain::{OrgId, Role, UserId, generate_signup_slug};
use uptimepage::storage::orgs::{
    self as orgs_store, AddMemberOutcome, create_signup_org_with_owner_in_tx,
};
use uuid::Uuid;

use crate::common::MIGRATOR;

async fn fresh_pg() -> Option<(String, String)> {
    common::fresh_test_db("auth_inv").await
}

async fn drop_pg(test_db: &str) {
    common::drop_test_db(test_db).await;
}

async fn open_pool(db_url: &str) -> sqlx::PgPool {
    common::open_test_pool(db_url).await
}

async fn seed_user(pool: &sqlx::PgPool, email: &str) -> UserId {
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO users (email, terms_version, privacy_version) \
         VALUES ($1, 'v1', 'v1') RETURNING id",
    )
    .bind(email)
    .fetch_one(pool)
    .await
    .unwrap();
    UserId(id)
}

async fn seed_org(pool: &sqlx::PgPool, owner: UserId) -> OrgId {
    let mut tx = pool.begin().await.unwrap();
    let org = loop {
        let slug = generate_signup_slug();
        if let Some(o) = create_signup_org_with_owner_in_tx(&mut tx, owner, &slug, "T")
            .await
            .unwrap()
        {
            break o;
        }
    };
    tx.commit().await.unwrap();
    org
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn create_lookup_accept_flow() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let owner = seed_user(&pool, "owner@example.test").await;
    let org = seed_org(&pool, owner).await;

    let created = invitations::create(
        &pool,
        org,
        owner,
        "Alice@Example.test",
        Role::Member,
        168,
        u32::MAX,
    )
    .await
    .expect("create");
    assert!(!created.token.is_empty());

    // Find pending must locate exactly one row by raw token.
    let pending = invitations::find_pending_by_token(&pool, &created.token)
        .await
        .expect("lookup")
        .expect("row");
    assert_eq!(pending.id, created.row.id);
    assert_eq!(pending.role, Role::Member);

    // Recipient signs up (CITEXT — lower-case in users table).
    let alice = seed_user(&pool, "alice@example.test").await;
    let added = orgs_store::add_member(&pool, org, alice, alice, Role::Member, u32::MAX)
        .await
        .unwrap();
    assert_eq!(added, AddMemberOutcome::Added);
    let accepted = invitations::mark_accepted(&pool, org, created.row.id)
        .await
        .unwrap();
    assert!(accepted);

    // Second accept attempt fails — row is no longer pending.
    let again = invitations::mark_accepted(&pool, org, created.row.id)
        .await
        .unwrap();
    assert!(!again);
    let lookup = invitations::find_pending_by_token(&pool, &created.token)
        .await
        .unwrap();
    assert!(
        lookup.is_none(),
        "accepted invitation must not match lookup"
    );

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn resend_reissues_link_and_revives_expired() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let owner = seed_user(&pool, "rot@example.test").await;
    let org = seed_org(&pool, owner).await;
    let created = invitations::create(
        &pool,
        org,
        owner,
        "z@example.test",
        Role::Member,
        168,
        u32::MAX,
    )
    .await
    .unwrap();

    // Expire the link: it no longer matches a token lookup.
    sqlx::query("UPDATE invitations SET expires_at = now() - INTERVAL '1 hour' WHERE id = $1")
        .bind(created.row.id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        invitations::find_pending_by_token(&pool, &created.token)
            .await
            .unwrap()
            .is_none()
    );

    // Pre-check resolves the expired-but-unconsumed row + original inviter.
    let target = invitations::pending_for_resend(&pool, org, created.row.id)
        .await
        .unwrap()
        .expect("resendable");
    assert_eq!(target.email, "z@example.test");
    assert_eq!(target.inviter_id, owner);

    // Persisting a freshly minted token revives the row: old link stays dead,
    // new link resolves the same row.
    let fresh = invitations::generate_raw_token();
    let new_expiry = chrono::Utc::now() + chrono::Duration::hours(168);
    assert!(
        invitations::persist_resend(&pool, org, created.row.id, &fresh, new_expiry)
            .await
            .unwrap()
    );
    assert!(
        invitations::find_pending_by_token(&pool, &created.token)
            .await
            .unwrap()
            .is_none(),
        "old link stays dead after rotation"
    );
    let found = invitations::find_pending_by_token(&pool, &fresh)
        .await
        .unwrap()
        .expect("new token resolves");
    assert_eq!(found.id, created.row.id);

    // A consumed (declined) invitation is neither resendable nor persistable.
    let declined = invitations::create(
        &pool,
        org,
        owner,
        "d@example.test",
        Role::Member,
        168,
        u32::MAX,
    )
    .await
    .unwrap();
    invitations::mark_declined(&pool, org, declined.row.id)
        .await
        .unwrap();
    assert!(
        invitations::pending_for_resend(&pool, org, declined.row.id)
            .await
            .unwrap()
            .is_none()
    );
    let dead = invitations::generate_raw_token();
    assert!(
        !invitations::persist_resend(&pool, org, declined.row.id, &dead, new_expiry)
            .await
            .unwrap()
    );

    // A sibling org can't resend another tenant's invitation.
    let other_owner = seed_user(&pool, "rot-other@example.test").await;
    let other_org = seed_org(&pool, other_owner).await;
    assert!(
        invitations::pending_for_resend(&pool, other_org, created.row.id)
            .await
            .unwrap()
            .is_none()
    );

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn decline_flow() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let owner = seed_user(&pool, "o@example.test").await;
    let org = seed_org(&pool, owner).await;
    let inv = invitations::create(
        &pool,
        org,
        owner,
        "x@example.test",
        Role::Member,
        168,
        u32::MAX,
    )
    .await
    .unwrap();

    let declined = invitations::mark_declined(&pool, org, inv.row.id)
        .await
        .unwrap();
    assert!(declined);

    let lookup = invitations::find_pending_by_token(&pool, &inv.token)
        .await
        .unwrap();
    assert!(lookup.is_none());

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn expired_invitation_is_not_pending() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let owner = seed_user(&pool, "o2@example.test").await;
    let org = seed_org(&pool, owner).await;
    let inv = invitations::create(
        &pool,
        org,
        owner,
        "y@example.test",
        Role::Member,
        168,
        u32::MAX,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE invitations SET expires_at = now() - INTERVAL '1 hour' WHERE id = $1")
        .bind(inv.row.id)
        .execute(&pool)
        .await
        .unwrap();

    let lookup = invitations::find_pending_by_token(&pool, &inv.token)
        .await
        .unwrap();
    assert!(lookup.is_none());
    assert!(
        !invitations::mark_accepted(&pool, org, inv.row.id)
            .await
            .unwrap(),
        "expired must not accept"
    );

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn already_invited_blocks_second_pending() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let owner = seed_user(&pool, "o3@example.test").await;
    let org = seed_org(&pool, owner).await;
    invitations::create(
        &pool,
        org,
        owner,
        "z@example.test",
        Role::Member,
        168,
        u32::MAX,
    )
    .await
    .unwrap();
    assert!(
        invitations::exists_pending_for_email(&pool, org, "Z@Example.test")
            .await
            .unwrap(),
        "CITEXT match must catch the second invite",
    );

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn purge_old_drops_settled_and_expired_rows() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let owner = seed_user(&pool, "o4@example.test").await;
    let org = seed_org(&pool, owner).await;
    let old_accepted = invitations::create(
        &pool,
        org,
        owner,
        "old1@example.test",
        Role::Member,
        168,
        u32::MAX,
    )
    .await
    .unwrap();
    let old_expired = invitations::create(
        &pool,
        org,
        owner,
        "old2@example.test",
        Role::Member,
        168,
        u32::MAX,
    )
    .await
    .unwrap();
    let fresh = invitations::create(
        &pool,
        org,
        owner,
        "fresh@example.test",
        Role::Member,
        168,
        u32::MAX,
    )
    .await
    .unwrap();

    sqlx::query("UPDATE invitations SET accepted_at = now() - INTERVAL '90 days' WHERE id = $1")
        .bind(old_accepted.row.id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE invitations SET expires_at = now() - INTERVAL '90 days' WHERE id = $1")
        .bind(old_expired.row.id)
        .execute(&pool)
        .await
        .unwrap();

    let removed = invitations::purge_old(&pool, 30).await.unwrap();
    assert!(
        removed >= 2,
        "expected the two old rows to be removed (got {removed})"
    );

    let (n_fresh,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM invitations WHERE id = $1")
        .bind(fresh.row.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n_fresh, 1, "fresh row must survive");

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn mark_accepted_and_declined_require_matching_org() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let owner = seed_user(&pool, "tenant_a@example.test").await;
    let org_a = seed_org(&pool, owner).await;
    let attacker_owner = seed_user(&pool, "tenant_b@example.test").await;
    let org_b = seed_org(&pool, attacker_owner).await;

    let inv = invitations::create(
        &pool,
        org_a,
        owner,
        "victim@example.test",
        Role::Member,
        168,
        u32::MAX,
    )
    .await
    .unwrap();

    assert!(
        !invitations::mark_accepted(&pool, org_b, inv.row.id)
            .await
            .unwrap(),
        "wrong-org accept must be a no-op",
    );
    assert!(
        !invitations::mark_declined(&pool, org_b, inv.row.id)
            .await
            .unwrap(),
        "wrong-org decline must be a no-op",
    );
    assert!(
        invitations::find_pending_by_token(&pool, &inv.token)
            .await
            .unwrap()
            .is_some(),
        "invitation must still be pending after cross-tenant attempts",
    );

    assert!(
        invitations::mark_accepted(&pool, org_a, inv.row.id)
            .await
            .unwrap(),
        "matching-org accept must succeed",
    );

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn find_pending_by_id_honors_pending_guards() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let owner = seed_user(&pool, "owner-byid@example.test").await;
    let org = seed_org(&pool, owner).await;
    let created = invitations::create(
        &pool,
        org,
        owner,
        "byid@example.test",
        Role::Member,
        168,
        50,
    )
    .await
    .unwrap();

    let found = invitations::find_pending_by_id(&pool, created.row.id)
        .await
        .unwrap()
        .expect("pending row visible by id");
    assert_eq!(found.email, "byid@example.test");
    assert_eq!(found.org_id, org);

    // Declined → invisible.
    assert!(
        invitations::mark_declined(&pool, org, created.row.id)
            .await
            .unwrap()
    );
    assert!(
        invitations::find_pending_by_id(&pool, created.row.id)
            .await
            .unwrap()
            .is_none()
    );

    // Expired → invisible.
    let expired = invitations::create(
        &pool,
        org,
        owner,
        "byid2@example.test",
        Role::Member,
        168,
        50,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE invitations SET expires_at = now() - INTERVAL '1 minute' WHERE id = $1")
        .bind(expired.row.id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        invitations::find_pending_by_id(&pool, expired.row.id)
            .await
            .unwrap()
            .is_none()
    );

    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn revoking_does_not_buy_another_send() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let owner = seed_user(&pool, "spray@example.test").await;
    let org = seed_org(&pool, owner).await;

    // Revoking frees a slot under the stock cap. It must not free the send.
    let mut ids = Vec::new();
    for i in 0..invitations::MAX_SENDS_PER_WINDOW {
        invitations::ensure_send_window(&pool, org, owner)
            .await
            .expect("under the window");
        let created = invitations::create(
            &pool,
            org,
            owner,
            &format!("t{i}@example.test"),
            Role::Member,
            168,
            u32::MAX,
        )
        .await
        .expect("create");
        invitations::record_send(&pool, org, owner, created.row.id)
            .await
            .expect("record");
        ids.push(created.row.id);
    }
    for id in &ids {
        invitations::revoke(&pool, org, *id).await.expect("revoke");
    }
    let (pending,): (i64,) = sqlx::query_as("SELECT count(*) FROM invitations WHERE org_id = $1")
        .bind(org.0)
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(pending, 0, "every row is gone, so the stock cap is clear");

    let err = invitations::ensure_send_window(&pool, org, owner)
        .await
        .expect_err("the window still remembers");
    assert!(
        format!("{err:?}").contains("INVITATION_SEND_LIMIT"),
        "{err:?}"
    );

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_send_reaches_the_org_trail() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let owner = seed_user(&pool, "trail@example.test").await;
    let org = seed_org(&pool, owner).await;
    let created = invitations::create(
        &pool,
        org,
        owner,
        "guest@example.test",
        Role::Member,
        168,
        u32::MAX,
    )
    .await
    .expect("create");
    invitations::record_send(&pool, org, owner, created.row.id)
        .await
        .expect("record");

    // The count that bounds sending reads from here.
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT action FROM org_audit_log WHERE org_id = $1 AND action = 'invitation.sent'",
    )
    .bind(org.0)
    .fetch_all(&pool)
    .await
    .expect("read trail");
    assert_eq!(rows.len(), 1, "one send, one row");

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_window_forgets_what_fell_out_of_it() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let owner = seed_user(&pool, "aged@example.test").await;
    let org = seed_org(&pool, owner).await;

    for i in 0..invitations::MAX_SENDS_PER_WINDOW {
        let created = invitations::create(
            &pool,
            org,
            owner,
            &format!("a{i}@example.test"),
            Role::Member,
            168,
            u32::MAX,
        )
        .await
        .expect("create");
        invitations::record_send(&pool, org, owner, created.row.id)
            .await
            .expect("record");
        invitations::revoke(&pool, org, created.row.id)
            .await
            .expect("revoke");
    }
    invitations::ensure_send_window(&pool, org, owner)
        .await
        .expect_err("at the ceiling");

    // A ceiling on abuse, not a permanent one: age the trail past the window.
    sqlx::query(
        "UPDATE org_audit_log SET occurred_at = occurred_at - make_interval(hours => $2) \
         WHERE org_id = $1 AND action = 'invitation.sent'",
    )
    .bind(org.0)
    .bind(i32::try_from(invitations::SEND_WINDOW_HOURS + 1).unwrap())
    .execute(&pool)
    .await
    .expect("age the trail");

    invitations::ensure_send_window(&pool, org, owner)
        .await
        .expect("the window has moved on");

    pool.close().await;
    drop_pg(&name).await;
}
