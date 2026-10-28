//! Past maintenance on a status page: which windows the page's history and
//! archive list, and the merged incident + maintenance paging.
//!
//! Needs Postgres and ClickHouse (the page snapshot reads both):
//!
//!     DATABASE_URL=postgres://monitor:monitor@127.0.0.1:5432/<db> \
//!     CLICKHOUSE_URL=http://127.0.0.1:8123 \
//!       cargo nextest run --test it --run-ignored all -E 'test(/^maintenance_history_test::/)'

use crate::common;

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use sqlx::PgPool;
use uptimepage::domain::{
    CheckSpec, DayState, ExpectedStatus, MaintenanceUpdatePhase, NewStatusPage,
    NewStatusPageComponent, NewTarget, OrgId, PageRef, WriteSource,
};
use uptimepage::pagination::HistoryCursor;
use uptimepage::public_status::history::{HistoryEntry, merge};
use uptimepage::public_status::{
    AggregatorConfig, IncidentListQuery, MaintenanceListQuery, OrgAggregator, OrgPublicSource,
    PageCache, PublicSource,
};
use uptimepage::storage::{
    IncidentOpsStore, MaintenanceStore, PgMaintenanceStore, PgStatusPageStore, PostedUpdate,
    PostgresTargetStore, StatusPageStore, TargetStore,
};
use url::Url;
use uuid::Uuid;

use crate::common::default_http_check;

async fn seed_org(pool: &PgPool) -> OrgId {
    let slug = format!("mh-{}", Uuid::now_v7().simple());
    let (id,): (Uuid,) = sqlx::query_as(
        "WITH a AS (INSERT INTO accounts DEFAULT VALUES RETURNING id) \
         INSERT INTO organizations (slug, name, account_id) \
         SELECT $1, 'Maintenance history', a.id FROM a RETURNING id",
    )
    .bind(&slug[..30])
    .fetch_one(pool)
    .await
    .expect("insert org");
    OrgId(id)
}

async fn seed_target(pool: &PgPool, org: OrgId, name: &str) -> Uuid {
    let store = PostgresTargetStore::from_pool(pool.clone(), None);
    let url = Url::parse("https://example.com/").unwrap();
    let target = NewTarget {
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
        recovery_period_secs: None,
        group_name: None,
        group_id: None,
        owner_user_id: None,
        regions: None,
    };
    store
        .create(
            org,
            target,
            WriteSource::Ui,
            None,
            uptimepage::domain::WriteCaps::UNLIMITED,
        )
        .await
        .expect("create target")
        .id
}

async fn seed_page(pool: &PgPool, org: OrgId, target: Uuid) -> uptimepage::domain::StatusPageId {
    let store = PgStatusPageStore::new(pool.clone());
    let slug = format!("mhpage{}", Uuid::now_v7().simple());
    let page = store
        .create(
            org,
            NewStatusPage {
                access: None,
                slug: slug[..30].to_owned(),
                name: "History".into(),
                enabled: true,
            },
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .expect("create page")
        .expect("within page cap");
    store
        .add_component(
            org,
            page.id,
            NewStatusPageComponent {
                target_id: target,
                public_name: Some("Public API".into()),
                public_description: None,
                public_group: None,
                sort_order: 0,
                detail_link_enabled: false,
            },
            i64::MAX,
            None,
        )
        .await
        .expect("add component");
    page.id
}

/// A window from `start_h` to `end_h` hours from now on `targets`.
async fn seed_window(
    pool: &PgPool,
    org: OrgId,
    title: &str,
    start_h: i32,
    end_h: i32,
    targets: &[Uuid],
) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO maintenance_windows (org_id, title, description, starts_at, ends_at)
         VALUES ($1, $2, 'planned', now() + make_interval(hours => $3),
                 now() + make_interval(hours => $4))
         RETURNING id",
    )
    .bind(org.0)
    .bind(title)
    .bind(start_h)
    .bind(end_h)
    .fetch_one(pool)
    .await
    .expect("insert window");
    for target in targets {
        sqlx::query(
            "INSERT INTO maintenance_window_components (org_id, maintenance_id, target_id)
             VALUES ($1, $2, $3)",
        )
        .bind(org.0)
        .bind(id)
        .bind(target)
        .execute(pool)
        .await
        .expect("insert window component");
    }
    id
}

async fn seed_incident(pool: &PgPool, org: OrgId, target: Uuid, start_h: i32) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO incidents (org_id, target_id, started_at, ended_at, status_at_start, visibility)
         VALUES ($1, $2, now() + make_interval(hours => $3),
                 now() + make_interval(hours => $3, mins => 5), 'down', 'public')
         RETURNING id",
    )
    .bind(org.0)
    .bind(target)
    .bind(start_h)
    .fetch_one(pool)
    .await
    .expect("insert incident")
}

