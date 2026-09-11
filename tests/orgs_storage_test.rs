//! Live-Postgres tests for the org-management storage layer. Skipped at the
//! `cargo test` default; runs under `--include-ignored` once `DATABASE_URL` is
//! set. The tests share a database with the rest of the live-PG suite, so
//! every test seeds its own users + orgs with `Uuid::now_v7()` slugs and
//! cleans up via `ON DELETE CASCADE` from a final `DELETE FROM users`.

mod common;

use std::time::Duration;

use common::{default_http_check, make_user, unique_slug};
use uptimepage::domain::{
    CheckSpec, ExpectedStatus, NewStatusPage, NewTarget, OrgId, Role, UserId, WriteSource,
};
use uptimepage::storage::orgs as orgs_store;
use uptimepage::storage::{
    DeleteOutcome, PgStatusPageStore, PostgresTargetStore, RemoveOutcome, RestoreOutcome,
    StatusPageStore, TargetStore, UpdateOrgOutcome, create_org_with_owner, is_active_member,
    is_owner, list_deleted_orgs_deleted_by, list_members, list_orgs_for_user,
    oldest_membership_for_user, remove_member, restore_org, slug_is_available, soft_delete_org,
    soft_delete_org_for_user, update_org_fields,
};
use url::Url;
use uuid::Uuid;

#[tokio::test]
#[ignore]
async fn create_org_and_slug_check_round_trip() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "orgs").await;
    let slug = unique_slug("acme");

    assert!(slug_is_available(&pool, &slug).await.unwrap());
    let created = create_org_with_owner(&pool, user, &slug, "Acme")
        .await
        .unwrap()
        .expect("created");
    assert_eq!(created.slug, slug.to_ascii_lowercase());
    assert!(!slug_is_available(&pool, &slug).await.unwrap());

    // Second attempt at the same slug collides → None.
    let collision = create_org_with_owner(&pool, user, &slug, "Acme2")
        .await
        .unwrap();
    assert!(collision.is_none(), "expected slug-collision None");

    // Caller is now an active member + owner.
    assert!(is_active_member(&pool, user, created.id).await.unwrap());
    assert!(is_owner(&pool, user, created.id).await.unwrap());

    // Cleanup
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}

/// A tombstone frees the org's monitors from the account's pool, so a restore
/// has to re-earn them. Without this the deleted org is a parking space:
/// fill it, delete it, refill the pool elsewhere, restore, and the account
/// runs at double its plan.
#[tokio::test]
#[ignore]
async fn restoring_an_org_cannot_smuggle_its_monitors_past_the_pool() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "orgs").await;
    let parked = create_org_with_owner(&pool, user, &unique_slug("park"), "Parked")
        .await
        .unwrap()
        .expect("first org");
    let live = create_org_with_owner(&pool, user, &unique_slug("live"), "Live")
        .await
        .unwrap()
        .expect("second org");

    // Cap the account at four monitors, then spend all four in the org that
    // is about to be tombstoned.
    common::set_account_targets_cap(&pool, user, 4).await;
    common::seed_targets(&pool, parked.id, 4).await;
    soft_delete_org_for_user(&pool, parked.id, user)
        .await
        .unwrap();

    // The tombstone released the pool, so the surviving org can spend it again.
    common::seed_targets(&pool, live.id, 4).await;

    let outcome = restore_org(
        &pool,
        parked.id,
        user,
        30,
        &common::plan_for(&pool, parked.id).await,
    )
    .await
    .unwrap();
    match outcome {
        RestoreOutcome::QuotaExceeded {
            quota,
            current,
            limit,
        } => {
            assert_eq!(quota, "max_targets");
            assert_eq!((current, limit), (8, 4));
        }
        other => panic!("restore must not double the pool, got {other:?}"),
    }

    // Refused means refused: the org is still tombstoned and the pool intact.
    let (deleted, live_targets): (bool, i64) = sqlx::query_as(
        "SELECT (SELECT deleted_at IS NOT NULL FROM organizations WHERE id = $1), \
                (SELECT count(*) FROM targets t JOIN organizations o ON o.id = t.org_id \
                  WHERE o.deleted_at IS NULL AND o.id = ANY($2))",
    )
    .bind(parked.id.0)
    .bind(vec![parked.id.0, live.id.0])
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(deleted, "the refused restore must leave the tombstone");
    assert_eq!(live_targets, 4, "the live pool is unchanged");

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}

