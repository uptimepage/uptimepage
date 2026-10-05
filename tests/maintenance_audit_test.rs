use crate::common;

use chrono::{Duration, Utc};
use common::{make_user, pg_pool_from_env};
use sqlx::PgPool;
use uptimepage::domain::{
    MaintenanceFilter, MaintenanceWindowUpdate, NewMaintenanceWindow, OrgId, UserId, WriteSource,
};
use uptimepage::storage::{
    MaintenanceListQuery, MaintenanceStore, PgMaintenanceStore, PgStatusPageStore, StatusPageStore,
};
use uuid::Uuid;

async fn seed_org(pool: &PgPool) -> OrgId {
    let slug = format!("ma{}", Uuid::now_v7().simple());
    let (id,): (Uuid,) = sqlx::query_as(
        "WITH a AS (INSERT INTO accounts (plan_id) VALUES ('free') RETURNING id) \
         INSERT INTO organizations (slug, name, account_id) \
         SELECT $1, 'seeded', a.id FROM a RETURNING id",
    )
    .bind(&slug[..slug.len().min(30)])
    .fetch_one(pool)
    .await
    .expect("seed org");
    OrgId(id)
}

async fn seed_target(pool: &PgPool, org: OrgId) -> Uuid {
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO targets (org_id, name, check_spec, interval_secs) \
         VALUES ($1, 'seeded', '{\"type\":\"http\",\"url\":\"https://example.test\"}'::jsonb, 300) \
         RETURNING id",
    )
    .bind(org.0)
    .fetch_one(pool)
    .await
    .expect("seed target");
    id
}

fn running_window(components: Vec<Uuid>) -> NewMaintenanceWindow {
    NewMaintenanceWindow {
        title: "db upgrade".into(),
        description: None,
        starts_at: Utc::now() - Duration::hours(1),
        ends_at: Utc::now() + Duration::hours(1),
        component_ids: components,
        suppress_alerts: true,
    }
}

fn query(filter: MaintenanceFilter) -> MaintenanceListQuery {
    MaintenanceListQuery {
        filter,
        limit: 10,
        offset: 0,
    }
}

#[tokio::test]
async fn lifecycle_records_who_did_what() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    let author = make_user(&pool, "author").await;
    let editor = make_user(&pool, "editor").await;
    let store = PgMaintenanceStore::new(pool.clone());

    let created = store
        .create(org, running_window(vec![]), WriteSource::Ui, Some(author))
        .await
        .expect("create");
    assert_eq!(created.created_by, Some(author));
    assert_eq!(created.updated_by, Some(author));

    let edited = store
        .update(
            org,
            created.id,
            MaintenanceWindowUpdate {
                title: Some("db upgrade, extended".into()),
                ..Default::default()
            },
            WriteSource::Api,
            Some(editor),
        )
        .await
        .expect("update")
        .expect("window exists");
    assert_eq!(edited.created_by, Some(author));
    assert_eq!(edited.updated_by, Some(editor));

    assert!(
        store
            .delete(org, created.id, WriteSource::Ui, Some(editor))
            .await
            .expect("cancel")
    );
    let cancelled = store
        .get(org, created.id)
        .await
        .expect("get")
        .expect("a cancelled window is still readable");
    assert!(cancelled.deleted_at.is_some());
    assert_eq!(cancelled.deleted_by, Some(editor));
    assert_eq!(cancelled.write_source, WriteSource::Ui);

    assert!(
        !store
            .delete(org, created.id, WriteSource::Ui, Some(editor))
            .await
            .expect("repeat cancel"),
        "cancelling twice reports nothing to cancel"
    );
    assert!(
        store
            .update(
                org,
                created.id,
                MaintenanceWindowUpdate::default(),
                WriteSource::Ui,
                Some(editor),
            )
            .await
            .expect("update after cancel")
            .is_none(),
        "a cancelled window cannot be edited"
    );

    let trail: Vec<(String, Option<Uuid>)> = sqlx::query_as(
        "SELECT action, actor_id FROM org_audit_log \
         WHERE org_id = $1 AND action LIKE 'maintenance.%' ORDER BY occurred_at",
    )
    .bind(org.0)
    .fetch_all(&pool)
    .await
    .expect("audit rows");
    assert_eq!(
        trail,
        vec![
            ("maintenance.created".to_string(), Some(author.0)),
            ("maintenance.updated".to_string(), Some(editor.0)),
            ("maintenance.cancelled".to_string(), Some(editor.0)),
        ]
    );

    let _ = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org.0)
        .execute(&pool)
        .await;
}

