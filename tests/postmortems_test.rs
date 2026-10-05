//! Postgres-backed validation of `PgPostmortemStore`: the upsert org-guard,
//! the action-items JSONB round-trip, publish toggling, and cross-tenant
//! isolation. `#[ignore]`d unless `DATABASE_URL` is set.

use crate::common;

use common::{make_user, unique_slug};
use sqlx::PgPool;
use uptimepage::domain::{ActionItem, OrgId, PostmortemUpsert};
use uptimepage::storage::{PgPostmortemStore, PostmortemStore, create_org_with_owner};
use uuid::Uuid;

/// Seed an org + target + one incident; return (org, owner, incident id).
async fn seed(pool: &PgPool, prefix: &str) -> (OrgId, uptimepage::domain::UserId, Uuid) {
    let user = make_user(pool, prefix).await;
    let org = create_org_with_owner(pool, user, &unique_slug(prefix), "n")
        .await
        .expect("create org")
        .expect("org created");
    let target_id: Uuid = sqlx::query_scalar(
        "INSERT INTO targets (org_id, name, check_spec, interval_secs) \
         VALUES ($1, 'svc', '{}'::jsonb, 30) RETURNING id",
    )
    .bind(org.id.0)
    .fetch_one(pool)
    .await
    .expect("insert target");
    let incident_id: Uuid = sqlx::query_scalar(
        "INSERT INTO incidents (org_id, target_id, started_at, status_at_start, ended_at, state) \
         VALUES ($1, $2, now() - interval '1 hour', 'down', now(), 'resolved') RETURNING id",
    )
    .bind(org.id.0)
    .bind(target_id)
    .fetch_one(pool)
    .await
    .expect("insert incident");
    (org.id, user, incident_id)
}

#[tokio::test]
#[ignore]
async fn upsert_publish_and_isolation_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user, incident_id) = seed(&pool, "pm").await;
    let store = PgPostmortemStore::new(pool.clone());

    // Nothing yet.
    assert!(store.get(org, incident_id).await.unwrap().is_none());

    // Create with action items.
    let created = store
        .upsert(
            org,
            incident_id,
            user,
            PostmortemUpsert {
                summary: Some("cache stampede".into()),
                root_cause: Some("missing jitter".into()),
                impact: Some("8 min of 503s".into()),
                action_items: vec![
                    ActionItem {
                        text: "add jitter".into(),
                        owner_user_id: Some(user),
                        done: false,
                    },
                    ActionItem {
                        text: "alert on hit-rate".into(),
                        owner_user_id: None,
                        done: false,
                    },
                ],
            },
        )
        .await
        .unwrap()
        .expect("incident exists");
    assert_eq!(created.summary.as_deref(), Some("cache stampede"));
    assert_eq!(created.action_items.len(), 2);
    assert_eq!(created.author_id, Some(user));
    assert!(created.published_at.is_none());

    // Update replaces fields + the action-item list; author is preserved.
    let updated = store
        .upsert(
            org,
            incident_id,
            user,
            PostmortemUpsert {
                summary: Some("cache stampede (revised)".into()),
                action_items: vec![ActionItem {
                    text: "add jitter".into(),
                    owner_user_id: None,
                    done: true,
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.summary.as_deref(), Some("cache stampede (revised)"));
    assert_eq!(
        updated.root_cause, None,
        "an omitted field clears on replace"
    );
    assert_eq!(updated.action_items.len(), 1);
    assert!(updated.action_items[0].done);
    assert_eq!(updated.author_id, Some(user), "author kept across edits");

    // Publish then unpublish.
    let pubd = store
        .set_published(org, incident_id, true)
        .await
        .unwrap()
        .unwrap();
    assert!(pubd.published_at.is_some());
    let unpubd = store
        .set_published(org, incident_id, false)
        .await
        .unwrap()
        .unwrap();
    assert!(unpubd.published_at.is_none());

    // Cross-tenant: another org can neither read nor attach a postmortem.
    let other_user = make_user(&pool, "pmx").await;
    let other = create_org_with_owner(&pool, other_user, &unique_slug("pmx"), "n")
        .await
        .unwrap()
        .unwrap();
    assert!(store.get(other.id, incident_id).await.unwrap().is_none());
    assert!(
        store
            .upsert(
                other.id,
                incident_id,
                other_user,
                PostmortemUpsert::default()
            )
            .await
            .unwrap()
            .is_none(),
        "a foreign org cannot attach a postmortem to this incident"
    );
}

#[tokio::test]
#[ignore]
async fn upsert_unknown_incident_is_none_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "pmnone").await;
    let org = create_org_with_owner(&pool, user, &unique_slug("pmnone"), "n")
        .await
        .unwrap()
        .unwrap();
    let store = PgPostmortemStore::new(pool.clone());
    assert!(
        store
            .upsert(org.id, Uuid::now_v7(), user, PostmortemUpsert::default())
            .await
            .unwrap()
            .is_none()
    );
}
