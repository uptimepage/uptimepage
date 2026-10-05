//! Notification-channel create/update/delete must write an atomic
//! `org_audit_log` row, via record_audit_tx inside the mutation's tx.

use crate::common;

use uptimepage::domain::{
    ChannelConfig, NewNotificationChannel, NotificationChannelUpdate, SlackConfig, WriteSource,
};
use uptimepage::storage::{
    NotificationChannelStore, PgNotificationChannelStore, create_org_with_owner,
};

use crate::common::MIGRATOR;

fn slack(name: &str) -> NewNotificationChannel {
    NewNotificationChannel {
        name: name.into(),
        config: ChannelConfig::Slack(SlackConfig {
            webhook_url: "https://hooks.slack.com/services/T/B/x".into(),
            mention: None,
        }),
        enabled: true,
        auto_bind_tags: Vec::new(),
        acknowledge_button: true,
        resolve_button: false,
    }
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn channel_crud_writes_audit_rows() {
    let Some((db, name)) = common::fresh_test_db("nc_audit").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let user = common::make_user(&pool, "a").await;
    let org = create_org_with_owner(&pool, user, &common::unique_slug("a"), "Co")
        .await
        .unwrap()
        .unwrap();
    let store = PgNotificationChannelStore::new(pool.clone(), None);

    let ch = store
        .create(org.id, slack("ops"), WriteSource::Ui, 10, Some(user))
        .await
        .unwrap();
    let created: (Option<uuid::Uuid>, serde_json::Value) = sqlx::query_as(
        "SELECT actor_id, metadata FROM org_audit_log \
         WHERE org_id = $1 AND action = 'channel.created'",
    )
    .bind(org.id.0)
    .fetch_one(&pool)
    .await
    .expect("create audit row");
    assert_eq!(created.0, Some(user.0), "actor recorded");
    assert_eq!(
        created.1["channel_id"],
        serde_json::json!(ch.id),
        "channel_id in metadata"
    );
    assert_eq!(
        created.1["kind"],
        serde_json::json!(ch.kind.as_db_str()),
        "kind in metadata"
    );

    store
        .update(
            org.id,
            ch.id,
            NotificationChannelUpdate {
                name: Some("ops-2".into()),
                ..Default::default()
            },
            WriteSource::Ui,
            Some(user),
        )
        .await
        .unwrap()
        .unwrap();
    let updated: (Option<uuid::Uuid>, serde_json::Value) = sqlx::query_as(
        "SELECT actor_id, metadata FROM org_audit_log \
         WHERE org_id = $1 AND action = 'channel.updated'",
    )
    .bind(org.id.0)
    .fetch_one(&pool)
    .await
    .expect("update audit row");
    assert_eq!(updated.0, Some(user.0), "actor recorded");
    assert_eq!(
        updated.1["channel_id"],
        serde_json::json!(ch.id),
        "channel_id in metadata"
    );

    // A no-op delete (unknown id) writes no row.
    assert!(
        !store
            .delete(org.id, uuid::Uuid::new_v4(), Some(user))
            .await
            .unwrap()
    );
    assert!(store.delete(org.id, ch.id, Some(user)).await.unwrap());
    let deleted: Vec<(serde_json::Value,)> = sqlx::query_as(
        "SELECT metadata FROM org_audit_log \
         WHERE org_id = $1 AND action = 'channel.deleted'",
    )
    .bind(org.id.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        deleted.len(),
        1,
        "exactly one delete audit row (no-op wrote none)"
    );
    assert_eq!(
        deleted[0].0["channel_id"],
        serde_json::json!(ch.id),
        "channel_id in metadata"
    );

    common::drop_test_db(&name).await;
}