/// The plan is the only source of the org cap: with no override in play, a
/// free account holds exactly what `plans.max_orgs` says.
#[tokio::test]
#[ignore]
async fn free_account_org_cap_comes_from_the_plan() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "orgs").await;
    create_org_with_owner(&pool, user, &unique_slug("plan"), "one")
        .await
        .unwrap()
        .expect("first org");
    common::plan_default_orgs(&pool, user).await;

    let cap: i32 = sqlx::query_scalar("SELECT max_orgs FROM plans WHERE id = 'free'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(cap, 1, "free plan holds one org");

    let err = create_org_with_owner(&pool, user, &unique_slug("plan"), "two")
        .await
        .unwrap_err();
    assert!(format!("{err:?}").contains("OWNER_ORG_LIMIT"), "{err:?}");
    assert_eq!(common::live_orgs(&pool, user).await, 1);

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn owner_org_limit_is_atomic() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "orgs").await;
    // Cap 2 — first two creates succeed, third fails with OWNER_ORG_LIMIT.
    common::allow_orgs(&pool, user, 2).await;
    let s1 = unique_slug("lim");
    let s2 = unique_slug("lim");
    let s3 = unique_slug("lim");
    create_org_with_owner(&pool, user, &s1, "one")
        .await
        .unwrap()
        .expect("first");
    create_org_with_owner(&pool, user, &s2, "two")
        .await
        .unwrap()
        .expect("second");
    let err = create_org_with_owner(&pool, user, &s3, "three")
        .await
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("OWNER_ORG_LIMIT"), "got {msg}");

    assert_eq!(common::live_orgs(&pool, user).await, 2);

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn list_orgs_excludes_soft_deleted_and_update_name_works() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "orgs").await;
    let slug = unique_slug("list");
    let org = create_org_with_owner(&pool, user, &slug, "Old name")
        .await
        .unwrap()
        .unwrap();

    let listed = list_orgs_for_user(&pool, user).await.unwrap();
    assert!(
        listed
            .iter()
            .any(|o| o.org.id == org.id && o.role == Role::Owner)
    );

    let renamed = match update_org_fields(&pool, org.id, user, Some("New name"), None)
        .await
        .unwrap()
    {
        UpdateOrgOutcome::Updated(o) => o,
        other => panic!("expected Updated, got {other:?}"),
    };
    assert_eq!(renamed.name, "New name");

    assert!(soft_delete_org(&pool, org.id, user).await.unwrap());
    // Now hidden from active list, surfaced in deleted list.
    let listed = list_orgs_for_user(&pool, user).await.unwrap();
    assert!(listed.iter().all(|o| o.org.id != org.id));
    let deleted = list_deleted_orgs_deleted_by(&pool, user).await.unwrap();
    assert!(deleted.iter().any(|o| o.id == org.id));

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn restore_org_inside_window_succeeds() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "orgs").await;
    let slug = unique_slug("rest");
    let org = create_org_with_owner(&pool, user, &slug, "n")
        .await
        .unwrap()
        .unwrap();
    soft_delete_org(&pool, org.id, user).await.unwrap();

    let outcome = restore_org(
        &pool,
        org.id,
        user,
        30,
        &common::plan_for(&pool, org.id).await,
    )
    .await
    .unwrap();
    assert!(matches!(outcome, RestoreOutcome::Restored(ref o) if o.id == org.id));
    assert!(is_active_member(&pool, user, org.id).await.unwrap());

    // Restoring an already-active org reports NotDeleted, not Restored.
    let again = restore_org(
        &pool,
        org.id,
        user,
        30,
        &common::plan_for(&pool, org.id).await,
    )
    .await
    .unwrap();
    assert!(matches!(again, RestoreOutcome::NotDeleted));

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn restore_org_outside_window_refuses() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "orgs").await;
    let slug = unique_slug("expr");
    let org = create_org_with_owner(&pool, user, &slug, "n")
        .await
        .unwrap()
        .unwrap();
    soft_delete_org(&pool, org.id, user).await.unwrap();
    // Backdate the deletion past the grace window.
    sqlx::query("UPDATE organizations SET deleted_at = now() - INTERVAL '40 days' WHERE id = $1")
        .bind(org.id.0)
        .execute(&pool)
        .await
        .unwrap();

    let outcome = restore_org(
        &pool,
        org.id,
        user,
        30,
        &common::plan_for(&pool, org.id).await,
    )
    .await
    .unwrap();
    assert!(matches!(outcome, RestoreOutcome::WindowExpired));

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn remove_member_refuses_last_owner() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "orgs").await;
    let slug = unique_slug("solo");
    let org = create_org_with_owner(&pool, user, &slug, "Solo")
        .await
        .unwrap()
        .unwrap();

    let outcome = remove_member(&pool, org.id, user, user).await.unwrap();
    assert_eq!(outcome, RemoveOutcome::LastOwner);
    assert!(is_owner(&pool, user, org.id).await.unwrap(), "still owner");

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn remove_member_refuses_the_owner_of_the_paying_account() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let payer = make_user(&pool, "orgs").await;
    let other = make_user(&pool, "orgs").await;
    let org = create_org_with_owner(&pool, payer, &unique_slug("paid"), "Paid")
        .await
        .unwrap()
        .unwrap();
    sqlx::query("INSERT INTO memberships (user_id, org_id, role) VALUES ($1, $2, 'owner')")
        .bind(other.0)
        .bind(org.id.0)
        .execute(&pool)
        .await
        .unwrap();

    assert_eq!(
        remove_member(&pool, org.id, payer, payer).await.unwrap(),
        RemoveOutcome::AccountOwner
    );
    assert_eq!(
        remove_member(&pool, org.id, other, payer).await.unwrap(),
        RemoveOutcome::AccountOwner
    );
    assert!(is_owner(&pool, payer, org.id).await.unwrap());
    assert_eq!(
        remove_member(&pool, org.id, payer, other).await.unwrap(),
        RemoveOutcome::Removed
    );

    sqlx::query("DELETE FROM users WHERE id = ANY($1)")
        .bind(&[payer.0, other.0][..])
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn remove_member_succeeds_when_other_owners_exist() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let a = make_user(&pool, "orgs").await;
    let b = make_user(&pool, "orgs").await;
    let slug = unique_slug("pair");
    let org = create_org_with_owner(&pool, a, &slug, "Pair")
        .await
        .unwrap()
        .unwrap();
    // Manually add `b` as a second owner (no public API yet).
    sqlx::query("INSERT INTO memberships (user_id, org_id, role) VALUES ($1, $2, 'owner')")
        .bind(b.0)
        .bind(org.id.0)
        .execute(&pool)
        .await
        .unwrap();

    let outcome = remove_member(&pool, org.id, a, b).await.unwrap();
    assert_eq!(outcome, RemoveOutcome::Removed);
    let members = list_members(&pool, org.id).await.unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].membership.user_id, a);

    sqlx::query("DELETE FROM users WHERE id = ANY($1)")
        .bind(&[a.0, b.0][..])
        .execute(&pool)
        .await
        .unwrap();
}

