//! Live-Postgres contract for `status_pages` + `status_page_components`:
//! per-org isolation on every store method, the per-org page cap,
//! the distinct-target public-component cap,
//! the `add_component` outcome taxonomy (404 vs idempotent vs over-cap),
//! component curation CRUD + reorder, and global slug uniqueness across orgs.
//!
//! The in-memory store (`InMemoryStatusPageStore`) backs the no-DB harnesses;
//! this suite exercises the real SQL, triggers, and CHECK constraints.
//!
//! Live-PG ignored: needs `DATABASE_URL`. Migrations are auto-applied by
//! `pg_pool_from_env` on first connect — point it at a throwaway DB to also
//! validate the migrations themselves.

mod common;

use std::time::Duration;

use uptimepage::api::error::codes;
use uptimepage::domain::{
    CheckSpec, ExpectedStatus, NewMonitorShare, NewStatusPage, NewStatusPageComponent, NewTarget,
    OrgId, StatusPageComponentUpdate, StatusPageId, StatusPageUpdate, UserId, WriteSource,
};
use uptimepage::error::AppError;
use uptimepage::storage::{
    AddComponentOutcome, CreateShareOutcome, MonitorShareStore, PgMonitorShareStore,
    PgStatusPageStore, PostgresTargetStore, StatusPageStore, TargetStore, create_org_with_owner,
};
use uuid::Uuid;

use common::{default_http_check, make_user, pg_pool_from_env, unique_slug};

fn page(slug: &str) -> NewStatusPage {
    NewStatusPage {
        slug: slug.into(),
        name: "P".into(),
        enabled: true,
    }
}

fn component(target_id: Uuid) -> NewStatusPageComponent {
    NewStatusPageComponent {
        target_id,
        public_name: None,
        public_description: None,
        public_group: None,
        sort_order: 0,
        detail_link_enabled: false,
    }
}

async fn component_ids(store: &PgStatusPageStore, org: OrgId, page: StatusPageId) -> Vec<Uuid> {
    store
        .list_components(org, page)
        .await
        .unwrap()
        .iter()
        .map(|c| c.target_id)
        .collect()
}

/// Two owner-orgs (one user each). Returns `(org_a, org_b, user_a, user_b)`.
async fn two_orgs(pool: &sqlx::PgPool, tag: &str) -> (OrgId, OrgId, UserId, UserId) {
    let user_a = make_user(pool, tag).await;
    let user_b = make_user(pool, tag).await;
    let org_a = create_org_with_owner(pool, user_a, &unique_slug(tag), "A")
        .await
        .unwrap()
        .expect("org a")
        .id;
    let org_b = create_org_with_owner(pool, user_b, &unique_slug(tag), "B")
        .await
        .unwrap()
        .expect("org b")
        .id;
    (org_a, org_b, user_a, user_b)
}

async fn make_target(pool: &sqlx::PgPool, org: OrgId, name: &str) -> Uuid {
    let store = PostgresTargetStore::from_pool(pool.clone(), None);
    let url = url::Url::parse("https://example.com/").unwrap();
    let nt = NewTarget {
        name: name.into(),
        check: CheckSpec::Http(default_http_check(url, ExpectedStatus::Exact(200))),
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
    };
    store
        .create(org, nt, WriteSource::Ui, i64::MAX, i64::MAX)
        .await
        .expect("create target")
        .id
}