#[tokio::test]
async fn a_cancelled_window_leaves_every_live_view() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org).await;
    let store = PgMaintenanceStore::new(pool.clone());

    let window = store
        .create(
            org,
            running_window(vec![target]),
            WriteSource::Ui,
            None::<UserId>,
        )
        .await
        .expect("create");
    assert!(store.alerts_suppressed(org, target).await.expect("check"));
    assert_eq!(
        store
            .list(org, query(MaintenanceFilter::Active))
            .await
            .expect("active")
            .len(),
        1
    );

    store
        .delete(org, window.id, WriteSource::Ui, None)
        .await
        .expect("cancel");

    assert!(
        !store.alerts_suppressed(org, target).await.expect("check"),
        "a cancelled window no longer holds paging"
    );
    assert!(
        store
            .list(org, query(MaintenanceFilter::Active))
            .await
            .expect("active")
            .is_empty()
    );
    let past = store
        .list(org, query(MaintenanceFilter::Past))
        .await
        .expect("past");
    assert_eq!(past.len(), 1, "it is kept as history");
    assert!(past[0].deleted_at.is_some());
    assert_eq!(
        store
            .list(org, query(MaintenanceFilter::All))
            .await
            .expect("all")
            .len(),
        1
    );

    let _ = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org.0)
        .execute(&pool)
        .await;
}

#[tokio::test]
async fn past_is_ordered_by_when_a_window_became_history() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    let store = PgMaintenanceStore::new(pool.clone());
    let window = |from_hours: i64, to_hours: i64| NewMaintenanceWindow {
        title: "w".into(),
        description: None,
        starts_at: Utc::now() + Duration::hours(from_hours),
        ends_at: Utc::now() + Duration::hours(to_hours),
        component_ids: vec![],
        suppress_alerts: true,
    };

    let ran = store
        .create(org, window(-3, -1), WriteSource::Ui, None)
        .await
        .expect("completed window");
    let called_off = store
        .create(org, window(24 * 14, 24 * 14 + 1), WriteSource::Ui, None)
        .await
        .expect("future window");
    store
        .delete(org, called_off.id, WriteSource::Ui, None)
        .await
        .expect("cancel");
    sqlx::query(
        "UPDATE maintenance_windows SET deleted_at = now() - interval '3 days' WHERE id = $1",
    )
    .bind(called_off.id)
    .execute(&pool)
    .await
    .expect("backdate cancellation");

    let past = store
        .list(org, query(MaintenanceFilter::Past))
        .await
        .expect("past");

    let ids: Vec<Uuid> = past.iter().map(|w| w.id).collect();
    assert_eq!(
        ids,
        vec![ran.id, called_off.id],
        "a window cancelled days ago sits below one that just ended"
    );

    let _ = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org.0)
        .execute(&pool)
        .await;
}

async fn seed_page(pool: &PgPool, org: OrgId, enabled: bool, held: bool, targets: &[Uuid]) {
    let slug = format!("pg{}", Uuid::now_v7().simple());
    let (page,): (Uuid,) = sqlx::query_as(
        "INSERT INTO status_pages (org_id, slug, name, enabled, plan_hold_at) \
         VALUES ($1, $2, 'p', $3, CASE WHEN $4 THEN now() END) RETURNING id",
    )
    .bind(org.0)
    .bind(&slug[..slug.len().min(40)])
    .bind(enabled)
    .bind(held)
    .fetch_one(pool)
    .await
    .expect("seed page");
    for target in targets {
        sqlx::query(
            "INSERT INTO status_page_components (org_id, status_page_id, target_id) \
             VALUES ($1, $2, $3)",
        )
        .bind(org.0)
        .bind(page)
        .bind(target)
        .execute(pool)
        .await
        .expect("seed component");
    }
}

#[tokio::test]
async fn published_targets_follow_page_and_monitor_holds() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    let other_org = seed_org(&pool).await;
    let shown = seed_target(&pool, org).await;
    let on_draft_page = seed_target(&pool, org).await;
    let on_held_page = seed_target(&pool, org).await;
    let held_itself = seed_target(&pool, org).await;
    let elsewhere = seed_target(&pool, other_org).await;
    sqlx::query("UPDATE targets SET plan_hold_at = now() WHERE id = $1")
        .bind(held_itself)
        .execute(&pool)
        .await
        .unwrap();
    seed_page(&pool, org, true, false, &[shown, held_itself]).await;
    seed_page(&pool, org, false, false, &[on_draft_page]).await;
    seed_page(&pool, org, true, true, &[on_held_page]).await;
    seed_page(&pool, other_org, true, false, &[elsewhere]).await;

    let published = PgStatusPageStore::new(pool.clone())
        .published_target_ids(org)
        .await
        .unwrap();

    assert_eq!(published.into_iter().collect::<Vec<_>>(), vec![shown]);

    for id in [org, other_org] {
        let _ = sqlx::query("DELETE FROM organizations WHERE id = $1")
            .bind(id.0)
            .execute(&pool)
            .await;
    }
}