fn http_target(name: &str, owner: Option<UserId>) -> NewTarget {
    NewTarget {
        name: name.into(),
        check: CheckSpec::Http(default_http_check(
            Url::parse("https://example.com/").unwrap(),
            ExpectedStatus::Exact(200),
        )),
        interval: Duration::from_secs(60),
        enabled: true,
        tags: vec![],
        alerts: Default::default(),
        region_policy: Default::default(),
        alert_confirmations: 2,
        notify_recovery: true,
        renotify_interval_secs: 3600,
        group_name: None,
        owner_user_id: owner.map(|u| u.0),
        regions: None,
    }
}

/// A monitor's owner is a member by definition, so the two leave together.
#[tokio::test]
#[ignore]
async fn removing_a_member_clears_the_monitors_they_owned() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let owner = make_user(&pool, "orgs-own").await;
    let member = make_user(&pool, "orgs-own").await;
    let org = create_org_with_owner(&pool, owner, &unique_slug("own"), "Own")
        .await
        .unwrap()
        .unwrap();
    sqlx::query("INSERT INTO memberships (user_id, org_id, role) VALUES ($1, $2, 'member')")
        .bind(member.0)
        .bind(org.id.0)
        .execute(&pool)
        .await
        .unwrap();

    let store = PostgresTargetStore::from_pool(pool.clone(), None);
    let theirs = store
        .create(
            org.id,
            http_target("theirs", Some(member)),
            WriteSource::Ui,
            i64::MAX,
            i64::MAX,
        )
        .await
        .unwrap()
        .id;
    let untouched = store
        .create(
            org.id,
            http_target("the owner's", Some(owner)),
            WriteSource::Ui,
            i64::MAX,
            i64::MAX,
        )
        .await
        .unwrap()
        .id;

    assert_eq!(
        remove_member(&pool, org.id, owner, member).await.unwrap(),
        RemoveOutcome::Removed
    );

    let owners: std::collections::HashMap<Uuid, Option<Uuid>> =
        sqlx::query_as("SELECT id, owner_user_id FROM targets WHERE id = ANY($1)")
            .bind(&[theirs, untouched][..])
            .fetch_all(&pool)
            .await
            .unwrap()
            .into_iter()
            .collect();
    assert_eq!(
        owners.len(),
        2,
        "removal disowns a monitor, never deletes it"
    );
    assert_eq!(owners[&theirs], None, "the owner left the org");
    assert_eq!(owners[&untouched], Some(owner.0), "somebody else's monitor");

    sqlx::query("DELETE FROM users WHERE id = ANY($1)")
        .bind(&[owner.0, member.0][..])
        .execute(&pool)
        .await
        .unwrap();
}

