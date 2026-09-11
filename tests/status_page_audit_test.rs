//! Status-page create/delete must write an atomic `org_audit_log` row
//! (actor + action + metadata), via record_audit_tx inside the mutation's tx.

mod common;

use std::time::Duration;

use uptimepage::domain::{
    CheckSpec, ExpectedStatus, NewStatusPage, NewStatusPageComponent, NewTarget, StatusPageId,
    WriteSource,
};
use uptimepage::storage::{
    PgStatusPageStore, PostgresTargetStore, StatusPageStore, TargetStore, create_org_with_owner,
};

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/postgres");

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn create_and_delete_page_write_audit_rows() {
    let Some((db, name)) = common::fresh_test_db("sp_audit").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let user = common::make_user(&pool, "a").await;
    let org = create_org_with_owner(&pool, user, &common::unique_slug("a"), "Co")
        .await
        .unwrap()
        .unwrap();
    let store = PgStatusPageStore::new(pool.clone());

    let page = store
        .create(
            org.id,
            NewStatusPage {
                slug: common::unique_slug("aud"),
                name: "Aud".into(),
                enabled: true,
            },
            WriteSource::Ui,
            10,
            Some(user),
        )
        .await
        .unwrap()
        .unwrap();

    let created: (String, Option<uuid::Uuid>) = sqlx::query_as(
        "SELECT action, actor_id FROM org_audit_log \
         WHERE org_id = $1 AND action = 'status_page.created'",
    )
    .bind(org.id.0)
    .fetch_one(&pool)
    .await
    .expect("create audit row");
    assert_eq!(created.1, Some(user.0), "actor recorded");

    assert!(
        store
            .delete(org.id, StatusPageId(page.id.0), Some(user))
            .await
            .unwrap()
    );
    let (deleted_count,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM org_audit_log \
         WHERE org_id = $1 AND action = 'status_page.deleted'",
    )
    .bind(org.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(deleted_count, 1, "delete audit row written");

    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn add_and_remove_component_write_audit_rows() {
    let Some((db, name)) = common::fresh_test_db("spc_audit").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let user = common::make_user(&pool, "a").await;
    let org = create_org_with_owner(&pool, user, &common::unique_slug("a"), "Co")
        .await
        .unwrap()
        .unwrap();
    let store = PgStatusPageStore::new(pool.clone());
    let targets = PostgresTargetStore::from_pool(pool.clone(), None);

    let page = store
        .create(
            org.id,
            NewStatusPage {
                slug: common::unique_slug("aud"),
                name: "Aud".into(),
                enabled: true,
            },
            WriteSource::Ui,
            10,
            Some(user),
        )
        .await
        .unwrap()
        .unwrap();

    let url = url::Url::parse("https://example.com/").unwrap();
    let target = targets
        .create(
            org.id,
            NewTarget {
                name: "svc".into(),
                check: CheckSpec::Http(common::default_http_check(url, ExpectedStatus::Exact(200))),
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
            },
            WriteSource::Ui,
            i64::MAX,
            i64::MAX,
        )
        .await
        .unwrap();

    store
        .add_component(
            org.id,
            StatusPageId(page.id.0),
            NewStatusPageComponent {
                target_id: target.id,
                public_name: None,
                public_description: None,
                public_group: None,
                sort_order: 0,
                detail_link_enabled: false,
            },
            i64::MAX,
            Some(user),
        )
        .await
        .unwrap();
    let added: (Option<uuid::Uuid>, serde_json::Value) = sqlx::query_as(
        "SELECT actor_id, metadata FROM org_audit_log \
         WHERE org_id = $1 AND action = 'status_page.component_added'",
    )
    .bind(org.id.0)
    .fetch_one(&pool)
    .await
    .expect("add audit row");
    assert_eq!(added.0, Some(user.0), "actor recorded");
    assert_eq!(
        added.1["page_id"],
        serde_json::json!(page.id.0),
        "page_id in metadata"
    );
    assert_eq!(
        added.1["target_id"],
        serde_json::json!(target.id),
        "target_id in metadata"
    );

    // A no-op remove (target not on the page) writes no row.
    assert!(
        !store
            .remove_component(
                org.id,
                StatusPageId(page.id.0),
                uuid::Uuid::new_v4(),
                Some(user)
            )
            .await
            .unwrap()
    );
    assert!(
        store
            .remove_component(org.id, StatusPageId(page.id.0), target.id, Some(user))
            .await
            .unwrap()
    );
    let removed: Vec<(serde_json::Value,)> = sqlx::query_as(
        "SELECT metadata FROM org_audit_log \
         WHERE org_id = $1 AND action = 'status_page.component_removed'",
    )
    .bind(org.id.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        removed.len(),
        1,
        "exactly one remove audit row (no-op wrote none)"
    );
    assert_eq!(
        removed[0].0["target_id"],
        serde_json::json!(target.id),
        "target_id in metadata"
    );

    common::drop_test_db(&name).await;
}