async fn cleanup(pool: &sqlx::PgPool, orgs: &[OrgId], users: &[UserId]) {
    let _ = sqlx::query("DELETE FROM organizations WHERE id = ANY($1)")
        .bind(orgs.iter().map(|o| o.0).collect::<Vec<_>>())
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM users WHERE id = ANY($1)")
        .bind(users.iter().map(|u| u.0).collect::<Vec<_>>())
        .execute(pool)
        .await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn pages_and_components_isolated_across_orgs_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b, user_a, user_b) = two_orgs(&pool, "sp-iso").await;
    let store = PgStatusPageStore::new(pool.clone());

    let p = store
        .create(
            org_a,
            page(&unique_slug("aiso")),
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .expect("A creates its page")
        .expect("within page cap");
    let target = make_target(&pool, org_a, "A svc").await;
    assert_eq!(
        store
            .add_component(org_a, p.id, component(target), i64::MAX, None)
            .await
            .unwrap(),
        AddComponentOutcome::Added
    );

    // B cannot see A's page on any read path …
    assert!(store.get(org_b, p.id).await.unwrap().is_none());
    assert!(
        !store
            .list(org_b)
            .await
            .unwrap()
            .iter()
            .any(|x| x.id == p.id)
    );
    assert!(store.list_components(org_b, p.id).await.unwrap().is_empty());
    // … nor mutate it: update/delete are no-op misses, component writes too.
    assert!(
        store
            .update(org_b, p.id, StatusPageUpdate::default(), WriteSource::Ui)
            .await
            .unwrap()
            .is_none()
    );
    assert!(!store.delete(org_b, p.id, None).await.unwrap());
    assert!(
        !store
            .update_component(org_b, p.id, target, StatusPageComponentUpdate::default())
            .await
            .unwrap()
    );
    assert!(
        !store
            .remove_component(org_b, p.id, target, None)
            .await
            .unwrap()
    );

    // A still owns page + component after B's failed attempts.
    assert!(store.get(org_a, p.id).await.unwrap().is_some());
    assert_eq!(store.list_components(org_a, p.id).await.unwrap().len(), 1);

    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn page_cap_is_per_org_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b, user_a, user_b) = two_orgs(&pool, "sp-cap").await;
    let store = PgStatusPageStore::new(pool.clone());
    let cap = 1; // the real free-plan max_status_pages

    // Orgs are not auto-seeded a page — they start empty, so the free user gets
    // the full quota for pages they create themselves.
    assert_eq!(
        store.list(org_a).await.unwrap().len(),
        0,
        "no auto-seeded page"
    );

    // The first user page is within the cap of 1.
    store
        .create(
            org_a,
            page(&unique_slug("acap")),
            WriteSource::Ui,
            cap,
            None,
        )
        .await
        .expect("A create call ok")
        .expect("A's first page is allowed");

    // A second page hits the cap.
    let at_cap = store
        .create(
            org_a,
            page(&unique_slug("acap")),
            WriteSource::Ui,
            cap,
            None,
        )
        .await
        .expect("create call ok");
    assert!(at_cap.is_none(), "second page must hit the per-org cap");

    // The cap is per-org: B is unaffected by A being full.
    store
        .create(
            org_b,
            page(&unique_slug("bcap")),
            WriteSource::Ui,
            cap,
            None,
        )
        .await
        .expect("B create ok")
        .expect("B unaffected by A's cap");

    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn public_component_cap_counts_distinct_targets_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, _org_b, user_a, user_b) = two_orgs(&pool, "sp-pcap").await;
    let store = PgStatusPageStore::new(pool.clone());
    let p1 = store
        .create(
            org_a,
            page(&unique_slug("pc1")),
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    let p2 = store
        .create(
            org_a,
            page(&unique_slug("pc2")),
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    let t1 = make_target(&pool, org_a, "t1").await;
    let t2 = make_target(&pool, org_a, "t2").await;
    let cap = 1;

    // t1 on p1 → 1 distinct target, at cap.
    assert_eq!(
        store
            .add_component(org_a, p1.id, component(t1), cap, None)
            .await
            .unwrap(),
        AddComponentOutcome::Added
    );
    // t1 also on p2 → already-counted target costs 0, still within cap.
    assert_eq!(
        store
            .add_component(org_a, p2.id, component(t1), cap, None)
            .await
            .unwrap(),
        AddComponentOutcome::Added
    );
    // t2 is a new distinct target → would exceed the cap.
    assert_eq!(
        store
            .add_component(org_a, p1.id, component(t2), cap, None)
            .await
            .unwrap(),
        AddComponentOutcome::OverCap { used: 1 }
    );

    cleanup(&pool, &[org_a, _org_b], &[user_a, user_b]).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn detail_link_survives_untick_and_share_revoke_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org, _org_b, user, user_b) = two_orgs(&pool, "sp-dl").await;
    let store = PgStatusPageStore::new(pool.clone());
    let shares = PgMonitorShareStore::new(pool.clone(), None);
    let p = store
        .create(
            org,
            page(&unique_slug("dl")),
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    let t = make_target(&pool, org, "dl").await;
    store
        .add_component(org, p.id, component(t), i64::MAX, None)
        .await
        .unwrap();

    let only = |v: Vec<uptimepage::domain::StatusPageComponent>| v.into_iter().next().unwrap();
    let c = only(store.list_components(org, p.id).await.unwrap());
    assert!(!c.detail_link_enabled, "off by default");
    assert!(c.share_id.is_none());

    let CreateShareOutcome::Created(created) = shares
        .create(org, t, NewMonitorShare::default(), Some(user), None, None)
        .await
        .unwrap()
    else {
        panic!("share mint refused");
    };
    store
        .update_component(
            org,
            p.id,
            t,
            StatusPageComponentUpdate {
                detail_link_enabled: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    store
        .attach_share(org, p.id, t, None, created.share.id)
        .await
        .unwrap();
    let c = only(store.list_components(org, p.id).await.unwrap());
    assert!(c.detail_link_enabled);
    assert_eq!(c.share_id, Some(created.share.id));

    store
        .update(
            org,
            p.id,
            StatusPageUpdate {
                name: Some("Renamed".into()),
                ..Default::default()
            },
            WriteSource::Ui,
        )
        .await
        .unwrap();
    let listed = shares.list_for_target(org, t).await.unwrap();
    let backing = listed
        .iter()
        .find(|s| s.id == created.share.id)
        .expect("minted share listed");
    assert!(backing.label.is_none(), "page mints carry no label");
    assert_eq!(
        backing
            .used_by_pages
            .iter()
            .map(|u| u.name.as_str())
            .collect::<Vec<_>>(),
        vec!["Renamed"],
        "page use follows the rename"
    );

    store
        .update_component(
            org,
            p.id,
            t,
            StatusPageComponentUpdate {
                detail_link_enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let c = only(store.list_components(org, p.id).await.unwrap());
    assert!(!c.detail_link_enabled);
    assert_eq!(
        c.share_id,
        Some(created.share.id),
        "untick must not drop the share"
    );

    // Unticking must also drop the page from the share's attribution, or the
    // share modal claims a link the page no longer renders.
    assert!(
        shares
            .list_for_target(org, t)
            .await
            .unwrap()
            .iter()
            .find(|s| s.id == created.share.id)
            .expect("share still listed")
            .used_by_pages
            .is_empty(),
        "an unticked page must not still claim the link"
    );

    // Back on, so the revoke below hits a live link.
    store
        .update_component(
            org,
            p.id,
            t,
            StatusPageComponentUpdate {
                detail_link_enabled: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(only(store.list_components(org, p.id).await.unwrap()).share_live);

    assert!(
        shares
            .revoke(org, t, created.share.id, Some(user))
            .await
            .unwrap()
    );
    let c = only(store.list_components(org, p.id).await.unwrap());
    assert!(
        c.detail_link_enabled && c.share_id.is_some(),
        "revoke leaves the binding alone"
    );
    assert!(
        !c.share_live,
        "a revoked share must read as dead, not as a live link"
    );

    cleanup(&pool, &[org, _org_b], &[user, user_b]).await;
}

/// Two mints racing for one component: the swap is decided by the value each
/// caller read, so the loser is told to clean up rather than silently
/// overwriting the winner and stranding a live URL.
#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn attach_share_swaps_on_the_value_the_caller_read_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org, org_b, user, user_b) = two_orgs(&pool, "sp-cas").await;
    let store = PgStatusPageStore::new(pool.clone());
    let shares = PgMonitorShareStore::new(pool.clone(), None);
    let p = store
        .create(
            org,
            page(&unique_slug("cas")),
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    let t = make_target(&pool, org, "cas").await;
    store
        .add_component(org, p.id, component(t), i64::MAX, None)
        .await
        .unwrap();

    let mint = async || {
        let CreateShareOutcome::Created(c) = shares
            .create(org, t, NewMonitorShare::default(), Some(user), None, None)
            .await
            .unwrap()
        else {
            panic!("mint refused");
        };
        c
    };
    // Both racers read share_id = NULL, then both mint.
    let a = mint().await;
    let b = mint().await;

    assert!(
        store
            .attach_share(org, p.id, t, None, a.share.id)
            .await
            .unwrap(),
        "first swap from NULL wins"
    );
    assert!(
        !store
            .attach_share(org, p.id, t, None, b.share.id)
            .await
            .unwrap(),
        "second swap still expecting NULL must lose"
    );
    assert_eq!(
        store
            .list_components(org, p.id)
            .await
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
            .share_id,
        Some(a.share.id),
        "winner keeps the slot"
    );

    // Re-minting over a known-dead share still works: the caller passes what it
    // read, not NULL.
    let c = mint().await;
    assert!(
        store
            .attach_share(org, p.id, t, Some(a.share.id), c.share.id)
            .await
            .unwrap(),
        "swap from the observed value succeeds"
    );

    cleanup(&pool, &[org, org_b], &[user, user_b]).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn add_component_outcome_taxonomy_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b, user_a, user_b) = two_orgs(&pool, "sp-tax").await;
    let store = PgStatusPageStore::new(pool.clone());
    let p = store
        .create(
            org_a,
            page(&unique_slug("tax")),
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    let t = make_target(&pool, org_a, "t").await;

    assert_eq!(
        store
            .add_component(org_a, p.id, component(t), i64::MAX, None)
            .await
            .unwrap(),
        AddComponentOutcome::Added
    );
    // Re-adding the same target is an idempotent no-op, not a cap error.
    assert_eq!(
        store
            .add_component(org_a, p.id, component(t), i64::MAX, None)
            .await
            .unwrap(),
        AddComponentOutcome::AlreadyOnPage
    );
    // A page that isn't in this org → 404, not a quota error.
    match store
        .add_component(
            org_a,
            StatusPageId(Uuid::new_v4()),
            component(t),
            i64::MAX,
            None,
        )
        .await
    {
        Err(AppError::NotFound { code, .. }) => assert_eq!(code, codes::STATUS_PAGE_NOT_FOUND),
        other => panic!("expected STATUS_PAGE_NOT_FOUND, got {other:?}"),
    }
    // A target that isn't in this org → 404.
    match store
        .add_component(org_a, p.id, component(Uuid::new_v4()), i64::MAX, None)
        .await
    {
        Err(AppError::NotFound { code, .. }) => assert_eq!(code, codes::TARGET_NOT_FOUND),
        other => panic!("expected TARGET_NOT_FOUND, got {other:?}"),
    }
    // Cross-org: B's page can't adopt A's target (target not in B's org).
    let pb = store
        .create(
            org_b,
            page(&unique_slug("taxb")),
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    match store
        .add_component(org_b, pb.id, component(t), i64::MAX, None)
        .await
    {
        Err(AppError::NotFound { code, .. }) => assert_eq!(code, codes::TARGET_NOT_FOUND),
        other => panic!("expected TARGET_NOT_FOUND (cross-org target), got {other:?}"),
    }

    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn component_curation_crud_and_reorder_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, _org_b, user_a, user_b) = two_orgs(&pool, "sp-crud").await;
    let store = PgStatusPageStore::new(pool.clone());
    let p = store
        .create(
            org_a,
            page(&unique_slug("crud")),
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    let t1 = make_target(&pool, org_a, "t1").await;
    let t2 = make_target(&pool, org_a, "t2").await;
    let t3 = make_target(&pool, org_a, "t3").await;
    for t in [t1, t2, t3] {
        store
            .add_component(org_a, p.id, component(t), i64::MAX, None)
            .await
            .unwrap();
    }

    // Set an override, then clear it back via explicit null (double-Option).
    assert!(
        store
            .update_component(
                org_a,
                p.id,
                t1,
                StatusPageComponentUpdate {
                    public_name: Some(Some("Edge".into())),
                    public_group: Some(Some("Core".into())),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
    );
    let after_set = store.list_components(org_a, p.id).await.unwrap();
    let c1 = after_set.iter().find(|c| c.target_id == t1).unwrap();
    assert_eq!(c1.public_name.as_deref(), Some("Edge"));
    assert_eq!(c1.public_group.as_deref(), Some("Core"));

    assert!(
        store
            .update_component(
                org_a,
                p.id,
                t1,
                StatusPageComponentUpdate {
                    public_name: Some(None),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
    );
    let cleared = store.list_components(org_a, p.id).await.unwrap();
    let c1 = cleared.iter().find(|c| c.target_id == t1).unwrap();
    assert_eq!(c1.public_name, None, "explicit null clears the override");
    assert_eq!(
        c1.public_group.as_deref(),
        Some("Core"),
        "omitted field stays unchanged"
    );

    // Clear t1's group so all three are ungrouped — list order is then purely
    // sort_order (otherwise the `NULLS LAST` grouping would float t1 first).
    store
        .update_component(
            org_a,
            p.id,
            t1,
            StatusPageComponentUpdate {
                public_group: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // Reorder, then verify list order follows the new sort_order.
    store
        .reorder_components(org_a, p.id, &[t3, t1, t2])
        .await
        .unwrap();
    let ordered: Vec<Uuid> = store
        .list_components(org_a, p.id)
        .await
        .unwrap()
        .iter()
        .map(|c| c.target_id)
        .collect();
    assert_eq!(ordered, vec![t3, t1, t2]);

    assert!(store.remove_component(org_a, p.id, t2, None).await.unwrap());
    assert_eq!(store.list_components(org_a, p.id).await.unwrap().len(), 2);

    cleanup(&pool, &[org_a, _org_b], &[user_a, user_b]).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn grouped_components_keep_the_order_they_were_dragged_into_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, _org_b, user_a, user_b) = two_orgs(&pool, "sp-order").await;
    let store = PgStatusPageStore::new(pool.clone());
    let p = store
        .create(
            org_a,
            page(&unique_slug("order")),
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    let ns1 = make_target(&pool, org_a, "ns1").await;
    let ns2 = make_target(&pool, org_a, "ns2").await;
    let site = make_target(&pool, org_a, "site").await;
    let clients = make_target(&pool, org_a, "clients").await;
    for (t, group) in [
        (ns1, "DNS"),
        (ns2, "DNS"),
        (site, "NQUARE"),
        (clients, "NQUARE"),
    ] {
        store
            .add_component(
                org_a,
                p.id,
                NewStatusPageComponent {
                    public_group: Some(group.into()),
                    ..component(t)
                },
                i64::MAX,
                None,
            )
            .await
            .unwrap();
    }
    assert_eq!(
        component_ids(&store, org_a, p.id).await,
        vec![ns1, ns2, clients, site],
        "equal sort_order falls back to group name, then monitor name"
    );

    store
        .reorder_components(org_a, p.id, &[site, clients, ns1, ns2])
        .await
        .unwrap();
    assert_eq!(
        component_ids(&store, org_a, p.id).await,
        vec![site, clients, ns1, ns2]
    );

    // An ungrouped component is orderable like any other, not pinned to the end.
    let notes = make_target(&pool, org_a, "notes").await;
    store
        .add_component(org_a, p.id, component(notes), i64::MAX, None)
        .await
        .unwrap();
    store
        .reorder_components(org_a, p.id, &[notes, site, clients, ns1, ns2])
        .await
        .unwrap();
    assert_eq!(
        component_ids(&store, org_a, p.id).await,
        vec![notes, site, clients, ns1, ns2]
    );

    // A row dropped inside another group rejoins its own rather than splitting it.
    store
        .reorder_components(org_a, p.id, &[notes, site, ns1, clients, ns2])
        .await
        .unwrap();
    assert_eq!(
        component_ids(&store, org_a, p.id).await,
        vec![notes, site, clients, ns1, ns2]
    );

    cleanup(&pool, &[org_a, _org_b], &[user_a, user_b]).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn slug_is_globally_unique_across_orgs_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b, user_a, user_b) = two_orgs(&pool, "sp-slug").await;
    let store = PgStatusPageStore::new(pool.clone());
    let shared = unique_slug("shared");

    store
        .create(org_a, page(&shared), WriteSource::Ui, i64::MAX, None)
        .await
        .expect("A claims the slug call ok")
        .expect("A claims the slug");
    // The slug routes a subdomain, so it's unique across the whole table, not
    // per-org: B cannot reuse it.
    match store
        .create(org_b, page(&shared), WriteSource::Ui, i64::MAX, None)
        .await
    {
        Err(AppError::Unprocessable { code, .. }) => assert_eq!(code, codes::SLUG_TAKEN),
        other => panic!("expected SLUG_TAKEN, got {other:?}"),
    }

    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}

// ── pages_for_targets: reverse lookup that drives target-write cache busting ──
#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn pages_for_targets_resolves_curated_pages_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b, user_a, user_b) = two_orgs(&pool, "sp-rev").await;
    let store = PgStatusPageStore::new(pool.clone());

    let p1 = store
        .create(
            org_a,
            page(&unique_slug("rev1")),
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    let p2 = store
        .create(
            org_a,
            page(&unique_slug("rev2")),
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    let curated = make_target(&pool, org_a, "curated").await;
    let lonely = make_target(&pool, org_a, "lonely").await;
    // Same target on two pages → both come back, deduped to two ids.
    store
        .add_component(org_a, p1.id, component(curated), i64::MAX, None)
        .await
        .unwrap();
    store
        .add_component(org_a, p2.id, component(curated), i64::MAX, None)
        .await
        .unwrap();

    let mut pages = store.pages_for_targets(org_a, &[curated]).await.unwrap();
    pages.sort_by_key(|p| p.0);
    let mut want = vec![p1.id, p2.id];
    want.sort_by_key(|p| p.0);
    assert_eq!(pages, want, "a target on two pages resolves to both");

    // A target on no page → empty; an empty id list → empty (no query).
    assert!(
        store
            .pages_for_targets(org_a, &[lonely])
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .pages_for_targets(org_a, &[])
            .await
            .unwrap()
            .is_empty()
    );
    // Cross-org: B asking for A's curated target sees nothing.
    assert!(
        store
            .pages_for_targets(org_b, &[curated])
            .await
            .unwrap()
            .is_empty()
    );

    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}