/// The write-time check runs on another connection, so the constraint is what
/// stops a monitor being stamped with somebody from another tenant.
#[tokio::test]
#[ignore]
async fn a_monitor_cannot_name_an_owner_from_outside_the_org() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let owner = make_user(&pool, "orgs-out").await;
    let outsider = make_user(&pool, "orgs-out").await;
    let org = create_org_with_owner(&pool, owner, &unique_slug("out"), "Out")
        .await
        .unwrap()
        .unwrap();
    let store = PostgresTargetStore::from_pool(pool.clone(), None);
    let mine = store
        .create(
            org.id,
            http_target("mine", Some(owner)),
            WriteSource::Ui,
            i64::MAX,
            i64::MAX,
        )
        .await
        .unwrap()
        .id;

    let rejected = sqlx::query("UPDATE targets SET owner_user_id = $2 WHERE id = $1")
        .bind(mine)
        .bind(outsider.0)
        .execute(&pool)
        .await
        .expect_err("a non-member cannot own this monitor");
    let db = rejected
        .as_database_error()
        .expect("a constraint refused it");
    assert_eq!(db.code().as_deref(), Some("23503"));
    assert_eq!(db.constraint(), Some("targets_owner_is_member_fk"));

    sqlx::query("DELETE FROM users WHERE id = ANY($1)")
        .bind(&[owner.0, outsider.0][..])
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn default_org_lookup_returns_none_for_user_without_memberships() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    // A brand-new user has no memberships → no default org until signup
    // auto-creates one.
    let user = make_user(&pool, "orgs").await;
    assert!(
        oldest_membership_for_user(&pool, user)
            .await
            .unwrap()
            .is_none()
    );

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}

/// 50 simultaneous create-org attempts for one user with `owner_limit = 3`
/// must produce exactly 3 winners. The cap is enforced by the count-subquery
/// inside the membership INSERT, so MVCC row-level locking on
/// `(user_id, org_id)` PK serialises the writes — no row needs to know about
/// any other attempt's outcome.
#[tokio::test]
#[ignore]
async fn owner_limit_holds_under_50_concurrent_creates() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "orgs").await;

    let limit: u32 = 3;
    common::allow_orgs(&pool, user, i32::try_from(limit).unwrap()).await;
    let attempts: usize = 50;
    let mut tasks = Vec::with_capacity(attempts);
    for i in 0..attempts {
        let pool = pool.clone();
        let slug = unique_slug(&format!("race-{i:02}"));
        tasks.push(tokio::spawn(async move {
            create_org_with_owner(&pool, user, &slug, "n").await
        }));
    }

    let mut created = 0u32;
    let mut over_limit = 0u32;
    for t in tasks {
        match t.await.unwrap() {
            Ok(Some(_)) => created += 1,
            Ok(None) => {} // slug collision shouldn't happen with unique slugs
            Err(e) if format!("{e:?}").contains("OWNER_ORG_LIMIT") => over_limit += 1,
            Err(e) => panic!("unexpected error: {e:?}"),
        }
    }

    assert_eq!(created, limit, "exactly `limit` should win");
    assert_eq!(
        over_limit,
        u32::try_from(attempts).unwrap() - limit,
        "every loser should report OWNER_ORG_LIMIT"
    );
    assert_eq!(common::live_orgs(&pool, user).await, limit);

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}

/// Models the signup retry loop: user_a holds the generated slug X, user_b
/// then collides on X and must succeed on the next `generate_signup_slug`
/// draw. Uses an explicit fixed slug for user_a instead of reseeding
/// `fastrand`'s thread-local RNG — seed pollution would leak into sibling
/// tests sharing the test binary's threads.
#[tokio::test]
#[ignore]
async fn signup_org_collision_retry_succeeds() {
    use uptimepage::domain::generate_signup_slug;

    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user_a = make_user(&pool, "orgs").await;
    let user_b = make_user(&pool, "orgs").await;

    // Fixed slug distinct enough that two concurrent test runs won't collide.
    let suffix = &Uuid::new_v4().simple().to_string()[..6];
    let collided = format!("fixed-test-{suffix}");
    let _ = create_org_with_owner(&pool, user_a, &collided, "A")
        .await
        .unwrap()
        .expect("first user takes the slug");

    // user_b tries the same slug and collides.
    let none = create_org_with_owner(&pool, user_b, &collided, "B")
        .await
        .unwrap();
    assert!(none.is_none(), "expected slug-collision None");

    // Retry: a fresh draw from the generator yields a distinct slug user_b
    // can claim. Five attempts mirror the signup transaction's retry budget.
    let mut succeeded = false;
    for _ in 0..5 {
        let retry_slug = generate_signup_slug();
        if retry_slug == collided {
            continue;
        }
        if let Some(o) = create_org_with_owner(&pool, user_b, &retry_slug, "B")
            .await
            .unwrap()
        {
            assert_eq!(o.slug, retry_slug.to_ascii_lowercase());
            succeeded = true;
            break;
        }
    }
    assert!(succeeded, "retry should win within 5 attempts");

    for u in [user_a, user_b] {
        sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(u.0)
            .execute(&pool)
            .await
            .unwrap();
    }
}

