//! ClickHouse-backed integration tests for the public-status aggregator.
//!
//! These exercise the real `clickhouse-rs` bind/deserialize path, which unit
//! tests with in-memory stores miss. Both bugs that produced
//! `STATUS_DATA_UNAVAILABLE` in dev (UUID array bind + DateTime→i64 deser)
//! would have been caught by `build_round_trips_seeded_data` below.
//!
//! Skipped by default: requires the dev compose stack.
//!
//!     docker compose -f compose.dev.yml up -d
//!     DATABASE_URL=postgres://monitor:monitor@127.0.0.1:5432/monitor \
//!     CLICKHOUSE_URL=http://127.0.0.1:8123 \
//!       cargo test --test clickhouse_aggregator_test -- --ignored
//!
//! Each test purges its own prefix at the start (idempotent — recovers from
//! prior crashed runs) and deletes its seeded target at the end, even on
//! panic. ClickHouse rows are left to expire via the 90-day TTL since `ALTER
//! TABLE ... DELETE` is async + expensive and our fresh UUIDs per run
//! prevent cross-test interference.

mod common;

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures::FutureExt;
use sqlx::PgPool;
use uptimepage::domain::{
    CheckResult, CheckSpec, CheckStatus, DayState, ExpectedStatus, NewMonitorShare, NewStatusPage,
    NewStatusPageComponent, NewTarget, OrgId, OverallState, PublicComponentStatus,
    StatusPageComponentUpdate, StatusPageId, WriteSource,
};
use uptimepage::public_status::{AggregatorConfig, OrgAggregator};
use uptimepage::storage::{
    ClickhouseResultSink, CreateShareOutcome, MonitorShareStore, PgMonitorShareStore,
    PgStatusPageStore, PostgresTargetStore, ResultSink, StatusPageStore, TargetStore,
};
use url::Url;
use uuid::Uuid;

use crate::common::default_http_check;