#[tokio::test]
async fn listing_loads_each_windows_components() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    let (a, b) = (seed_target(&pool, org).await, seed_target(&pool, org).await);
    let store = PgMaintenanceStore::new(pool.clone());
    let both = store
        .create(org, running_window(vec![a, b]), WriteSource::Ui, None)
        .await
        .unwrap();
    let one = store
        .create(org, running_window(vec![a]), WriteSource::Ui, None)
        .await
        .unwrap();
    let none = store
        .create(org, running_window(vec![]), WriteSource::Ui, None)
        .await
        .unwrap();

    let listed = store
        .list(org, query(MaintenanceFilter::Active))
        .await
        .unwrap();

    let components = |id: Uuid| {
        let mut ids = listed
            .iter()
            .find(|w| w.id == id)
            .expect("window listed")
            .component_ids
            .clone();
        ids.sort();
        ids
    };
    let mut expected = vec![a, b];
    expected.sort();
    assert_eq!(components(both.id), expected);
    assert_eq!(components(one.id), vec![a]);
    assert!(components(none.id).is_empty());

    for id in [org] {
        let _ = sqlx::query("DELETE FROM organizations WHERE id = $1")
            .bind(id.0)
            .execute(&pool)
            .await;
    }
}

#[tokio::test]
async fn closing_a_running_window_is_audited_and_cannot_repeat() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    let actor = make_user(&pool, "ender").await;
    let store = PgMaintenanceStore::new(pool.clone());
    let running = store
        .create(org, running_window(vec![]), WriteSource::Ui, None)
        .await
        .unwrap();
    let close = || MaintenanceWindowUpdate {
        ends_at: Some(Utc::now() - Duration::minutes(1)),
        ..Default::default()
    };

    let closed = store
        .update(org, running.id, close(), WriteSource::Api, Some(actor))
        .await
        .unwrap()
        .expect("a running window can be closed");

    assert!(closed.ends_at < running.ends_at && closed.ends_at > running.starts_at);
    assert_eq!(closed.updated_by, Some(actor));
    assert_eq!(closed.write_source, WriteSource::Api);
    assert!(
        store
            .update(org, running.id, close(), WriteSource::Api, Some(actor))
            .await
            .unwrap()
            .is_none(),
        "a window that already ended is not edited again"
    );
    let trail: Vec<(String, Option<serde_json::Value>)> = sqlx::query_as(
        "SELECT action, metadata->'changed' FROM org_audit_log \
         WHERE org_id = $1 AND action LIKE 'maintenance.%' ORDER BY occurred_at",
    )
    .bind(org.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        trail,
        vec![
            ("maintenance.created".to_string(), None),
            (
                "maintenance.updated".to_string(),
                Some(serde_json::json!(["ends_at"]))
            ),
        ]
    );

    let _ = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org.0)
        .execute(&pool)
        .await;
}

#[tokio::test]
async fn a_finished_window_cannot_be_cancelled_and_upcoming_lists_soonest_first() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    let store = PgMaintenanceStore::new(pool.clone());
    let over = store
        .create(org, running_window(vec![]), WriteSource::Ui, None)
        .await
        .unwrap();
    store
        .update(
            org,
            over.id,
            MaintenanceWindowUpdate {
                ends_at: Some(Utc::now() - Duration::minutes(1)),
                ..Default::default()
            },
            WriteSource::Ui,
            None,
        )
        .await
        .unwrap()
        .expect("ended");
    let later = store
        .create(org, window_from_now(30, 31), WriteSource::Ui, None)
        .await
        .unwrap();
    let sooner = store
        .create(org, window_from_now(2, 3), WriteSource::Ui, None)
        .await
        .unwrap();

    assert!(
        !store
            .delete(org, over.id, WriteSource::Ui, None)
            .await
            .unwrap(),
        "history is not rewritten by a late cancel"
    );
    assert!(
        store
            .get(org, over.id)
            .await
            .unwrap()
            .unwrap()
            .deleted_at
            .is_none()
    );
    let upcoming = store
        .list(org, query(MaintenanceFilter::Upcoming))
        .await
        .unwrap();
    assert_eq!(
        upcoming.iter().map(|w| w.id).collect::<Vec<_>>(),
        vec![sooner.id, later.id]
    );

    let _ = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org.0)
        .execute(&pool)
        .await;
}

fn window_from_now(from_hours: i64, to_hours: i64) -> NewMaintenanceWindow {
    NewMaintenanceWindow {
        title: "w".into(),
        description: None,
        starts_at: Utc::now() + Duration::hours(from_hours),
        ends_at: Utc::now() + Duration::hours(to_hours),
        component_ids: vec![],
        suppress_alerts: true,
    }
}