#[tokio::test]
#[ignore]
async fn update_org_slug_happy_path_records_audit() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "slug").await;
    let from = unique_slug("from");
    let org = create_org_with_owner(&pool, user, &from, "Co")
        .await
        .unwrap()
        .unwrap();
    let to = unique_slug("to");

    let outcome = update_org_fields(&pool, org.id, user, None, Some(&to))
        .await
        .unwrap();
    let renamed = match outcome {
        UpdateOrgOutcome::Updated(o) => o,
        other => panic!("expected Updated, got {other:?}"),
    };
    assert_eq!(renamed.slug, to);
    assert_eq!(renamed.id, org.id);
    assert!(
        slug_is_available(&pool, &from).await.unwrap(),
        "old slug freed"
    );
    assert!(!slug_is_available(&pool, &to).await.unwrap());

    let (action, meta): (String, serde_json::Value) = sqlx::query_as(
        "SELECT action, metadata FROM org_audit_log \
         WHERE org_id = $1 AND action = 'org.slug_changed' \
         ORDER BY occurred_at DESC LIMIT 1",
    )
    .bind(org.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(action, "org.slug_changed");
    assert_eq!(meta["from"], from);
    assert_eq!(meta["to"], to);

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn update_org_slug_same_slug_is_noop_updated() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "slug").await;
    let slug = unique_slug("same");
    let org = create_org_with_owner(&pool, user, &slug, "Co")
        .await
        .unwrap()
        .unwrap();

    let outcome = update_org_fields(&pool, org.id, user, None, Some(&slug))
        .await
        .unwrap();
    assert!(matches!(outcome, UpdateOrgOutcome::Updated(o) if o.slug == slug));

    // No `org.slug_changed` audit row written for a no-op (combined fn only
    // audits actually-changed fields).
    let (count,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM org_audit_log WHERE org_id = $1 AND action = 'org.slug_changed'",
    )
    .bind(org.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 0, "no-op slug must not write audit");

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn update_org_slug_conflict_returns_slug_taken() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "slug").await;
    let mine = unique_slug("mine");
    let taken = unique_slug("taken");
    create_org_with_owner(&pool, user, &taken, "T")
        .await
        .unwrap()
        .unwrap();
    let org = create_org_with_owner(&pool, user, &mine, "M")
        .await
        .unwrap()
        .unwrap();

    let outcome = update_org_fields(&pool, org.id, user, None, Some(&taken))
        .await
        .unwrap();
    assert!(matches!(outcome, UpdateOrgOutcome::SlugTaken));
    // Original row's slug is unchanged.
    let still = orgs_store::get_org(&pool, org.id).await.unwrap().unwrap();
    assert_eq!(still.slug, mine);

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn update_org_slug_not_found_on_missing_or_soft_deleted() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "slug").await;
    let target = unique_slug("x");
    // Missing org.
    let outcome = update_org_fields(&pool, OrgId(Uuid::now_v7()), user, None, Some(&target))
        .await
        .unwrap();
    assert!(matches!(outcome, UpdateOrgOutcome::NotFound));

    // Soft-deleted org.
    let org = create_org_with_owner(&pool, user, &unique_slug("del"), "D")
        .await
        .unwrap()
        .unwrap();
    assert!(soft_delete_org(&pool, org.id, user).await.unwrap());
    let after = unique_slug("after");
    let outcome = update_org_fields(&pool, org.id, user, None, Some(&after))
        .await
        .unwrap();
    assert!(matches!(outcome, UpdateOrgOutcome::NotFound));

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn update_org_fields_combined_name_and_slug_is_atomic() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "slug").await;
    let from = unique_slug("comb");
    let org = create_org_with_owner(&pool, user, &from, "Old name")
        .await
        .unwrap()
        .unwrap();
    let to = unique_slug("comb-new");

    let outcome = update_org_fields(&pool, org.id, user, Some("New name"), Some(&to))
        .await
        .unwrap();
    let updated = match outcome {
        UpdateOrgOutcome::Updated(o) => o,
        other => panic!("expected Updated, got {other:?}"),
    };
    assert_eq!(updated.name, "New name");
    assert_eq!(updated.slug, to);

    // Both audit rows written in the same tx.
    let actions: Vec<(String,)> = sqlx::query_as(
        "SELECT action FROM org_audit_log WHERE org_id = $1 AND action IN ('org.slug_changed', 'org.renamed')",
    )
    .bind(org.id.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    let names: std::collections::HashSet<_> = actions.into_iter().map(|(a,)| a).collect();
    assert!(names.contains("org.slug_changed"));
    assert!(names.contains("org.renamed"));

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}