/// Publish `target_id` on a fresh enabled page and return the page id, so the
/// page-keyed aggregator has a component set to filter on.
async fn seed_page_with_target(pool: &PgPool, org: OrgId, target_id: Uuid) -> StatusPageId {
    let store = PgStatusPageStore::new(pool.clone());
    let slug = format!("aggpage{}", Uuid::now_v7().simple());
    let slug = slug[..slug.len().min(30)].to_string();
    let page = store
        .create(
            org,
            NewStatusPage {
                slug,
                name: "Agg".into(),
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
                target_id,
                public_name: None,
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

async fn seed_org(pool: &sqlx::PgPool, prefix: &str) -> uptimepage::domain::OrgId {
    let slug = format!("{prefix}-{}", Uuid::now_v7().simple());
    let slug = &slug[..slug.len().min(30)];
    let (id,): (Uuid,) = sqlx::query_as(
        "WITH a AS (INSERT INTO accounts DEFAULT VALUES RETURNING id) \
         INSERT INTO organizations (slug, name, account_id) \
         SELECT $1, 'Agg Test', a.id FROM a RETURNING id",
    )
    .bind(slug)
    .fetch_one(pool)
    .await
    .expect("insert agg-test org");
    uptimepage::domain::OrgId(id)
}

fn public_target(name: &str) -> NewTarget {
    let url = Url::parse("https://example.com/").unwrap();
    NewTarget {
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
    }
}

fn ok_result(target_id: Uuid, org_id: Uuid, ts: chrono::DateTime<Utc>) -> CheckResult {
    CheckResult {
        target_id,
        org_id,
        timestamp: ts,
        status: CheckStatus::Up,
        duration_ms: 42,
        dns_ms: None,
        connect_ms: None,
        tls_ms: None,
        ttfb_ms: None,
        response_code: Some(200),
        response_size: None,
        diagnostic: None,
        error: None,
    }
}

/// Best-effort cleanup of leftover rows from prior runs. Scoped to a prefix
/// so parallel tests with different prefixes don't fight. Cascades to
/// incidents and maintenance_window_components via FK.
async fn purge_prefix(pool: &PgPool, prefix: &str) {
    let _ = sqlx::query("DELETE FROM targets WHERE name LIKE $1")
        .bind(format!("{prefix}%"))
        .execute(pool)
        .await;
}

async fn delete_target(pool: &PgPool, id: Uuid) {
    let _ = sqlx::query("DELETE FROM targets WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await;
}

/// Runs `f` and always cleans up `target_id` from PG, even if `f` panics.
/// Re-raises the panic afterwards so the test still fails loudly.
async fn with_cleanup<F>(pool: &PgPool, target_id: Uuid, f: F)
where
    F: std::future::Future<Output = ()>,
{
    let result = AssertUnwindSafe(f).catch_unwind().await;
    delete_target(pool, target_id).await;
    if let Err(p) = result {
        std::panic::resume_unwind(p);
    }
}

/// The rendered page keeps the order the operator dragged the components into:
/// a group sits where its earliest component sits, and an ungrouped one is not
/// pinned to the end.
#[tokio::test]
#[ignore = "requires DATABASE_URL + CLICKHOUSE_URL — run via `docker compose -f compose.dev.yml up -d` then `cargo test -- --ignored`"]
async fn rendered_groups_follow_the_stored_order() {
    let Some(pool) = common::pg_pool_from_env().await else {
        eprintln!("skipped: DATABASE_URL not set");
        return;
    };
    let Some(ch) = common::ch_client_from_env().await else {
        eprintln!("skipped: CLICKHOUSE_URL not set");
        return;
    };

    let org_id = seed_org(&pool, "agg-order").await;
    let store = Arc::new(PostgresTargetStore::from_pool(pool.clone(), None));
    let pages = PgStatusPageStore::new(pool.clone());
    let mut ids = Vec::new();
    for name in ["ns1", "site", "notes"] {
        let unique = format!("agg-test-{name}-{}", Uuid::now_v7());
        let target = store
            .create(
                org_id,
                public_target(&unique),
                WriteSource::Ui,
                i64::MAX,
                i64::MAX,
            )
            .await
            .expect("create public target");
        ids.push(target.id);
    }
    let (ns1, site, notes) = (ids[0], ids[1], ids[2]);

    let page_id = seed_page_with_target(&pool, org_id, ns1).await;
    for (target_id, group) in [(site, Some("NQUARE")), (notes, None)] {
        pages
            .add_component(
                org_id,
                page_id,
                NewStatusPageComponent {
                    target_id,
                    public_name: None,
                    public_description: None,
                    public_group: group.map(str::to_owned),
                    sort_order: 0,
                    detail_link_enabled: false,
                },
                i64::MAX,
                None,
            )
            .await
            .expect("add component");
    }
    pages
        .update_component(
            org_id,
            page_id,
            ns1,
            StatusPageComponentUpdate {
                public_group: Some(Some("DNS".into())),
                ..Default::default()
            },
        )
        .await
        .expect("group ns1");
    pages
        .reorder_components(org_id, page_id, &[notes, site, ns1])
        .await
        .expect("reorder");

    let agg = OrgAggregator::new(pool.clone(), ch, AggregatorConfig::default(), None);
    let (page, _markers, _names, _hidden) =
        agg.build(page_id, org_id).await.expect("aggregator build");

    let rendered: Vec<Option<String>> = page.groups.iter().map(|g| g.name.clone()).collect();
    assert_eq!(
        rendered,
        vec![None, Some("NQUARE".into()), Some("DNS".into())],
        "ungrouped first, then the groups in dragged order"
    );

    for target_id in ids {
        delete_target(&pool, target_id).await;
    }
}

/// Exercises the history-strip `has(?, target_id)` query site and the
/// `DateTime → i64` deserialization in its `SELECT`. Either bug breaks this
/// test with a 503-equivalent error.
#[tokio::test]
#[ignore = "requires DATABASE_URL + CLICKHOUSE_URL — run via `docker compose -f compose.dev.yml up -d` then `cargo test -- --ignored`"]
async fn build_round_trips_seeded_data() {
    let Some(pool) = common::pg_pool_from_env().await else {
        eprintln!("skipped: DATABASE_URL not set");
        return;
    };
    let Some(ch) = common::ch_client_from_env().await else {
        eprintln!("skipped: CLICKHOUSE_URL not set");
        return;
    };

    purge_prefix(&pool, "agg-test-").await;

    let org_id = seed_org(&pool, "agg-a").await;
    let unique = format!("agg-test-{}", Uuid::now_v7());
    let store = Arc::new(PostgresTargetStore::from_pool(pool.clone(), None));
    let target = store
        .create(
            org_id,
            public_target(&unique),
            WriteSource::Ui,
            i64::MAX,
            i64::MAX,
        )
        .await
        .expect("create public target");
    let target_id = target.id;
    let pool_for_cleanup = pool.clone();

    with_cleanup(&pool_for_cleanup, target_id, async move {
        let sink = ClickhouseResultSink::new(
            ch.clone(),
            "default".into(),
            "default".into(),
            uptimepage::storage::OrgTtlDays::new(),
        );
        let now = Utc::now();
        let rows: Vec<CheckResult> = (0..5)
            .map(|i| ok_result(target_id, org_id.0, now - chrono::Duration::seconds(i * 30)))
            .collect();
        sink.write_batch(&rows).await.expect("ch insert");

        let page_id = seed_page_with_target(&pool, org_id, target_id).await;
        let agg = OrgAggregator::new(pool, ch, AggregatorConfig::default(), None);
        let (page, _markers, _names, _hidden) =
            agg.build(page_id, org_id).await.expect("aggregator build");

        let component = page
            .groups
            .iter()
            .flat_map(|g| &g.components)
            .find(|c| c.id == target_id)
            .expect("seeded public component present in page");

        // No open confirmed incident → operational.
        assert_eq!(component.current_status, PublicComponentStatus::Operational);
        assert_eq!(component.history.len(), 90);
    })
    .await;
}

/// Detail link needs both the opt-in and a live share.
#[tokio::test]
#[ignore = "requires DATABASE_URL + CLICKHOUSE_URL — run via `docker compose -f compose.dev.yml up -d` then `cargo test -- --ignored`"]
async fn detail_link_renders_only_for_a_live_share() {
    let Some(pool) = common::pg_pool_from_env().await else {
        eprintln!("skipped: DATABASE_URL not set");
        return;
    };
    let Some(ch) = common::ch_client_from_env().await else {
        eprintln!("skipped: CLICKHOUSE_URL not set");
        return;
    };
    let org_id = seed_org(&pool, "agg-dl").await;
    let targets = PostgresTargetStore::from_pool(pool.clone(), None);
    let target = targets
        .create(
            org_id,
            public_target("dl"),
            WriteSource::Ui,
            i64::MAX,
            i64::MAX,
        )
        .await
        .expect("create target");
    let target_id = target.id;
    let pool_for_cleanup = pool.clone();

    with_cleanup(&pool_for_cleanup, target_id, async move {
        let page_id = seed_page_with_target(&pool, org_id, target_id).await;
        let pages = PgStatusPageStore::new(pool.clone());
        let shares = PgMonitorShareStore::new(pool.clone(), None);
        let cfg = AggregatorConfig {
            app_base_url: "https://app.example.com".into(),
            ..AggregatorConfig::default()
        };
        let detail_url = |page: &uptimepage::domain::PublicStatusPage| {
            page.groups
                .iter()
                .flat_map(|g| &g.components)
                .find(|c| c.id == target_id)
                .expect("seeded component")
                .detail_url
                .clone()
        };

        let CreateShareOutcome::Created(created) = shares
            .create(
                org_id,
                target_id,
                NewMonitorShare {
                    label: None,
                    expires_at: None,
                },
                None,
                None,
                None,
            )
            .await
            .expect("mint share")
        else {
            panic!("share mint refused");
        };
        pages
            .attach_share(org_id, page_id, target_id, None, created.share.id)
            .await
            .expect("attach share");

        let agg = OrgAggregator::new(pool.clone(), ch.clone(), cfg.clone(), None);
        let (page, _, _, _) = agg.build(page_id, org_id).await.expect("build");
        assert_eq!(detail_url(&page), None, "flag off means no link");

        pages
            .update_component(
                org_id,
                page_id,
                target_id,
                StatusPageComponentUpdate {
                    detail_link_enabled: Some(true),
                    ..Default::default()
                },
            )
            .await
            .expect("enable detail link");
        let agg = OrgAggregator::new(pool.clone(), ch.clone(), cfg.clone(), None);
        let (page, _, _, _) = agg.build(page_id, org_id).await.expect("build");
        assert_eq!(
            detail_url(&page),
            Some(format!("https://app.example.com/m/{}", created.token)),
            "opted-in component links at the share token"
        );

        shares
            .revoke(org_id, target_id, created.share.id, None)
            .await
            .expect("revoke share");
        let agg = OrgAggregator::new(pool.clone(), ch.clone(), cfg, None);
        let (page, _, _, _) = agg.build(page_id, org_id).await.expect("build");
        assert_eq!(
            detail_url(&page),
            None,
            "revoking the share unlinks without removing the component"
        );
    })
    .await;
}

/// The public face of the heartbeat pending state: an unwired job has not been
/// proven healthy, so the banner must not borrow confidence from it.
#[tokio::test]
#[ignore = "requires DATABASE_URL + CLICKHOUSE_URL — run via `docker compose -f compose.dev.yml up -d` then `cargo test -- --ignored`"]
async fn a_component_that_recorded_nothing_is_no_data_not_operational() {
    let Some(pool) = common::pg_pool_from_env().await else {
        eprintln!("skipped: DATABASE_URL not set");
        return;
    };
    let Some(ch) = common::ch_client_from_env().await else {
        eprintln!("skipped: CLICKHOUSE_URL not set");
        return;
    };

    purge_prefix(&pool, "agg-silent-").await;

    let org_id = seed_org(&pool, "agg-silent").await;
    let unique = format!("agg-silent-{}", Uuid::now_v7());
    let store = Arc::new(PostgresTargetStore::from_pool(pool.clone(), None));
    let target = store
        .create(
            org_id,
            public_target(&unique),
            WriteSource::Ui,
            i64::MAX,
            i64::MAX,
        )
        .await
        .expect("create public target");
    let target_id = target.id;
    let pool_for_cleanup = pool.clone();

    with_cleanup(&pool_for_cleanup, target_id, async move {
        // Deliberately no ClickHouse rows and no incident.
        let page_id = seed_page_with_target(&pool, org_id, target_id).await;
        let agg = OrgAggregator::new(pool, ch, AggregatorConfig::default(), None);
        let (page, _markers, _names, _hidden) =
            agg.build(page_id, org_id).await.expect("aggregator build");

        let component = page
            .groups
            .iter()
            .flat_map(|g| &g.components)
            .find(|c| c.id == target_id)
            .expect("seeded public component present in page");

        assert_eq!(component.current_status, PublicComponentStatus::NoData);
        assert!(
            component.history.iter().all(|d| *d == DayState::NoData),
            "the pill and the strip under it agree"
        );
        assert_eq!(
            page.overall.state,
            OverallState::Operational,
            "silence carries no evidence, so it neither raises nor holds the banner"
        );
    })
    .await;
}

/// `component_history` is a separate code path from `build()`. It hits the
/// same history-strip query directly — would have caught the DateTime→i64
/// bug independently of the page-build smoke test.
#[tokio::test]
#[ignore = "requires DATABASE_URL + CLICKHOUSE_URL — run via `docker compose -f compose.dev.yml up -d` then `cargo test -- --ignored`"]
async fn component_history_returns_strip_for_public_target() {
    let Some(pool) = common::pg_pool_from_env().await else {
        eprintln!("skipped: DATABASE_URL not set");
        return;
    };
    let Some(ch) = common::ch_client_from_env().await else {
        eprintln!("skipped: CLICKHOUSE_URL not set");
        return;
    };

    purge_prefix(&pool, "hist-test-").await;

    let org_id = seed_org(&pool, "agg-b").await;
    let unique = format!("hist-test-{}", Uuid::now_v7());
    let store = Arc::new(PostgresTargetStore::from_pool(pool.clone(), None));
    let target = store
        .create(
            org_id,
            public_target(&unique),
            WriteSource::Ui,
            i64::MAX,
            i64::MAX,
        )
        .await
        .expect("create public target");
    let target_id = target.id;
    let pool_for_cleanup = pool.clone();

    with_cleanup(&pool_for_cleanup, target_id, async move {
        let sink = ClickhouseResultSink::new(
            ch.clone(),
            "default".into(),
            "default".into(),
            uptimepage::storage::OrgTtlDays::new(),
        );
        let now = Utc::now();
        sink.write_batch(&[ok_result(target_id, org_id.0, now)])
            .await
            .expect("ch insert");

        let page_id = seed_page_with_target(&pool, org_id, target_id).await;
        let agg = OrgAggregator::new(pool, ch, AggregatorConfig::default(), None);
        let resp = agg
            .component_history(page_id, org_id, target_id, 7)
            .await
            .expect("component_history succeeds");
        assert_eq!(resp.component_id, target_id);
        assert_eq!(resp.days, 7);
        assert_eq!(resp.history.len(), 7);
    })
    .await;
}

/// The visibility gate: an `internal` incident on a public component must not
/// surface in the page's active list, recent list, or history markers — only
/// `public` incidents do. Guards the internal-vs-public separation end-to-end.
#[tokio::test]
#[ignore = "requires DATABASE_URL + CLICKHOUSE_URL"]
async fn build_excludes_internal_incidents() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let Some(ch) = common::ch_client_from_env().await else {
        return;
    };
    purge_prefix(&pool, "agg-vis-").await;

    let org_id = seed_org(&pool, "agg-vis").await;
    let store = Arc::new(PostgresTargetStore::from_pool(pool.clone(), None));
    let mk_target = |role: &str| public_target(&format!("agg-vis-{role}-{}", Uuid::now_v7()));
    // The unique open-incident index forbids two open incidents on one target,
    // so the public and internal incidents live on separate page components;
    // the aggregator must still drop the internal one by visibility alone.
    let pub_target = store
        .create(
            org_id,
            mk_target("pub"),
            WriteSource::Ui,
            i64::MAX,
            i64::MAX,
        )
        .await
        .expect("create public target");
    let int_target = store
        .create(
            org_id,
            mk_target("int"),
            WriteSource::Ui,
            i64::MAX,
            i64::MAX,
        )
        .await
        .expect("create internal target");
    let pub_target_id = pub_target.id;
    let int_target_id = int_target.id;
    let pool_for_cleanup = pool.clone();

    let body = async move {
        let now = Utc::now();
        let mk = |tid: Uuid, vis: &str| {
            sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO incidents (org_id, target_id, started_at, status_at_start, visibility) \
                 VALUES ($1, $2, $3, 'down', $4) RETURNING id",
            )
            .bind(org_id.0)
            .bind(tid)
            .bind(now - chrono::Duration::minutes(5))
            .bind(vis.to_string())
        };
        let public_id = mk(pub_target_id, "public")
            .fetch_one(&pool)
            .await
            .expect("insert public incident");
        let _internal_id = mk(int_target_id, "internal")
            .fetch_one(&pool)
            .await
            .expect("insert internal incident");

        let page_id = seed_page_with_target(&pool, org_id, pub_target_id).await;
        PgStatusPageStore::new(pool.clone())
            .add_component(
                org_id,
                page_id,
                NewStatusPageComponent {
                    target_id: int_target_id,
                    public_name: None,
                    public_description: None,
                    public_group: None,
                    sort_order: 1,
                    detail_link_enabled: false,
                },
                i64::MAX,
                None,
            )
            .await
            .expect("add internal component");

        let agg = OrgAggregator::new(pool, ch, AggregatorConfig::default(), None);
        let (page, markers, _names, _hidden) =
            agg.build(page_id, org_id).await.expect("aggregator build");

        let active: Vec<Uuid> = page.active_incidents.iter().map(|i| i.id).collect();
        assert_eq!(
            active,
            vec![public_id],
            "only the public incident is active"
        );
        let recent: Vec<Uuid> = page.recent_incidents.iter().map(|i| i.id).collect();
        assert_eq!(
            recent,
            vec![public_id],
            "only the public incident is recent"
        );
        let marked: Vec<Uuid> = markers.iter().map(|m| m.id).collect();
        assert_eq!(
            marked,
            vec![public_id],
            "only the public incident is marked"
        );
    };

    let result = AssertUnwindSafe(body).catch_unwind().await;
    delete_target(&pool_for_cleanup, pub_target_id).await;
    delete_target(&pool_for_cleanup, int_target_id).await;
    if let Err(p) = result {
        std::panic::resume_unwind(p);
    }
}

/// Component state and the day strip derive from confirmed public incidents,
/// not raw samples: an open incident with surviving regions reads as a
/// partial outage, one with none as a major outage, and both paint today's
/// history cell. The banner takes the worst component.
#[tokio::test]
#[ignore = "requires DATABASE_URL + CLICKHOUSE_URL"]
async fn build_component_state_follows_confirmed_incidents() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let Some(ch) = common::ch_client_from_env().await else {
        return;
    };
    purge_prefix(&pool, "agg-conf-").await;

    let org_id = seed_org(&pool, "agg-conf").await;
    let store = Arc::new(PostgresTargetStore::from_pool(pool.clone(), None));
    let mk_target = |role: &str| public_target(&format!("agg-conf-{role}-{}", Uuid::now_v7()));
    let partial_target = store
        .create(
            org_id,
            mk_target("partial"),
            WriteSource::Ui,
            i64::MAX,
            i64::MAX,
        )
        .await
        .expect("create partial target");
    let major_target = store
        .create(
            org_id,
            mk_target("major"),
            WriteSource::Ui,
            i64::MAX,
            i64::MAX,
        )
        .await
        .expect("create major target");
    let partial_id = partial_target.id;
    let major_id = major_target.id;
    let pool_for_cleanup = pool.clone();

    let body = async move {
        let sink = ClickhouseResultSink::new(
            ch.clone(),
            "default".into(),
            "default".into(),
            uptimepage::storage::OrgTtlDays::new(),
        );
        let now = Utc::now();
        sink.write_batch(&[
            ok_result(partial_id, org_id.0, now),
            ok_result(major_id, org_id.0, now),
        ])
        .await
        .expect("ch insert");

        // Open incident with a surviving region → partial; without → major.
        sqlx::query(
            "INSERT INTO incidents (org_id, target_id, started_at, status_at_start, regions_up, visibility) \
             VALUES ($1, $2, $3, 'error', $4, 'public')",
        )
        .bind(org_id.0)
        .bind(partial_id)
        .bind(now - chrono::Duration::minutes(5))
        .bind(vec!["eu-helsinki".to_string()])
        .execute(&pool)
        .await
        .expect("insert partial incident");
        sqlx::query(
            "INSERT INTO incidents (org_id, target_id, started_at, status_at_start, visibility) \
             VALUES ($1, $2, $3, 'down', 'public')",
        )
        .bind(org_id.0)
        .bind(major_id)
        .bind(now - chrono::Duration::minutes(5))
        .execute(&pool)
        .await
        .expect("insert major incident");

        let page_id = seed_page_with_target(&pool, org_id, partial_id).await;
        PgStatusPageStore::new(pool.clone())
            .add_component(
                org_id,
                page_id,
                NewStatusPageComponent {
                    target_id: major_id,
                    public_name: None,
                    public_description: None,
                    public_group: None,
                    sort_order: 1,
                    detail_link_enabled: false,
                },
                i64::MAX,
                None,
            )
            .await
            .expect("add major component");

        let agg = OrgAggregator::new(pool, ch, AggregatorConfig::default(), None);
        let (page, _markers, _names, _hidden) =
            agg.build(page_id, org_id).await.expect("aggregator build");

        let component = |id: Uuid| {
            page.groups
                .iter()
                .flat_map(|g| &g.components)
                .find(|c| c.id == id)
                .expect("component present")
        };
        let partial = component(partial_id);
        let major = component(major_id);
        assert_eq!(
            partial.current_status,
            PublicComponentStatus::PartialOutage,
            "surviving region → partial outage"
        );
        assert_eq!(
            major.current_status,
            PublicComponentStatus::MajorOutage,
            "no surviving region → major outage"
        );
        assert_eq!(
            partial.history.last().copied(),
            Some(DayState::PartialOutage),
            "open incident paints today's cell"
        );
        assert_eq!(
            major.history.last().copied(),
            Some(DayState::MajorOutage),
            "open incident paints today's cell"
        );
        assert_eq!(page.overall.state, OverallState::MajorOutage);
    };

    let result = AssertUnwindSafe(body).catch_unwind().await;
    delete_target(&pool_for_cleanup, partial_id).await;
    delete_target(&pool_for_cleanup, major_id).await;
    if let Err(p) = result {
        std::panic::resume_unwind(p);
    }
}

/// An auto (monitor) incident is the writer's quorum-gated outage, so it paints
/// the strip and drives the component dot even while `internal`; the operator
/// can keep it out of the incident narrative but not off the uptime bar. An
/// `internal` manual incident stays hidden from both surfaces.
#[tokio::test]
#[ignore = "requires DATABASE_URL + CLICKHOUSE_URL"]
async fn internal_auto_incident_paints_strip_manual_stays_hidden() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let Some(ch) = common::ch_client_from_env().await else {
        return;
    };
    purge_prefix(&pool, "agg-auto-").await;

    let org_id = seed_org(&pool, "agg-auto").await;
    let store = Arc::new(PostgresTargetStore::from_pool(pool.clone(), None));
    let mk_target = |role: &str| public_target(&format!("agg-auto-{role}-{}", Uuid::now_v7()));
    let auto_target = store
        .create(
            org_id,
            mk_target("auto"),
            WriteSource::Ui,
            i64::MAX,
            i64::MAX,
        )
        .await
        .expect("create auto target");
    let manual_target = store
        .create(
            org_id,
            mk_target("manual"),
            WriteSource::Ui,
            i64::MAX,
            i64::MAX,
        )
        .await
        .expect("create manual target");
    let auto_id = auto_target.id;
    let manual_id = manual_target.id;
    let pool_for_cleanup = pool.clone();

    let body = async move {
        let now = Utc::now();
        let mk = |tid: Uuid, origin: &str| {
            sqlx::query(
                "INSERT INTO incidents (org_id, target_id, started_at, status_at_start, origin, visibility) \
                 VALUES ($1, $2, $3, 'down', $4, 'internal')",
            )
            .bind(org_id.0)
            .bind(tid)
            .bind(now - chrono::Duration::minutes(5))
            .bind(origin.to_string())
        };
        mk(auto_id, "monitor")
            .execute(&pool)
            .await
            .expect("insert internal auto incident");
        mk(manual_id, "manual")
            .execute(&pool)
            .await
            .expect("insert internal manual incident");

        let page_id = seed_page_with_target(&pool, org_id, auto_id).await;
        PgStatusPageStore::new(pool.clone())
            .add_component(
                org_id,
                page_id,
                NewStatusPageComponent {
                    target_id: manual_id,
                    public_name: None,
                    public_description: None,
                    public_group: None,
                    sort_order: 1,
                    detail_link_enabled: false,
                },
                i64::MAX,
                None,
            )
            .await
            .expect("add manual component");

        let agg = OrgAggregator::new(pool, ch, AggregatorConfig::default(), None);
        let (page, markers, _names, _hidden) =
            agg.build(page_id, org_id).await.expect("aggregator build");

        let component = |id: Uuid| {
            page.groups
                .iter()
                .flat_map(|g| &g.components)
                .find(|c| c.id == id)
                .expect("component present")
        };
        let auto = component(auto_id);
        let manual = component(manual_id);

        assert_eq!(
            auto.current_status,
            PublicComponentStatus::MajorOutage,
            "internal auto incident drives the component dot"
        );
        assert_eq!(
            auto.history.last().copied(),
            Some(DayState::MajorOutage),
            "internal auto incident paints the strip"
        );
        assert_eq!(page.overall.state, OverallState::MajorOutage);

        // Neither value is an outage, which is what this asserts; the strip
        // assertion below has always allowed the same pair.
        assert!(
            matches!(
                manual.current_status,
                PublicComponentStatus::Operational | PublicComponentStatus::NoData
            ),
            "internal manual incident does not touch the dot"
        );
        assert!(
            matches!(
                manual.history.last().copied(),
                Some(DayState::Operational | DayState::NoData)
            ),
            "internal manual incident does not paint the strip"
        );

        // Neither internal incident enters the curated narrative surfaces.
        assert!(
            page.active_incidents.is_empty(),
            "no internal incident is active"
        );
        assert!(markers.is_empty(), "no internal incident is marked");
    };

    let result = AssertUnwindSafe(body).catch_unwind().await;
    delete_target(&pool_for_cleanup, auto_id).await;
    delete_target(&pool_for_cleanup, manual_id).await;
    if let Err(p) = result {
        std::panic::resume_unwind(p);
    }
}

