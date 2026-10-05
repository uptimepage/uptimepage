//! Live-Postgres contract for `PgPageAssetStore`: the put → get → get_meta →
//! delete roundtrip on the `db` storage path, plus content-hash stability.
//!
//! Live-PG ignored: needs `DATABASE_URL`. Migrations are auto-applied by
//! `pg_pool_from_env` on first connect — point it at a throwaway DB to also
//! validate the migrations themselves.

use crate::common;

use uptimepage::domain::{AssetSlot, NewStatusPage, OrgId, UserId, WriteSource};
use uptimepage::storage::{
    PageAssetStore, PgPageAssetStore, PgStatusPageStore, StatusPageStore, create_org_with_owner,
};

use common::{make_user, pg_pool_from_env, unique_slug};

async fn cleanup(pool: &sqlx::PgPool, org: OrgId, user: UserId) {
    let _ = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org.0)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(pool)
        .await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn page_asset_roundtrip_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "pa").await;
    let org = create_org_with_owner(&pool, user, &unique_slug("pa"), "PA Co")
        .await
        .unwrap()
        .expect("org")
        .id;
    let pages = PgStatusPageStore::new(pool.clone());
    let page = pages
        .create(
            org,
            NewStatusPage {
                slug: unique_slug("pa-page"),
                name: "P".into(),
                enabled: true,
            },
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .unwrap()
        .expect("page within cap");

    let store = PgPageAssetStore::new(pool.clone());

    // Nothing stored yet.
    assert!(store.get(page.id, AssetSlot::Logo).await.unwrap().is_none());
    assert!(
        store
            .get_meta(org, page.id, AssetSlot::Logo)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !store
            .delete(org, page.id, AssetSlot::Logo, None)
            .await
            .unwrap()
    );

    let bytes = b"\x89PNG\r\n\x1a\nfake-logo-bytes";
    let meta = store
        .put(
            org,
            page.id,
            AssetSlot::Logo,
            "image/png",
            bytes,
            serde_json::json!({ "width": 10, "height": 5 }),
            None,
        )
        .await
        .unwrap();
    assert_eq!(meta.byte_size, bytes.len() as i64);
    assert_eq!(meta.content_type, "image/png");

    let got = store
        .get(page.id, AssetSlot::Logo)
        .await
        .unwrap()
        .expect("asset present");
    assert_eq!(got.bytes, bytes);
    assert_eq!(got.content_type, "image/png");
    assert_eq!(got.content_hash, meta.content_hash);

    let got_meta = store
        .get_meta(org, page.id, AssetSlot::Logo)
        .await
        .unwrap()
        .expect("meta present");
    assert_eq!(got_meta.content_hash, meta.content_hash);
    assert_eq!(got_meta.byte_size, bytes.len() as i64);

    // A foreign org reading the same page id sees nothing — org scoping holds.
    assert!(
        store
            .get_meta(OrgId(uuid::Uuid::new_v4()), page.id, AssetSlot::Logo)
            .await
            .unwrap()
            .is_none()
    );

    // Re-put new bytes UPSERTs the same (page, slot) row and rolls the hash.
    let bytes2 = b"\x89PNG\r\n\x1a\ndifferent-bytes";
    let meta2 = store
        .put(
            org,
            page.id,
            AssetSlot::Logo,
            "image/webp",
            bytes2,
            serde_json::json!({}),
            None,
        )
        .await
        .unwrap();
    assert_ne!(meta2.content_hash, meta.content_hash);
    let got2 = store.get(page.id, AssetSlot::Logo).await.unwrap().unwrap();
    assert_eq!(got2.bytes, bytes2);
    assert_eq!(got2.content_type, "image/webp");

    assert!(
        store
            .delete(org, page.id, AssetSlot::Logo, None)
            .await
            .unwrap()
    );
    assert!(store.get(page.id, AssetSlot::Logo).await.unwrap().is_none());

    cleanup(&pool, org, user).await;
}