// --- Future-rename safety: the org slug is cosmetic, ids carry the data. ---
// These pin the invariants a user-facing slug rename will rely on, even though
// the rename has no UI yet (API-only via `update_org_fields` / PATCH /orgs).

// (1) Decoupling: renaming the org slug must NOT change a status-page slug.
// A page carries its own editable slug, independent of the org slug.
#[tokio::test]
#[ignore]
async fn update_org_slug_does_not_touch_status_page_slug() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "slug").await;
    let from = unique_slug("decouple");
    let org = create_org_with_owner(&pool, user, &from, "Co")
        .await
        .unwrap()
        .unwrap();

    // The page carries its own slug, independent of the org slug.
    let page_store = PgStatusPageStore::new(pool.clone());
    let page_slug = unique_slug("page");
    let page = page_store
        .create(
            org.id,
            NewStatusPage {
                slug: page_slug.clone(),
                name: "P".into(),
                enabled: true,
            },
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .unwrap()
        .unwrap();

    let to = unique_slug("decouple-new");
    let outcome = update_org_fields(&pool, org.id, user, None, Some(&to))
        .await
        .unwrap();
    assert!(matches!(outcome, UpdateOrgOutcome::Updated(o) if o.slug == to));

    // The page's slug is unchanged — decoupled from the org slug.
    let after = page_store
        .get(org.id, page.id)
        .await
        .unwrap()
        .expect("page still there");
    assert_eq!(
        after.slug, page_slug,
        "status-page slug must not follow the org-slug rename"
    );

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}

// (2) No orphan: org-scoped resources are keyed by org_id (UUID), not slug, so
// they stay reachable after a slug rename. Proven with a target; channels and
// pages share the identical `org_id` FK pattern.
#[tokio::test]
#[ignore]
async fn update_org_slug_keeps_resources_reachable_by_id() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "slug").await;
    let from = unique_slug("orphan");
    let org = create_org_with_owner(&pool, user, &from, "Co")
        .await
        .unwrap()
        .unwrap();

    let store = PostgresTargetStore::from_pool(pool.clone(), None);
    let new = NewTarget {
        name: "mon".into(),
        check: CheckSpec::Http(default_http_check(
            Url::parse("https://example.com/").unwrap(),
            ExpectedStatus::Exact(200),
        )),
        interval: Duration::from_secs(30),
        enabled: true,
        tags: vec![],
        alerts: Default::default(),
        region_policy: Default::default(),
        alert_confirmations: 2,
        notify_recovery: true,
        renotify_interval_secs: 3600,
        group_name: None,
        owner_user_id: None,
        regions: None,
    };
    let target = store
        .create(org.id, new, WriteSource::Ui, i64::MAX, i64::MAX)
        .await
        .unwrap();

    let to = unique_slug("orphan-new");
    let outcome = update_org_fields(&pool, org.id, user, None, Some(&to))
        .await
        .unwrap();
    assert!(matches!(outcome, UpdateOrgOutcome::Updated(o) if o.slug == to));

    // Target still reachable by (org_id, id) — the rename didn't orphan it.
    let still = store.get(org.id, target.id).await.unwrap();
    assert!(
        still.is_some(),
        "target must survive an org-slug rename (keyed by org_id)"
    );

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}

// (3) Resolver tracks the rename: `find_id_by_slug` is exactly what
// `optional_org_from_header` uses to turn `X-Uptimepage-Org` into an OrgId.
// After a rename the new slug resolves and the old slug stops resolving (which
// surfaces to API tokens as `ORG_HEADER_INVALID`).
#[tokio::test]
#[ignore]
async fn update_org_slug_resolved_by_find_id_by_slug() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "slug").await;
    let from = unique_slug("resolve");
    let org = create_org_with_owner(&pool, user, &from, "Co")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        orgs_store::find_id_by_slug(&pool, &from).await.unwrap(),
        Some(org.id),
        "old slug resolves before rename"
    );

    let to = unique_slug("resolve-new");
    let outcome = update_org_fields(&pool, org.id, user, None, Some(&to))
        .await
        .unwrap();
    assert!(matches!(outcome, UpdateOrgOutcome::Updated(o) if o.slug == to));

    // New slug resolves; old slug no longer matches any org.
    assert_eq!(
        orgs_store::find_id_by_slug(&pool, &to).await.unwrap(),
        Some(org.id),
        "new slug resolves after rename"
    );
    assert_eq!(
        orgs_store::find_id_by_slug(&pool, &from).await.unwrap(),
        None,
        "old slug stops resolving (→ ORG_HEADER_INVALID for API tokens)"
    );

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}