async fn with_org_cleanup<F>(pool: &PgPool, org: OrgId, f: F)
where
    F: std::future::Future<Output = ()>,
{
    let result = AssertUnwindSafe(f).catch_unwind().await;
    let _ = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org.0)
        .execute(pool)
        .await;
    if let Err(p) = result {
        std::panic::resume_unwind(p);
    }
}

fn aggregator(pool: &PgPool, ch: clickhouse::Client) -> Arc<OrgAggregator> {
    Arc::new(OrgAggregator::new(
        pool.clone(),
        ch,
        AggregatorConfig::default(),
        None,
    ))
}

fn public_source(pool: &PgPool, agg: Arc<OrgAggregator>) -> OrgPublicSource {
    let cfg = uptimepage::config::PublicStatusConfig::default();
    OrgPublicSource::new(agg, PageCache::new(&cfg), pool.clone())
}

fn titles(windows: &[uptimepage::domain::PublicMaintenance]) -> Vec<&str> {
    windows.iter().map(|w| w.title.as_str()).collect()
}

#[tokio::test]
#[ignore = "requires DATABASE_URL + CLICKHOUSE_URL"]
async fn a_page_lists_the_windows_that_ran_on_its_components() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let Some(ch) = common::ch_client_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    with_org_cleanup(&pool, org, async {
        let on_page = seed_target(&pool, org, "mh-on-page").await;
        let elsewhere = seed_target(&pool, org, "mh-elsewhere").await;
        let page = seed_page(&pool, org, on_page).await;

        seed_window(&pool, org, "both", -48, -47, &[on_page, elsewhere]).await;
        seed_window(&pool, org, "ended", -72, -71, &[on_page]).await;
        seed_window(&pool, org, "old", -40 * 24, -40 * 24 + 1, &[on_page]).await;
        seed_window(&pool, org, "running", -1, 1, &[on_page]).await;
        seed_window(&pool, org, "upcoming", 24, 25, &[on_page]).await;
        seed_window(&pool, org, "other page", -24, -23, &[elsewhere]).await;
        let cancelled = seed_window(&pool, org, "cancelled", -30, -29, &[on_page]).await;
        sqlx::query("UPDATE maintenance_windows SET deleted_at = now() WHERE id = $1")
            .bind(cancelled)
            .execute(&pool)
            .await
            .unwrap();

        let agg = aggregator(&pool, ch);
        let (snapshot, _, _, _) = agg.build(page, org).await.expect("build");
        assert_eq!(
            titles(&snapshot.recent_maintenance),
            ["both", "ended"],
            "ended in the last 30 days, newest first"
        );
        assert!(!snapshot.recent_maintenance_has_more);
        assert_eq!(
            snapshot.recent_maintenance[0].affected_component_names,
            ["Public API"],
            "only this page's components, by their public name"
        );

        let source = public_source(&pool, agg);
        let page_ref = PageRef { page, org };
        let list = |ended_only, limit, cursor| MaintenanceListQuery {
            limit,
            cursor,
            ended_only,
        };

        let past = source
            .list_maintenance(page_ref, list(true, 100, None))
            .await
            .unwrap();
        assert_eq!(titles(&past.items), ["both", "ended", "old"]);
        assert!(past.next_cursor.is_none());

        let started = source
            .list_maintenance(page_ref, list(false, 100, None))
            .await
            .unwrap();
        assert_eq!(titles(&started.items), ["running", "both", "ended", "old"]);

        let mut walked = Vec::new();
        let mut cursor = None;
        loop {
            let one = source
                .list_maintenance(page_ref, list(false, 1, cursor))
                .await
                .unwrap();
            walked.extend(one.items.into_iter().map(|w| w.title));
            match one.next_cursor {
                Some(next) => cursor = Some(HistoryCursor::decode(&next).unwrap()),
                None => break,
            }
        }
        assert_eq!(walked, ["running", "both", "ended", "old"]);
    })
    .await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL + CLICKHOUSE_URL"]
async fn the_merged_history_pages_through_both_kinds_without_a_gap() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let Some(ch) = common::ch_client_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    with_org_cleanup(&pool, org, async {
        let target = seed_target(&pool, org, "mh-merge").await;
        let page = seed_page(&pool, org, target).await;
        let mut expected: Vec<(i32, Uuid)> = Vec::new();
        for h in [-2, -5, -9, -13] {
            expected.push((h, seed_incident(&pool, org, target, h).await));
        }
        for h in [-3, -4, -10, -20] {
            let id = seed_window(&pool, org, &format!("w{h}"), h, h + 1, &[target]).await;
            expected.push((h, id));
        }
        expected.sort_by_key(|(h, _)| std::cmp::Reverse(*h));
        let expected: Vec<Uuid> = expected.into_iter().map(|(_, id)| id).collect();

        let agg = aggregator(&pool, ch);
        let source = public_source(&pool, agg);
        let page_ref = PageRef { page, org };

        let mut walked: Vec<Uuid> = Vec::new();
        let mut cursor: Option<HistoryCursor> = None;
        let mut pages = 0;
        loop {
            let incidents = source
                .list_incidents(
                    page_ref,
                    IncidentListQuery {
                        limit: 3,
                        cursor,
                        ongoing_only: false,
                    },
                )
                .await
                .unwrap();
            let windows = source
                .list_maintenance(
                    page_ref,
                    MaintenanceListQuery {
                        limit: 3,
                        cursor,
                        ended_only: false,
                    },
                )
                .await
                .unwrap();
            let merged = merge(incidents, windows, 3);
            pages += 1;
            walked.extend(merged.entries.iter().map(|e| match e {
                HistoryEntry::Incident(i) => i.id,
                HistoryEntry::Maintenance(m) => m.id,
            }));
            match merged.next_cursor {
                Some(next) => cursor = Some(HistoryCursor::decode(&next).unwrap()),
                None => break,
            }
        }
        assert_eq!(walked, expected, "every row once, newest first");
        assert_eq!(pages, 3);
    })
    .await;
}

