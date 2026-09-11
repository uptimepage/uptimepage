//! Monitor deletes are hard deletes: once the row is gone, the `org_audit_log`
//! entry written in the same transaction is the only surviving record that the
//! monitor existed.

mod common;

use std::time::Duration;

use uptimepage::domain::{CheckSpec, ExpectedStatus, NewTarget, OrgId, WriteSource};
use uptimepage::storage::{PostgresTargetStore, TargetStore, create_org_with_owner};
use url::Url;
use uuid::Uuid;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/postgres");

fn http_target(name: &str) -> NewTarget {
    NewTarget {
        name: name.into(),
        check: CheckSpec::Http(common::default_http_check(
            Url::parse("https://example.test/healthz").unwrap(),
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
        owner_user_id: None,
        regions: None,
    }
}

async fn seed(store: &PostgresTargetStore, org: OrgId, name: &str) -> Uuid {
    store
        .create(org, http_target(name), WriteSource::Ui, i64::MAX, i64::MAX)
        .await
        .unwrap()
        .id
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn target_delete_writes_audit_rows() {
    let Some((db, name)) = common::fresh_test_db("tgt_audit").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let user = common::make_user(&pool, "a").await;
    let org = create_org_with_owner(&pool, user, &common::unique_slug("a"), "Co")
        .await
        .unwrap()
        .unwrap();
    let store = PostgresTargetStore::from_pool(pool.clone(), None);

    let solo = seed(&store, org.id, "checkout-api").await;

    // A no-op delete (unknown id) writes no row.
    assert!(
        !store
            .delete(org.id, Uuid::new_v4(), Some(user))
            .await
            .unwrap()
    );
    assert!(store.delete(org.id, solo, Some(user)).await.unwrap());

    let single: Vec<(Option<Uuid>, serde_json::Value)> = sqlx::query_as(
        "SELECT actor_id, metadata FROM org_audit_log \
         WHERE org_id = $1 AND action = 'target.deleted'",
    )
    .bind(org.id.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(single.len(), 1, "one row, and the no-op wrote none");
    assert_eq!(single[0].0, Some(user.0), "actor recorded");
    assert_eq!(single[0].1["target_id"], serde_json::json!(solo));
    assert_eq!(single[0].1["name"], serde_json::json!("checkout-api"));
    assert_eq!(single[0].1["kind"], serde_json::json!("http"));

    let a = seed(&store, org.id, "edge-a").await;
    let b = seed(&store, org.id, "edge-b").await;
    let gone = store
        .delete_bulk(org.id, &[a, b, Uuid::new_v4()], Some(user))
        .await
        .unwrap();
    assert_eq!(gone.len(), 2, "unknown id is skipped, not an error");

    let bulk: Vec<(Option<Uuid>, serde_json::Value)> = sqlx::query_as(
        "SELECT actor_id, metadata FROM org_audit_log \
         WHERE org_id = $1 AND action = 'target.bulk_deleted'",
    )
    .bind(org.id.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(bulk.len(), 1, "one row for the whole call");
    assert_eq!(bulk[0].0, Some(user.0), "actor recorded");
    assert_eq!(bulk[0].1["count"], serde_json::json!(2));
    assert_eq!(bulk[0].1["truncated"], serde_json::json!(false));
    let names: Vec<String> = bulk[0].1["targets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"edge-a".to_string()));
    assert!(names.contains(&"edge-b".to_string()));

    // An all-miss bulk delete writes nothing.
    assert!(
        store
            .delete_bulk(org.id, &[Uuid::new_v4()], Some(user))
            .await
            .unwrap()
            .is_empty()
    );
    let (bulk_rows,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM org_audit_log \
         WHERE org_id = $1 AND action = 'target.bulk_deleted'",
    )
    .bind(org.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(bulk_rows, 1, "no row for a bulk delete that hit nothing");

    common::drop_test_db(&name).await;
}
