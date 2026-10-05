//! Page-asset put/delete must write an atomic `org_audit_log` row, and delete
//! is org-scoped (a foreign org can't drop another tenant's asset).

use crate::common;

use uptimepage::domain::{AssetSlot, NewStatusPage, StatusPageId, WriteSource};
use uptimepage::storage::{
    PageAssetStore, PgPageAssetStore, PgStatusPageStore, StatusPageStore, create_org_with_owner,
};

use crate::common::MIGRATOR;

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn page_asset_put_delete_write_audit_rows_and_delete_is_org_scoped() {
    let Some((db, name)) = common::fresh_test_db("pa_audit").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let user = common::make_user(&pool, "a").await;
    let org = create_org_with_owner(&pool, user, &common::unique_slug("a"), "Co")
        .await
        .unwrap()
        .unwrap();
    let other_user = common::make_user(&pool, "b").await;
    let other_org = create_org_with_owner(&pool, other_user, &common::unique_slug("b"), "Co2")
        .await
        .unwrap()
        .unwrap();

    let pages = PgStatusPageStore::new(pool.clone());
    let page = pages
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
    let store = PgPageAssetStore::new(pool.clone());

    let bytes = b"\x89PNG\r\n\x1a\nlogo";
    store
        .put(
            org.id,
            StatusPageId(page.id.0),
            AssetSlot::Logo,
            "image/png",
            bytes,
            serde_json::json!({ "width": 4, "height": 2 }),
            Some(user),
        )
        .await
        .unwrap();
    let set: (Option<uuid::Uuid>, serde_json::Value) = sqlx::query_as(
        "SELECT actor_id, metadata FROM org_audit_log \
         WHERE org_id = $1 AND action = 'page_asset.set'",
    )
    .bind(org.id.0)
    .fetch_one(&pool)
    .await
    .expect("set audit row");
    assert_eq!(set.0, Some(user.0), "actor recorded");
    assert_eq!(
        set.1["page_id"],
        serde_json::json!(page.id.0),
        "page_id in metadata"
    );
    assert_eq!(
        set.1["slot"],
        serde_json::json!(AssetSlot::Logo.as_str()),
        "slot in metadata"
    );

    // A foreign org cannot delete this org's asset (store-level org guard) and
    // writes no audit row.
    assert!(
        !store
            .delete(
                other_org.id,
                StatusPageId(page.id.0),
                AssetSlot::Logo,
                Some(other_user)
            )
            .await
            .unwrap()
    );
    assert!(
        store
            .get(StatusPageId(page.id.0), AssetSlot::Logo)
            .await
            .unwrap()
            .is_some(),
        "asset survives a foreign-org delete"
    );

    assert!(
        store
            .delete(org.id, StatusPageId(page.id.0), AssetSlot::Logo, Some(user))
            .await
            .unwrap()
    );
    let deleted: Vec<(serde_json::Value,)> = sqlx::query_as(
        "SELECT metadata FROM org_audit_log \
         WHERE org_id = $1 AND action = 'page_asset.deleted'",
    )
    .bind(org.id.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        deleted.len(),
        1,
        "exactly one delete audit row (foreign-org miss wrote none)"
    );
    assert_eq!(
        deleted[0].0["page_id"],
        serde_json::json!(page.id.0),
        "page_id in metadata"
    );

    common::drop_test_db(&name).await;
}
