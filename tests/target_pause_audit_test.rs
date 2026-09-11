//! Pausing a monitor stops it reporting without leaving a mark on the row, so
//! the `org_audit_log` entry written in the same transaction is the only record
//! of who stopped watching, and of when.

mod common;

use std::time::Duration;

use uptimepage::domain::{CheckSpec, ExpectedStatus, NewTarget, OrgId, TargetUpdate, WriteSource};
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

type AuditRow = (Option<Uuid>, serde_json::Value);

async fn rows(pool: &sqlx::PgPool, org: OrgId, action: &str) -> Vec<AuditRow> {
    sqlx::query_as(
        "SELECT actor_id, metadata FROM org_audit_log \
         WHERE org_id = $1 AND action = $2 ORDER BY occurred_at",
    )
    .bind(org.0)
    .bind(action)
    .fetch_all(pool)
    .await
    .unwrap()
}

fn names(metadata: &serde_json::Value) -> Vec<String> {
    metadata["targets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn bulk_pause_and_resume_write_audit_rows() {
    let Some((db, name)) = common::fresh_test_db("tgt_pause_audit").await else {
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

    let a = seed(&store, org.id, "edge-a").await;
    let b = seed(&store, org.id, "edge-b").await;

    let hit = store
        .set_enabled(org.id, &[a, b, Uuid::new_v4()], false, Some(user))
        .await
        .unwrap();
    assert_eq!(hit.len(), 2, "unknown id is skipped, not an error");

    let paused = rows(&pool, org.id, "target.paused").await;
    assert_eq!(paused.len(), 1, "one row for the whole call");
    assert_eq!(paused[0].0, Some(user.0), "actor recorded");
    assert_eq!(paused[0].1["count"], serde_json::json!(2));
    assert_eq!(paused[0].1["truncated"], serde_json::json!(false));
    let listed = names(&paused[0].1);
    assert!(listed.contains(&"edge-a".to_string()), "{listed:?}");
    assert!(listed.contains(&"edge-b".to_string()), "{listed:?}");

    // Pausing what is already paused changed nothing, so it records nothing:
    // an audit trail of non-events is one nobody can read.
    store
        .set_enabled(org.id, &[a, b], false, Some(user))
        .await
        .unwrap();
    assert_eq!(
        rows(&pool, org.id, "target.paused").await.len(),
        1,
        "a re-pause of paused monitors adds no row"
    );

    // A mixed call names only the ids that actually flipped.
    store
        .set_enabled(org.id, &[a], true, Some(user))
        .await
        .unwrap();
    store
        .set_enabled(org.id, &[a, b], true, Some(user))
        .await
        .unwrap();
    let resumed = rows(&pool, org.id, "target.resumed").await;
    assert_eq!(resumed.len(), 2);
    assert_eq!(resumed[0].1["count"], serde_json::json!(1));
    assert_eq!(names(&resumed[0].1), vec!["edge-a".to_string()]);
    assert_eq!(
        resumed[1].1["count"],
        serde_json::json!(1),
        "already-running edge-a is left out of the second row"
    );
    assert_eq!(names(&resumed[1].1), vec!["edge-b".to_string()]);

    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_single_monitor_pause_is_attributed_too() {
    let Some((db, name)) = common::fresh_test_db("tgt_pause_one_audit").await else {
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
    let id = seed(&store, org.id, "checkout-api").await;

    let pause = TargetUpdate {
        enabled: Some(false),
        ..Default::default()
    };
    store
        .update(org.id, id, pause, Some(WriteSource::Ui), Some(user))
        .await
        .unwrap()
        .unwrap();

    let paused = rows(&pool, org.id, "target.paused").await;
    assert_eq!(paused.len(), 1);
    assert_eq!(paused[0].0, Some(user.0), "actor recorded");
    assert_eq!(paused[0].1["count"], serde_json::json!(1));
    assert_eq!(names(&paused[0].1), vec!["checkout-api".to_string()]);

    // An edit that leaves `enabled` alone is not a pause and writes no row.
    let rename = TargetUpdate {
        name: Some("checkout-api-v2".into()),
        ..Default::default()
    };
    store
        .update(org.id, id, rename, Some(WriteSource::Ui), Some(user))
        .await
        .unwrap()
        .unwrap();

    // Nor does re-sending the state it is already in.
    let again = TargetUpdate {
        enabled: Some(false),
        ..Default::default()
    };
    store
        .update(org.id, id, again, Some(WriteSource::Ui), Some(user))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        rows(&pool, org.id, "target.paused").await.len(),
        1,
        "only the flip is a pause"
    );

    common::drop_test_db(&name).await;
}