async fn add_to_page(
    pool: &PgPool,
    org: OrgId,
    page: uptimepage::domain::StatusPageId,
    target: Uuid,
    name: &str,
) {
    PgStatusPageStore::new(pool.clone())
        .add_component(
            org,
            page,
            NewStatusPageComponent {
                target_id: target,
                public_name: Some(name.into()),
                public_description: None,
                public_group: None,
                sort_order: 1,
                detail_link_enabled: false,
            },
            i64::MAX,
            None,
        )
        .await
        .expect("add component");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL + CLICKHOUSE_URL"]
async fn a_page_keeps_what_it_showed_after_the_monitor_leaves_it() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let Some(ch) = common::ch_client_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    with_org_cleanup(&pool, org, async {
        let removed = seed_target(&pool, org, "mh-removed").await;
        let deleted = seed_target(&pool, org, "mh-deleted").await;
        let page = seed_page(&pool, org, removed).await;
        add_to_page(&pool, org, page, deleted, "Webhooks").await;

        seed_window(&pool, org, "ran on the removed one", -48, -47, &[removed]).await;
        seed_window(&pool, org, "ran on the deleted one", -30, -29, &[deleted]).await;
        seed_window(&pool, org, "not started", 24, 25, &[removed]).await;
        let incident = seed_incident(&pool, org, removed, -10).await;
        let open: Uuid = sqlx::query_scalar(
            "INSERT INTO incidents (org_id, target_id, started_at, status_at_start, visibility)
             VALUES ($1, $2, now() - interval '5 minutes', 'down', 'public') RETURNING id",
        )
        .bind(org.0)
        .bind(removed)
        .fetch_one(&pool)
        .await
        .expect("insert open incident");

        assert!(
            PgStatusPageStore::new(pool.clone())
                .remove_component(org, page, removed, None)
                .await
                .unwrap()
        );
        assert!(
            PostgresTargetStore::from_pool(pool.clone(), None)
                .delete(org, deleted, None)
                .await
                .unwrap()
        );

        let agg = aggregator(&pool, ch);
        let (snapshot, _, _, _) = agg.build(page, org).await.expect("build");
        assert_eq!(
            titles(&snapshot.recent_maintenance),
            ["ran on the deleted one", "ran on the removed one"]
        );
        assert_eq!(
            snapshot.recent_maintenance[0].affected_component_names,
            ["Webhooks"],
            "under the name the page gave it"
        );
        assert_eq!(
            snapshot.recent_maintenance[1].affected_component_names,
            ["Public API"]
        );
        assert!(
            snapshot.upcoming_maintenance.is_empty(),
            "a window that never ran on the page follows the live page"
        );
        let kept = snapshot
            .recent_incidents
            .iter()
            .find(|i| i.id == incident)
            .expect("the incident stays in the history");
        assert_eq!(kept.component_name, "Public API");
        assert_eq!(
            kept.component_id, None,
            "a monitor no longer on the page is not one of its components"
        );
        assert!(
            uptimepage::storage::PgIncidentOpsStore::new(pool.clone())
                .status_pages(org, incident)
                .await
                .unwrap()
                .is_empty(),
            "a monitor's incident is still not published to pages of its own"
        );
        assert!(
            snapshot.active_incidents.iter().all(|i| i.id != open),
            "the live page follows its live components"
        );
        sqlx::query("UPDATE incidents SET ended_at = now(), state = 'resolved' WHERE id = $1")
            .bind(open)
            .execute(&pool)
            .await
            .unwrap();
        let (after, _, _, _) = agg.build(page, org).await.expect("build");
        assert!(
            after.recent_incidents.iter().any(|i| i.id == open),
            "once it ended, the history keeps it"
        );

        let source = public_source(&pool, agg);
        let past = source
            .list_maintenance(
                PageRef { page, org },
                MaintenanceListQuery {
                    limit: 100,
                    cursor: None,
                    ended_only: true,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            titles(&past.items),
            ["ran on the deleted one", "ran on the removed one"]
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL + CLICKHOUSE_URL"]
async fn a_window_day_reads_as_maintenance_and_the_page_carries_its_updates() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let Some(ch) = common::ch_client_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    with_org_cleanup(&pool, org, async {
        let target = seed_target(&pool, org, "mh-planned").await;
        let page = seed_page(&pool, org, target).await;
        let window = seed_window(&pool, org, "planned", -27, -24, &[target]).await;
        seed_incident(&pool, org, target, -26).await;

        let store = PgMaintenanceStore::new(pool.clone());
        let posted = store
            .post_update(org, window, "Finished early.", 50, None)
            .await
            .unwrap();
        let PostedUpdate::Posted(update) = posted else {
            panic!("posted: {posted:?}");
        };
        assert_eq!(update.phase, MaintenanceUpdatePhase::Completed);
        assert!(matches!(
            store
                .post_update(org, window, "full", 1, None)
                .await
                .unwrap(),
            PostedUpdate::Full
        ));
        assert!(matches!(
            store
                .post_update(org, Uuid::now_v7(), "nobody", 50, None)
                .await
                .unwrap(),
            PostedUpdate::NotFound
        ));
        let audited: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM org_audit_log
             WHERE org_id = $1 AND action = 'maintenance.update_posted'",
        )
        .bind(org.0)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(audited, 1);

        let agg = aggregator(&pool, ch);
        let (snapshot, markers, _, _) = agg.build(page, org).await.expect("build");
        let history = &snapshot.groups[0].components[0].history;
        assert!(history.contains(&DayState::Maintenance));
        assert!(
            !history.iter().any(|d| matches!(
                d,
                DayState::MajorOutage | DayState::PartialOutage | DayState::Degraded
            )),
            "the outage inside the window is planned work: {history:?}"
        );
        assert!(markers.iter().any(|m| m.id == window));
        assert_eq!(snapshot.recent_maintenance[0].updates.len(), 1);
        assert_eq!(
            snapshot.recent_maintenance[0].updates[0].message,
            "Finished early."
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL + CLICKHOUSE_URL"]
async fn a_running_window_that_drops_a_monitor_stays_in_that_page_history() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let Some(ch) = common::ch_client_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    with_org_cleanup(&pool, org, async {
        let on_page = seed_target(&pool, org, "mh-dropped").await;
        let elsewhere = seed_target(&pool, org, "mh-still-covered").await;
        let page = seed_page(&pool, org, on_page).await;
        let window = seed_window(&pool, org, "narrowed", -2, 1, &[on_page, elsewhere]).await;
        seed_incident(&pool, org, on_page, -2).await;
        sqlx::query(
            "INSERT INTO incidents (org_id, target_id, started_at, ended_at, status_at_start, visibility)
             VALUES ($1, $2, now() - interval '20 minutes', now() - interval '15 minutes',
                     'down', 'public')",
        )
        .bind(org.0)
        .bind(on_page)
        .execute(&pool)
        .await
        .expect("insert incident after the monitor left the window");

        PgMaintenanceStore::new(pool.clone())
            .update(
                org,
                window,
                uptimepage::domain::MaintenanceWindowUpdate {
                    component_ids: Some(vec![elsewhere]),
                    ..Default::default()
                },
                WriteSource::Api,
                None,
            )
            .await
            .unwrap()
            .expect("a running window can be edited");

        let agg = aggregator(&pool, ch);
        let (running, _, _, _) = agg.build(page, org).await.expect("build");
        assert!(
            running.active_maintenance.is_empty(),
            "no longer on this page now"
        );

        // The page lost the monitor half an hour ago.
        sqlx::query(
            "UPDATE maintenance_window_departures SET left_at = now() - interval '30 minutes'
             WHERE maintenance_id = $1",
        )
        .bind(window)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE maintenance_windows SET ends_at = now() - interval '1 minute' WHERE id = $1",
        )
        .bind(window)
        .execute(&pool)
        .await
        .unwrap();
        let (ended, _, _, _) = agg.build(page, org).await.expect("build");
        assert_eq!(titles(&ended.recent_maintenance), ["narrowed"]);
        assert_eq!(
            ended.recent_maintenance[0].affected_component_names,
            ["Public API"]
        );
        let down: i64 = ended.groups[0].components[0]
            .downtime
            .iter()
            .map(|d| d.major_secs)
            .sum();
        assert_eq!(
            down,
            5 * 60,
            "planned while the window held the monitor, an outage once it let go"
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL + CLICKHOUSE_URL"]
async fn a_monitor_counts_as_under_maintenance_only_while_it_was_in_the_window() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let Some(ch) = common::ch_client_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    with_org_cleanup(&pool, org, async {
        let on_page = seed_target(&pool, org, "mh-rejoined").await;
        let elsewhere = seed_target(&pool, org, "mh-stays").await;
        let page = seed_page(&pool, org, on_page).await;
        let window = seed_window(&pool, org, "rejoined", -2, 1, &[on_page, elsewhere]).await;
        let store = PgMaintenanceStore::new(pool.clone());
        let cover = |ids: Vec<Uuid>| uptimepage::domain::MaintenanceWindowUpdate {
            component_ids: Some(ids),
            ..Default::default()
        };
        let outage = |from: i32, until: i32| {
            sqlx::query(
                "INSERT INTO incidents (org_id, target_id, started_at, ended_at, status_at_start, visibility)
                 VALUES ($1, $2, now() - make_interval(mins => $3), now() - make_interval(mins => $4),
                         'down', 'public')",
            )
            .bind(org.0)
            .bind(on_page)
            .bind(from)
            .bind(until)
            .execute(&pool)
        };
        let agg = aggregator(&pool, ch);
        let downtime = || async {
            let (page_now, _, _, _) = agg.build(page, org).await.expect("build");
            page_now.groups[0].components[0]
                .downtime
                .iter()
                .map(|d| d.major_secs)
                .sum::<i64>()
        };

        // Out of the window from an hour ago, back in from half an hour ago.
        store
            .update(org, window, cover(vec![elsewhere]), WriteSource::Api, None)
            .await
            .unwrap()
            .expect("dropped");
        sqlx::query(
            "UPDATE maintenance_window_departures SET left_at = now() - interval '1 hour'
             WHERE maintenance_id = $1",
        )
        .bind(window)
        .execute(&pool)
        .await
        .unwrap();
        store
            .update(
                org,
                window,
                cover(vec![on_page, elsewhere]),
                WriteSource::Api,
                None,
            )
            .await
            .unwrap()
            .expect("back in");
        sqlx::query(
            "UPDATE maintenance_window_components SET joined_at = now() - interval '30 minutes'
             WHERE maintenance_id = $1 AND target_id = $2",
        )
        .bind(window)
        .bind(on_page)
        .execute(&pool)
        .await
        .unwrap();
        outage(50, 45).await.expect("outage while out of the window");
        outage(20, 15).await.expect("outage while back in it");

        assert_eq!(downtime().await, 5 * 60, "only the outage out of the window");
        store
            .update(
                org,
                window,
                cover(vec![elsewhere, on_page]),
                WriteSource::Api,
                None,
            )
            .await
            .unwrap()
            .expect("kept");
        assert_eq!(
            downtime().await,
            5 * 60,
            "an edit that keeps the monitor keeps when it came back"
        );
        store
            .update(org, window, cover(vec![elsewhere]), WriteSource::Api, None)
            .await
            .unwrap()
            .expect("dropped again");
        assert_eq!(
            downtime().await,
            5 * 60,
            "both stretches in the window still hold after it left again"
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL + CLICKHOUSE_URL"]
async fn a_page_lists_a_window_its_monitor_left_before_the_page_had_it() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let Some(ch) = common::ch_client_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    with_org_cleanup(&pool, org, async {
        let dropped = seed_target(&pool, org, "mh-dropped-first").await;
        let stays = seed_target(&pool, org, "mh-stays-in").await;
        let window = seed_window(&pool, org, "before the page", -2, 1, &[dropped, stays]).await;
        PgMaintenanceStore::new(pool.clone())
            .update(
                org,
                window,
                uptimepage::domain::MaintenanceWindowUpdate {
                    component_ids: Some(vec![stays]),
                    ..Default::default()
                },
                WriteSource::Api,
                None,
            )
            .await
            .unwrap()
            .expect("dropped");
        sqlx::query(
            "UPDATE maintenance_window_departures SET left_at = now() - interval '30 minutes'
             WHERE maintenance_id = $1",
        )
        .bind(window)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE maintenance_windows SET ends_at = now() - interval '1 minute' WHERE id = $1",
        )
        .bind(window)
        .execute(&pool)
        .await
        .unwrap();

        let page = seed_page(&pool, org, dropped).await;
        let other_page = seed_page(&pool, org, dropped).await;
        let agg = aggregator(&pool, ch);
        let (built, _, _, _) = agg.build(page, org).await.expect("build");
        assert_eq!(
            built.groups[0].components[0].history.last(),
            Some(&uptimepage::domain::DayState::Maintenance),
            "the strip paints the stretch the monitor spent in the window"
        );
        assert_eq!(titles(&built.recent_maintenance), ["before the page"]);
        assert_eq!(
            built.recent_maintenance[0].affected_component_names,
            ["Public API"]
        );

        // One page loses the monitor, then the monitor is deleted from the other.
        assert!(
            PgStatusPageStore::new(pool.clone())
                .remove_component(org, page, dropped, None)
                .await
                .unwrap()
        );
        assert!(
            PostgresTargetStore::from_pool(pool.clone(), None)
                .delete(org, dropped, None)
                .await
                .unwrap()
        );
        for (p, how) in [(page, "removed"), (other_page, "deleted")] {
            let (kept, _, _, _) = agg.build(p, org).await.expect("build");
            assert_eq!(
                titles(&kept.recent_maintenance),
                ["before the page"],
                "a page keeps the window once its monitor is {how}"
            );
            assert_eq!(
                kept.recent_maintenance[0].affected_component_names,
                ["Public API"]
            );
        }
    })
    .await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL + CLICKHOUSE_URL"]
async fn deleting_a_monitor_while_a_window_drops_it_lets_both_finish() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    with_org_cleanup(&pool, org, async {
        let doomed = seed_target(&pool, org, "mh-doomed").await;
        let stays = seed_target(&pool, org, "mh-survivor").await;
        let page = seed_page(&pool, org, doomed).await;
        let window = seed_window(&pool, org, "racing", -1, 1, &[doomed, stays]).await;

        // The deletion's first step: it holds the monitor.
        let mut deletion = pool.begin().await.unwrap();
        sqlx::query("SET LOCAL lock_timeout = '5s'")
            .execute(&mut *deletion)
            .await
            .unwrap();
        let deletion_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *deletion)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM targets WHERE id = $1 AND org_id = $2 FOR UPDATE")
            .bind(doomed)
            .bind(org.0)
            .execute(&mut *deletion)
            .await
            .unwrap();

        let edit = tokio::spawn({
            let pool = pool.clone();
            async move {
                PgMaintenanceStore::new(pool)
                    .update(
                        org,
                        window,
                        uptimepage::domain::MaintenanceWindowUpdate {
                            component_ids: Some(vec![stays]),
                            ..Default::default()
                        },
                        WriteSource::Api,
                        None,
                    )
                    .await
            }
        });
        wait_until(|| async { waiting_on(&pool, deletion_pid).await > 0 }).await;

        // The deletion keeps the window on the page, then drops the monitor.
        sqlx::query(
            "INSERT INTO maintenance_status_pages
                 (org_id, maintenance_id, status_page_id, target_ref, component_name)
             VALUES ($1, $2, $3, $4, 'Public API')",
        )
        .bind(org.0)
        .bind(window)
        .bind(page.0)
        .bind(doomed)
        .execute(&mut *deletion)
        .await
        .expect("the deletion pins the window");
        sqlx::query("DELETE FROM targets WHERE id = $1 AND org_id = $2")
            .bind(doomed)
            .bind(org.0)
            .execute(&mut *deletion)
            .await
            .expect("the deletion removes the monitor");
        deletion.commit().await.expect("the deletion commits");

        let edited = edit
            .await
            .unwrap()
            .expect("the edit goes through")
            .expect("the window is still there");
        assert_eq!(edited.component_ids, [stays]);
    })
    .await;
}

/// How many sessions wait on a lock session `holder` holds.
async fn waiting_on(pool: &PgPool, holder: i32) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid))",
    )
    .bind(holder)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn wait_until<F, Fut>(done: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..250 {
        if done().await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the sessions never reached the expected waits");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL + CLICKHOUSE_URL"]
async fn deleting_a_monitor_another_edit_just_added_lets_both_finish() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    with_org_cleanup(&pool, org, async {
        let added = seed_target(&pool, org, "mh-added").await;
        let stays = seed_target(&pool, org, "mh-kept").await;
        let page = seed_page(&pool, org, added).await;
        let window = seed_window(&pool, org, "contended", -1, 1, &[stays]).await;

        // Another edit holds the window and adds the monitor.
        let mut other = pool.begin().await.unwrap();
        let other_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *other)
            .await
            .unwrap();
        sqlx::query(
            "SELECT 1 FROM maintenance_windows WHERE id = $1 AND org_id = $2 FOR NO KEY UPDATE",
        )
        .bind(window)
        .bind(org.0)
        .execute(&mut *other)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO maintenance_window_components (org_id, maintenance_id, target_id, joined_at)
             VALUES ($1, $2, $3, now())",
        )
        .bind(org.0)
        .bind(window)
        .bind(added)
        .execute(&mut *other)
        .await
        .unwrap();

        // This edit, which drops it, waits for that one.
        let edit = tokio::spawn({
            let pool = pool.clone();
            async move {
                PgMaintenanceStore::new(pool)
                    .update(
                        org,
                        window,
                        uptimepage::domain::MaintenanceWindowUpdate {
                            component_ids: Some(vec![stays]),
                            ..Default::default()
                        },
                        WriteSource::Api,
                        None,
                    )
                    .await
            }
        });
        wait_until(|| async { waiting_on(&pool, other_pid).await >= 1 }).await;

        // A deletion of the monitor queues for it behind the adding edit.
        let (pid_tx, pid_rx) = tokio::sync::oneshot::channel();
        let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
        let deletion = tokio::spawn({
            let pool = pool.clone();
            async move {
                let mut tx = pool.begin().await?;
                sqlx::query("SET LOCAL lock_timeout = '5s'")
                    .execute(&mut *tx)
                    .await?;
                let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                    .fetch_one(&mut *tx)
                    .await?;
                pid_tx.send(pid).ok();
                sqlx::query("SELECT id FROM targets WHERE id = $1 AND org_id = $2 FOR UPDATE")
                    .bind(added)
                    .bind(org.0)
                    .execute(&mut *tx)
                    .await?;
                go_rx.await.ok();
                sqlx::query(
                    "INSERT INTO maintenance_status_pages
                         (org_id, maintenance_id, status_page_id, target_ref, component_name)
                     VALUES ($1, $2, $3, $4, 'Public API')
                     ON CONFLICT DO NOTHING",
                )
                .bind(org.0)
                .bind(window)
                .bind(page.0)
                .bind(added)
                .execute(&mut *tx)
                .await?;
                sqlx::query("DELETE FROM targets WHERE id = $1 AND org_id = $2")
                    .bind(added)
                    .bind(org.0)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await
            }
        });
        let deletion_pid = pid_rx.await.unwrap();
        wait_until(|| async { waiting_on(&pool, other_pid).await >= 2 }).await;

        other.commit().await.unwrap();
        wait_until(|| async {
            edit.is_finished() || waiting_on(&pool, deletion_pid).await > 0
        })
        .await;
        go_tx.send(()).unwrap();

        deletion
            .await
            .unwrap()
            .expect("the deletion goes through");
        let edited = edit
            .await
            .unwrap()
            .expect("the edit goes through")
            .expect("the window is still there");
        assert_eq!(edited.component_ids, [stays]);
    })
    .await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL + CLICKHOUSE_URL"]
async fn deleting_a_monitor_while_a_page_removes_it_lets_both_finish() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    with_org_cleanup(&pool, org, async {
        let doomed = seed_target(&pool, org, "mh-removed-and-deleted").await;
        let page = seed_page(&pool, org, doomed).await;
        let incident = seed_incident(&pool, org, doomed, -2).await;

        // The deletion's first steps: it holds the monitor and its incidents.
        let mut deletion = pool.begin().await.unwrap();
        sqlx::query("SET LOCAL lock_timeout = '5s'")
            .execute(&mut *deletion)
            .await
            .unwrap();
        let deletion_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *deletion)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM targets WHERE id = $1 AND org_id = $2 FOR UPDATE")
            .bind(doomed)
            .bind(org.0)
            .execute(&mut *deletion)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM incidents WHERE target_id = $1 AND org_id = $2 FOR UPDATE")
            .bind(doomed)
            .bind(org.0)
            .execute(&mut *deletion)
            .await
            .unwrap();

        let removal = tokio::spawn({
            let pool = pool.clone();
            async move {
                PgStatusPageStore::new(pool)
                    .remove_component(org, page, doomed, None)
                    .await
            }
        });
        wait_until(|| async { waiting_on(&pool, deletion_pid).await > 0 }).await;

        // The deletion keeps the incident on the page, then drops the monitor.
        sqlx::query(
            "INSERT INTO incident_status_pages
                 (org_id, incident_id, status_page_id, component_name)
             VALUES ($1, $2, $3, 'Public API')
             ON CONFLICT DO NOTHING",
        )
        .bind(org.0)
        .bind(incident)
        .bind(page.0)
        .execute(&mut *deletion)
        .await
        .expect("the deletion pins the incident");
        sqlx::query("DELETE FROM targets WHERE id = $1 AND org_id = $2")
            .bind(doomed)
            .bind(org.0)
            .execute(&mut *deletion)
            .await
            .expect("the deletion removes the monitor");
        deletion.commit().await.expect("the deletion commits");

        let removed = removal.await.unwrap().expect("the removal goes through");
        assert!(
            !removed,
            "the deletion already took the monitor off the page"
        );
    })
    .await;
}

/// Records which pages were invalidated; answers nothing else.
#[derive(Default)]
struct Invalidations(parking_lot::Mutex<Vec<uptimepage::domain::StatusPageId>>);

#[async_trait::async_trait]
impl PublicSource for Invalidations {
    async fn page(
        &self,
        page: PageRef,
    ) -> Result<Arc<uptimepage::domain::PublicStatusPage>, uptimepage::error::PublicAppError> {
        uptimepage::public_status::NoopPublicSource.page(page).await
    }
    async fn component_history(
        &self,
        page: PageRef,
        id: Uuid,
        days: u32,
    ) -> Result<uptimepage::domain::ComponentHistoryResponse, uptimepage::error::PublicAppError>
    {
        uptimepage::public_status::NoopPublicSource
            .component_history(page, id, days)
            .await
    }
    async fn list_incidents(
        &self,
        page: PageRef,
        q: IncidentListQuery,
    ) -> Result<
        uptimepage::pagination::page::CursorPage<uptimepage::domain::PublicIncident>,
        uptimepage::error::PublicAppError,
    > {
        uptimepage::public_status::NoopPublicSource
            .list_incidents(page, q)
            .await
    }
    async fn incident_by_id(
        &self,
        page: PageRef,
        id: Uuid,
    ) -> Result<uptimepage::domain::PublicIncident, uptimepage::error::PublicAppError> {
        uptimepage::public_status::NoopPublicSource
            .incident_by_id(page, id)
            .await
    }
    async fn maintenance(
        &self,
        page: PageRef,
    ) -> Result<uptimepage::domain::PublicMaintenanceList, uptimepage::error::PublicAppError> {
        uptimepage::public_status::NoopPublicSource
            .maintenance(page)
            .await
    }
    async fn list_maintenance(
        &self,
        page: PageRef,
        q: MaintenanceListQuery,
    ) -> Result<
        uptimepage::pagination::page::CursorPage<uptimepage::domain::PublicMaintenance>,
        uptimepage::error::PublicAppError,
    > {
        uptimepage::public_status::NoopPublicSource
            .list_maintenance(page, q)
            .await
    }
    async fn incidents_rss(
        &self,
        page: PageRef,
        links: uptimepage::public_status::source::FeedLinks<'_>,
    ) -> Result<String, uptimepage::error::PublicAppError> {
        uptimepage::public_status::NoopPublicSource
            .incidents_rss(page, links)
            .await
    }
    async fn invalidate(&self, page: uptimepage::domain::StatusPageId) {
        self.0.lock().push(page);
    }
}

#[tokio::test]
#[ignore = "requires DATABASE_URL + CLICKHOUSE_URL"]
async fn a_change_to_a_kept_incident_reaches_the_page_that_kept_it() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    with_org_cleanup(&pool, org, async {
        let target = seed_target(&pool, org, "mh-kept-incident").await;
        let page = seed_page(&pool, org, target).await;
        let incident = seed_incident(&pool, org, target, -3).await;
        PgStatusPageStore::new(pool.clone())
            .remove_component(org, page, target, None)
            .await
            .unwrap();

        let pages = PgStatusPageStore::new(pool.clone());
        let shares = uptimepage::storage::PgMonitorShareStore::new(pool.clone(), None);
        let incidents = uptimepage::storage::PgIncidentOpsStore::new(pool.clone());
        let source = Invalidations::default();
        let ops = incidents.get(org, incident).await.unwrap().unwrap();
        uptimepage::public_status::publishing::Publishing {
            pages: &pages,
            shares: &shares,
            incidents: &incidents,
            source: &source,
        }
        .invalidate_incident(org, &ops)
        .await;
        assert!(
            source.0.lock().contains(&page),
            "unpublishing it must not leave the kept page serving it"
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL + CLICKHOUSE_URL"]
async fn the_archive_lists_a_window_that_ran_into_its_span() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let Some(ch) = common::ch_client_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    with_org_cleanup(&pool, org, async {
        let target = seed_target(&pool, org, "mh-long-window").await;
        let page = seed_page(&pool, org, target).await;
        seed_window(&pool, org, "began long ago", -100 * 24, -80 * 24, &[target]).await;
        seed_window(&pool, org, "over long ago", -120 * 24, -100 * 24, &[target]).await;

        let agg = aggregator(&pool, ch);
        let source = public_source(&pool, agg);
        let past = source
            .list_maintenance(
                PageRef { page, org },
                MaintenanceListQuery {
                    limit: 100,
                    cursor: None,
                    ended_only: true,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            titles(&past.items),
            ["began long ago"],
            "the strip paints it, so the archive it links to must list it"
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL + CLICKHOUSE_URL"]
async fn a_page_keeps_a_window_that_ran_into_its_span_when_the_monitor_leaves() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let Some(ch) = common::ch_client_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    with_org_cleanup(&pool, org, async {
        let target = seed_target(&pool, org, "mh-long-kept").await;
        let page = seed_page(&pool, org, target).await;
        seed_window(&pool, org, "began long ago", -100 * 24, -80 * 24, &[target]).await;
        let over = seed_window(&pool, org, "over long ago", -120 * 24, -100 * 24, &[target]).await;
        let source = public_source(&pool, aggregator(&pool, ch));
        let archive = || async {
            source
                .list_maintenance(
                    PageRef { page, org },
                    MaintenanceListQuery {
                        limit: 100,
                        cursor: None,
                        ended_only: true,
                    },
                )
                .await
                .unwrap()
        };
        assert_eq!(titles(&archive().await.items), ["began long ago"]);

        assert!(
            PgStatusPageStore::new(pool.clone())
                .remove_component(org, page, target, None)
                .await
                .unwrap()
        );
        source.invalidate(page).await;
        assert_eq!(
            titles(&archive().await.items),
            ["began long ago"],
            "the archive listed it before the monitor left, so it keeps it"
        );
        let kept_over: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM maintenance_status_pages WHERE maintenance_id = $1",
        )
        .bind(over)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            kept_over, 0,
            "what ended before the archive's span is not kept"
        );
    })
    .await;
}
