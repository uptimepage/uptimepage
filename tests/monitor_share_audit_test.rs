//! Monitor-share mint/revoke must write an atomic `org_audit_log` row
//! (actor + action + metadata), via record_audit_tx inside the mutation's tx.

mod common;

use std::time::Duration;

use uptimepage::domain::{
    CheckSpec, ExpectedStatus, MonitorShareId, NewMonitorShare, NewTarget, WriteSource,
};
use uptimepage::storage::{
    CreateShareOutcome, MonitorShareStore, PgMonitorShareStore, PostgresTargetStore, TargetStore,
    create_org_with_owner,
};

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/postgres");

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn mint_and_revoke_share_write_audit_rows() {
    let Some((db, name)) = common::fresh_test_db("ms_audit").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let user = common::make_user(&pool, "a").await;
    let org = create_org_with_owner(&pool, user, &common::unique_slug("a"), "Co")
        .await
        .unwrap()
        .unwrap();
    let targets = PostgresTargetStore::from_pool(pool.clone(), None);
    let store = PgMonitorShareStore::new(pool.clone(), None);

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

    let created = match store
        .create(
            org.id,
            target.id,
            NewMonitorShare {
                label: None,
                expires_at: None,
            },
            Some(user),
            Some(i64::MAX),
            Some(i64::MAX),
        )
        .await
        .unwrap()
    {
        CreateShareOutcome::Created(c) => c,
        o => panic!("expected Created, got {o:?}"),
    };

    let minted: (Option<uuid::Uuid>, serde_json::Value) = sqlx::query_as(
        "SELECT actor_id, metadata FROM org_audit_log \
         WHERE org_id = $1 AND action = 'monitor_share.created'",
    )
    .bind(org.id.0)
    .fetch_one(&pool)
    .await
    .expect("mint audit row");
    assert_eq!(minted.0, Some(user.0), "actor recorded");
    assert_eq!(
        minted.1["share_id"],
        serde_json::json!(created.share.id.0),
        "share_id in metadata"
    );
    assert_eq!(
        minted.1["target_id"],
        serde_json::json!(target.id),
        "target_id in metadata"
    );

    // A no-op revoke (unknown share id) writes no row.
    assert!(
        !store
            .revoke(
                org.id,
                target.id,
                MonitorShareId(uuid::Uuid::new_v4()),
                Some(user)
            )
            .await
            .unwrap()
    );
    assert!(
        store
            .revoke(org.id, target.id, created.share.id, Some(user))
            .await
            .unwrap()
    );
    let revoked: Vec<(serde_json::Value,)> = sqlx::query_as(
        "SELECT metadata FROM org_audit_log \
         WHERE org_id = $1 AND action = 'monitor_share.revoked'",
    )
    .bind(org.id.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        revoked.len(),
        1,
        "exactly one revoke audit row (no-op wrote none)"
    );
    assert_eq!(
        revoked[0].0["share_id"],
        serde_json::json!(created.share.id.0),
        "share_id in metadata"
    );

    common::drop_test_db(&name).await;
}
