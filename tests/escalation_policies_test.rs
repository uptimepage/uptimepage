//! Postgres-backed validation of `PgEscalationPolicyStore` and migration 019.
//! The in-memory unit tests cover the routing logic; these exercise the real
//! SQL: nested step+target inserts, the parent-chain org-match triggers, the
//! channel-ownership IDOR guard, soft-delete + binding cleanup, the
//! `resolve_for_target` COALESCE join, and cross-tenant scoping.
//!
//! `#[ignore]`d by default; runs under `--run-ignored all` once `DATABASE_URL`
//! is set. The harness auto-applies all migrations on first connect, so a clean
//! run is also the fresh-DB validation of `019_escalation_policies`.

use crate::common;

use common::{make_user, unique_slug};
use sqlx::PgPool;
use uptimepage::domain::{
    EscalationTargetType, NewEscalationPolicy, NewEscalationStep, NewEscalationTarget, OrgId,
};
use uptimepage::storage::{EscalationPolicyStore, PgEscalationPolicyStore, create_org_with_owner};
use uuid::Uuid;

async fn seed_org(pool: &PgPool, prefix: &str) -> OrgId {
    let user = make_user(pool, prefix).await;
    create_org_with_owner(pool, user, &unique_slug(prefix), "n")
        .await
        .expect("create org")
        .expect("org created")
        .id
}

async fn seed_target(pool: &PgPool, org: OrgId) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO targets (org_id, name, check_spec, interval_secs) \
         VALUES ($1, 'svc', '{}'::jsonb, 30) RETURNING id",
    )
    .bind(org.0)
    .fetch_one(pool)
    .await
    .expect("insert target")
}

async fn seed_channel(pool: &PgPool, org: OrgId, name: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO notification_channels (org_id, name, kind, config, enabled) \
         VALUES ($1, $2, 'webhook', '{\"webhook\":{\"url\":\"http://x/\",\"headers\":{}}}'::jsonb, true) \
         RETURNING id",
    )
    .bind(org.0)
    .bind(name)
    .fetch_one(pool)
    .await
    .expect("insert channel")
}

fn channel_step(level: i32, delay: i32, channel: Uuid) -> NewEscalationStep {
    NewEscalationStep {
        level,
        delay_secs: delay,
        targets: vec![NewEscalationTarget {
            target_type: EscalationTargetType::Channel,
            user_id: None,
            schedule_id: None,
            channel_id: Some(channel),
        }],
    }
}

fn policy(name: &str, steps: Vec<NewEscalationStep>) -> NewEscalationPolicy {
    NewEscalationPolicy {
        name: name.into(),
        description: Some("primary ladder".into()),
        repeat_count: 1,
        steps,
    }
}

#[tokio::test]
#[ignore]
async fn create_get_replace_delete_roundtrip_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let org = seed_org(&pool, "escpol").await;
    let c1 = seed_channel(&pool, org, "ops").await;
    let c2 = seed_channel(&pool, org, "esc").await;
    let store = PgEscalationPolicyStore::new(pool.clone());

    let created = store
        .create(
            org,
            policy(
                "primary",
                vec![channel_step(1, 300, c1), channel_step(2, 600, c2)],
            ),
            10,
        )
        .await
        .expect("create");
    assert_eq!(created.steps.len(), 2);
    assert_eq!(created.repeat_count, 1);
    assert_eq!(created.steps[0].level, 1);
    assert_eq!(created.steps[0].targets[0].channel_id, Some(c1));

    let fetched = store.get(org, created.id).await.unwrap().unwrap();
    assert_eq!(fetched.steps.len(), 2);
    assert_eq!(store.list(org).await.unwrap().len(), 1);

    // Replace the whole ladder with a single rung.
    let replaced = store
        .replace(
            org,
            created.id,
            policy("primary", vec![channel_step(1, 60, c1)]),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(replaced.steps.len(), 1);

    assert!(store.delete(org, created.id).await.unwrap());
    assert!(store.get(org, created.id).await.unwrap().is_none());
    assert!(store.list(org).await.unwrap().is_empty());
}