/// Publishing says something happened; counting says the service was down.
#[tokio::test]
#[ignore = "requires DATABASE_URL + CLICKHOUSE_URL"]
async fn a_published_declaration_paints_only_when_it_counts() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let Some(ch) = common::ch_client_from_env().await else {
        return;
    };
    purge_prefix(&pool, "agg-decl-").await;

    let org_id = seed_org(&pool, "agg-decl").await;
    let store = Arc::new(PostgresTargetStore::from_pool(pool.clone(), None));
    let mk_target = |role: &str| public_target(&format!("agg-decl-{role}-{}", Uuid::now_v7()));
    let notice_target = store
        .create(
            org_id,
            mk_target("notice"),
            WriteSource::Ui,
            i64::MAX,
            i64::MAX,
        )
        .await
        .expect("create notice target");
    let outage_target = store
        .create(
            org_id,
            mk_target("outage"),
            WriteSource::Ui,
            i64::MAX,
            i64::MAX,
        )
        .await
        .expect("create outage target");
    let notice_id = notice_target.id;
    let outage_id = outage_target.id;
    let pool_for_cleanup = pool.clone();

    let body = async move {
        let now = Utc::now();
        let mk = |tid: Uuid, counts: bool| {
            sqlx::query(
                "INSERT INTO incidents (org_id, target_id, started_at, status_at_start, origin,                                         visibility, severity, counts_as_downtime)                  VALUES ($1, $2, $3, 'down', 'manual', 'public', 'critical', $4)",
            )
            .bind(org_id.0)
            .bind(tid)
            .bind(now - chrono::Duration::minutes(5))
            .bind(counts)
        };
        mk(notice_id, false)
            .execute(&pool)
            .await
            .expect("insert published declaration that does not count");
        mk(outage_id, true)
            .execute(&pool)
            .await
            .expect("insert published declaration that counts");

        let page_id = seed_page_with_target(&pool, org_id, notice_id).await;
        PgStatusPageStore::new(pool.clone())
            .add_component(
                org_id,
                page_id,
                NewStatusPageComponent {
                    target_id: outage_id,
                    public_name: None,
                    public_description: None,
                    public_group: None,
                    sort_order: 1,
                    detail_link_enabled: false,
                },
                i64::MAX,
                None,
            )
            .await
            .expect("add outage component");

        let agg = OrgAggregator::new(pool, ch, AggregatorConfig::default(), None);
        let (page, _markers, _names, _hidden) =
            agg.build(page_id, org_id).await.expect("aggregator build");

        let component = |id: Uuid| {
            page.groups
                .iter()
                .flat_map(|g| &g.components)
                .find(|c| c.id == id)
                .expect("component present")
        };
        let notice = component(notice_id);
        let outage = component(outage_id);

        assert!(
            matches!(
                notice.current_status,
                PublicComponentStatus::Operational | PublicComponentStatus::NoData
            ),
            "a notice does not flip the dot"
        );
        assert!(
            matches!(
                notice.history.last().copied(),
                Some(DayState::Operational | DayState::NoData)
            ),
            "a notice paints no day"
        );
        assert_eq!(
            outage.current_status,
            PublicComponentStatus::MajorOutage,
            "counting means the service was down"
        );
        assert_eq!(
            outage.history.last().copied(),
            Some(DayState::MajorOutage),
            "counting paints the day"
        );
        assert_eq!(
            page.active_incidents.len(),
            2,
            "both are published, so both are still told to customers"
        );
    };

    let result = AssertUnwindSafe(body).catch_unwind().await;
    delete_target(&pool_for_cleanup, notice_id).await;
    delete_target(&pool_for_cleanup, outage_id).await;
    if let Err(p) = result {
        std::panic::resume_unwind(p);
    }
}