// Silence unused-import warnings when none of the live-PG tests run because
// `DATABASE_URL` is unset (every body early-returns).
#[allow(dead_code, unused_imports)]
fn _imports_used() {
    let _ = orgs_store::is_active_member;
    let _: Option<OrgId> = None;
}

#[tokio::test]
#[ignore]
async fn owner_delete_refuses_the_callers_last_org() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "orgs").await;
    let first = create_org_with_owner(&pool, user, &unique_slug("solo"), "Solo")
        .await
        .unwrap()
        .unwrap();

    // Sole org: refused, and still active afterwards.
    let outcome = soft_delete_org_for_user(&pool, first.id, user)
        .await
        .unwrap();
    assert_eq!(outcome, DeleteOutcome::LastOrg);
    assert!(is_active_member(&pool, user, first.id).await.unwrap());

    // With a sibling in place it goes through.
    let second = create_org_with_owner(&pool, user, &unique_slug("pair"), "Pair")
        .await
        .unwrap()
        .unwrap();
    let outcome = soft_delete_org_for_user(&pool, first.id, user)
        .await
        .unwrap();
    assert_eq!(outcome, DeleteOutcome::Deleted);
    assert!(!is_active_member(&pool, user, first.id).await.unwrap());

    // The survivor is now itself undeletable.
    assert_eq!(
        soft_delete_org_for_user(&pool, second.id, user)
            .await
            .unwrap(),
        DeleteOutcome::LastOrg
    );

    // Already tombstoned is NotFound, not LastOrg.
    assert_eq!(
        soft_delete_org_for_user(&pool, first.id, user)
            .await
            .unwrap(),
        DeleteOutcome::NotFound
    );

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn deleting_an_org_repoints_sessions_pinned_to_it() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "orgs").await;
    let keep = create_org_with_owner(&pool, user, &unique_slug("keep"), "Keep")
        .await
        .unwrap()
        .unwrap();
    let doomed = create_org_with_owner(&pool, user, &unique_slug("doom"), "Doomed")
        .await
        .unwrap()
        .unwrap();

    let live = format!("live-{}", Uuid::now_v7());
    let expired = format!("expired-{}", Uuid::now_v7());
    for (hash, ttl) in [(&live, "1 day"), (&expired, "-1 day")] {
        sqlx::query(
            "INSERT INTO sessions (id_hash, user_id, active_org_id, expires_at) \
             VALUES ($1, $2, $3, now() + $4::interval)",
        )
        .bind(hash)
        .bind(user.0)
        .bind(doomed.id.0)
        .bind(ttl)
        .execute(&pool)
        .await
        .unwrap();
    }

    soft_delete_org(&pool, doomed.id, user).await.unwrap();

    // Otherwise the session names a tombstone and every page 403s.
    let (moved,): (Option<Uuid>,) =
        sqlx::query_as("SELECT active_org_id FROM sessions WHERE id_hash = $1")
            .bind(&live)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(moved, Some(keep.id.0));

    // An expired session can never be presented.
    let (untouched,): (Option<Uuid>,) =
        sqlx::query_as("SELECT active_org_id FROM sessions WHERE id_hash = $1")
            .bind(&expired)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(untouched, Some(doomed.id.0));

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn a_tombstoned_slug_stays_held_for_the_restore_window() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "orgs").await;
    let keep = create_org_with_owner(&pool, user, &unique_slug("keep"), "Keep")
        .await
        .unwrap()
        .unwrap();
    let slug = unique_slug("reused");
    let first = create_org_with_owner(&pool, user, &slug, "First")
        .await
        .unwrap()
        .unwrap();

    soft_delete_org_for_user(&pool, first.id, user)
        .await
        .unwrap();

    // Held for the whole window, so the deleter's restore stays possible.
    assert!(
        create_org_with_owner(&pool, user, &slug, "Second")
            .await
            .unwrap()
            .is_none()
    );
    assert!(!slug_is_available(&pool, &slug).await.unwrap());

    // Renaming onto it is refused too — the partial index alone would miss it.
    let other = create_org_with_owner(&pool, user, &unique_slug("other"), "Other")
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        update_org_fields(&pool, other.id, user, None, Some(&slug))
            .await
            .unwrap(),
        UpdateOrgOutcome::SlugTaken
    ));

    assert!(matches!(
        restore_org(
            &pool,
            first.id,
            user,
            30,
            &common::plan_for(&pool, first.id).await
        )
        .await
        .unwrap(),
        RestoreOutcome::Restored(_)
    ));
    assert!(is_active_member(&pool, user, first.id).await.unwrap());
    assert!(is_active_member(&pool, user, keep.id).await.unwrap());

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn restore_readopts_sessions_the_delete_orphaned() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let owner = make_user(&pool, "orgs").await;
    let member = make_user(&pool, "orgs").await;
    // Owner keeps a second org so the delete is allowed at all.
    create_org_with_owner(&pool, owner, &unique_slug("other"), "Other")
        .await
        .unwrap()
        .unwrap();
    let shared = create_org_with_owner(&pool, owner, &unique_slug("shared"), "Shared")
        .await
        .unwrap()
        .unwrap();
    orgs_store::add_member(&pool, shared.id, owner, member, Role::Member, 5)
        .await
        .unwrap();

    let hash = format!("member-{}", Uuid::now_v7());
    sqlx::query(
        "INSERT INTO sessions (id_hash, user_id, active_org_id, expires_at) \
         VALUES ($1, $2, $3, now() + INTERVAL '1 day')",
    )
    .bind(&hash)
    .bind(member.0)
    .bind(shared.id.0)
    .execute(&pool)
    .await
    .unwrap();

    // The member's only org, so the repoint has nowhere to send them.
    soft_delete_org_for_user(&pool, shared.id, owner)
        .await
        .unwrap();
    let (orphaned,): (Option<Uuid>,) =
        sqlx::query_as("SELECT active_org_id FROM sessions WHERE id_hash = $1")
            .bind(&hash)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(orphaned, None);

    // Or the member keeps failing `CurrentOrg` until they sign in again.
    assert!(matches!(
        restore_org(
            &pool,
            shared.id,
            owner,
            30,
            &common::plan_for(&pool, shared.id).await
        )
        .await
        .unwrap(),
        RestoreOutcome::Restored(_)
    ));
    let (readopted,): (Option<Uuid>,) =
        sqlx::query_as("SELECT active_org_id FROM sessions WHERE id_hash = $1")
            .bind(&hash)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(readopted, Some(shared.id.0));

    sqlx::query("DELETE FROM users WHERE id = ANY($1)")
        .bind(vec![owner.0, member.0])
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn rename_of_a_missing_org_is_not_found_even_when_the_slug_is_held() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "orgs").await;
    let keep = create_org_with_owner(&pool, user, &unique_slug("keep"), "Keep")
        .await
        .unwrap()
        .unwrap();
    let held = create_org_with_owner(&pool, user, &unique_slug("held"), "Held")
        .await
        .unwrap()
        .unwrap();
    let gone = create_org_with_owner(&pool, user, &unique_slug("gone"), "Gone")
        .await
        .unwrap()
        .unwrap();
    soft_delete_org_for_user(&pool, gone.id, user)
        .await
        .unwrap();

    // The slug probe must not answer before the org itself is known to exist.
    assert!(matches!(
        update_org_fields(&pool, gone.id, user, None, Some(&held.slug))
            .await
            .unwrap(),
        UpdateOrgOutcome::NotFound
    ));
    // Live org, same held slug: that one really is SlugTaken.
    assert!(matches!(
        update_org_fields(&pool, keep.id, user, None, Some(&held.slug))
            .await
            .unwrap(),
        UpdateOrgOutcome::SlugTaken
    ));

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore]
async fn restore_is_refused_when_the_owner_slot_was_filled() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "orgs").await;
    common::allow_orgs(&pool, user, 3).await;
    let mut owned = Vec::new();
    for _ in 0..3 {
        owned.push(
            create_org_with_owner(&pool, user, &unique_slug("cap"), "Cap")
                .await
                .unwrap()
                .unwrap(),
        );
    }

    // Deleting frees a slot, so the replacement is allowed...
    soft_delete_org_for_user(&pool, owned[0].id, user)
        .await
        .unwrap();
    assert_eq!(common::live_orgs(&pool, user).await, 2);
    let replacement = create_org_with_owner(&pool, user, &unique_slug("repl"), "Replacement")
        .await
        .unwrap()
        .unwrap();

    // ...but the restore must not hand back a fourth.
    assert!(matches!(
        restore_org(
            &pool,
            owned[0].id,
            user,
            30,
            &common::plan_for(&pool, owned[0].id).await
        )
        .await
        .unwrap(),
        RestoreOutcome::OwnerLimit
    ));
    assert_eq!(common::live_orgs(&pool, user).await, 3);

    // Free a slot again and the same restore goes through.
    soft_delete_org_for_user(&pool, replacement.id, user)
        .await
        .unwrap();
    assert!(matches!(
        restore_org(
            &pool,
            owned[0].id,
            user,
            30,
            &common::plan_for(&pool, owned[0].id).await
        )
        .await
        .unwrap(),
        RestoreOutcome::Restored(_)
    ));
    assert_eq!(common::live_orgs(&pool, user).await, 3);

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .unwrap();
}