#[tokio::test]
#[ignore]
async fn rejects_cross_org_channel_target_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let org = seed_org(&pool, "escidor").await;
    let other = seed_org(&pool, "escidoroth").await;
    let foreign_channel = seed_channel(&pool, other, "theirs").await;
    let store = PgEscalationPolicyStore::new(pool.clone());

    // A policy in `org` cannot reference another tenant's channel.
    let err = store
        .create(
            org,
            policy("x", vec![channel_step(1, 60, foreign_channel)]),
            10,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        uptimepage::error::AppError::Unprocessable { .. }
    ));
    // The failed insert rolled back — no orphan policy left behind.
    assert!(store.list(org).await.unwrap().is_empty());
}

#[tokio::test]
#[ignore]
async fn resolve_for_target_binding_default_and_delete_fallback_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let org = seed_org(&pool, "escresolve").await;
    let target = seed_target(&pool, org).await;
    let store = PgEscalationPolicyStore::new(pool.clone());

    let def = store
        .create(org, policy("default", vec![]), 10)
        .await
        .unwrap();
    let bound = store
        .create(org, policy("bound", vec![]), 10)
        .await
        .unwrap();

    assert_eq!(store.resolve_for_target(org, target).await.unwrap(), None);

    store.set_org_default(org, Some(def.id)).await.unwrap();
    assert_eq!(store.org_default(org).await.unwrap(), Some(def.id));
    assert_eq!(
        store.resolve_for_target(org, target).await.unwrap(),
        Some(def.id)
    );
    // Effective policy is the default, but the monitor has no own binding yet.
    assert_eq!(store.target_policy(org, target).await.unwrap(), None);

    assert!(
        store
            .set_target_policy(org, target, Some(bound.id))
            .await
            .unwrap()
    );
    assert_eq!(
        store.resolve_for_target(org, target).await.unwrap(),
        Some(bound.id)
    );
    assert_eq!(
        store.target_policy(org, target).await.unwrap(),
        Some(bound.id)
    );

    // Deleting the bound policy clears the dangling binding → falls back to the
    // org default (the soft-delete reference cleanup).
    assert!(store.delete(org, bound.id).await.unwrap());
    assert_eq!(
        store.resolve_for_target(org, target).await.unwrap(),
        Some(def.id)
    );

    // Deleting the default too clears it → no policy.
    assert!(store.delete(org, def.id).await.unwrap());
    assert_eq!(store.org_default(org).await.unwrap(), None);
    assert_eq!(store.resolve_for_target(org, target).await.unwrap(), None);
}

#[tokio::test]
#[ignore]
async fn create_enforces_quota_and_unique_name_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let org = seed_org(&pool, "escquota").await;
    let store = PgEscalationPolicyStore::new(pool.clone());

    store.create(org, policy("a", vec![]), 1).await.unwrap();
    let over = store.create(org, policy("b", vec![]), 1).await.unwrap_err();
    assert!(matches!(
        over,
        uptimepage::error::AppError::Unprocessable { .. }
    ));
    let dup = store
        .create(org, policy("a", vec![]), 10)
        .await
        .unwrap_err();
    assert!(matches!(
        dup,
        uptimepage::error::AppError::Unprocessable { .. }
    ));
    // A deleted name frees up for reuse (partial unique index on deleted_at).
    let live = store.list(org).await.unwrap();
    assert_eq!(live.len(), 1);
    store.delete(org, live[0].id).await.unwrap();
    store.create(org, policy("a", vec![]), 10).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn policies_are_isolated_per_org_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let a = seed_org(&pool, "escisoa").await;
    let b = seed_org(&pool, "escisob").await;
    let store = PgEscalationPolicyStore::new(pool.clone());

    let pa = store.create(a, policy("shared", vec![]), 10).await.unwrap();
    // Same name in another org is allowed and invisible across the tenant line.
    store.create(b, policy("shared", vec![]), 10).await.unwrap();
    assert!(store.get(b, pa.id).await.unwrap().is_none());
    assert!(!store.delete(b, pa.id).await.unwrap());
    assert!(
        store
            .replace(b, pa.id, policy("shared", vec![]))
            .await
            .unwrap()
            .is_none()
    );
    assert!(store.get(a, pa.id).await.unwrap().is_some());
}