/// End-to-end: a PUBLISHED postmortem surfaces on the public incident detail via
/// `OrgPublicSource::incident_by_id`; a DRAFT one (published_at NULL) does not.
/// Guards the publish → public-page wiring.
#[tokio::test]
#[ignore = "requires DATABASE_URL + CLICKHOUSE_URL"]
async fn published_postmortem_surfaces_on_public_incident() {
    use std::sync::Arc;
    use uptimepage::domain::PageRef;
    use uptimepage::public_status::{OrgPublicSource, PageCache, PublicSource};

    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let Some(ch) = common::ch_client_from_env().await else {
        return;
    };
    purge_prefix(&pool, "agg-pm-").await;

    let org_id = seed_org(&pool, "agg-pm").await;
    let unique = format!("agg-pm-{}", Uuid::now_v7());
    let store = Arc::new(PostgresTargetStore::from_pool(pool.clone(), None));
    let target = store
        .create(
            org_id,
            public_target(&unique),
            WriteSource::Ui,
            i64::MAX,
            i64::MAX,
        )
        .await
        .expect("create public target");
    let target_id = target.id;
    let pool_for_cleanup = pool.clone();

    with_cleanup(&pool_for_cleanup, target_id, async move {
        let incident_id: Uuid = sqlx::query_scalar(
            "INSERT INTO incidents (org_id, target_id, started_at, status_at_start, visibility) \
             VALUES ($1, $2, now() - interval '1 hour', 'down', 'public') RETURNING id",
        )
        .bind(org_id.0)
        .bind(target_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        let page_id = seed_page_with_target(&pool, org_id, target_id).await;
        let cfg = uptimepage::config::PublicStatusConfig::default();
        let agg = Arc::new(OrgAggregator::new(pool.clone(), ch, AggregatorConfig::default(), None));
        let source = OrgPublicSource::new(agg, PageCache::new(&cfg), pool.clone(), "uptimepage");
        let page_ref = PageRef { page: page_id, org: org_id };

        // A draft postmortem (published_at NULL) must not surface.
        sqlx::query(
            "INSERT INTO incident_postmortems (org_id, incident_id, summary) VALUES ($1, $2, 'draft secret')",
        )
        .bind(org_id.0)
        .bind(incident_id)
        .execute(&pool)
        .await
        .unwrap();
        let inc = source.incident_by_id(page_ref, incident_id).await.expect("incident");
        assert!(inc.postmortem.is_none(), "a draft postmortem must stay private");

        // Publish it.
        sqlx::query("UPDATE incident_postmortems SET published_at = now() WHERE incident_id = $1")
            .bind(incident_id)
            .execute(&pool)
            .await
            .unwrap();
        let inc = source.incident_by_id(page_ref, incident_id).await.expect("incident");
        let pm = inc.postmortem.expect("published postmortem surfaces");
        assert_eq!(pm.summary.as_deref(), Some("draft secret"));
    })
    .await;
}
