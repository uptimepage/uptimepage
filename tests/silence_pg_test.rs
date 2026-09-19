//! Postgres-backed validation of silence detection: the `unmonitored` query
//! (a monitor is silent when every region the plan still serves it from lacks a
//! fresh enabled agent) and the open/resolve state transitions.
//!
//! `#[ignore]`d by default; runs under `--run-ignored all` once `DATABASE_URL`
//! is set. Each test provisions its own throwaway database so concurrent runs
//! can't contend on shared rows.

mod common;

use common::{make_user, unique_slug};
use sqlx::PgPool;
use uptimepage::config::AppConfig;
use uptimepage::domain::OrgId;
use uptimepage::domain::quota::RegionCaps;
use uptimepage::quotas::QuotaService;
use uptimepage::quotas::effective::resolve_plans;
use uptimepage::storage::{PgSilenceStore, SilenceStore};
use uuid::Uuid;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/postgres");

const STALE_AFTER: u64 = 120;

/// Create org + target. Returns (org, target_id).
async fn seed_target(pool: &PgPool, prefix: &str) -> (OrgId, Uuid) {
    let user = make_user(pool, prefix).await;
    let org = uptimepage::storage::create_org_with_owner(pool, user, &unique_slug(prefix), "svc")
        .await
        .expect("create org")
        .expect("org created");
    let target_id: Uuid = sqlx::query_scalar(
        "INSERT INTO targets (org_id, name, check_spec, interval_secs) \
         VALUES ($1, 'svc', '{}'::jsonb, 30) RETURNING id",
    )
    .bind(org.id.0)
    .fetch_one(pool)
    .await
    .expect("insert target");
    (org.id, target_id)
}

/// A region with one agent whose `last_seen_at` is `seen_secs_ago` old. Assigns
/// the target to it.
async fn region_with_agent(pool: &PgPool, target_id: Uuid, seen_secs_ago: i64) {
    region_with_agent_ranked(pool, target_id, seen_secs_ago, true).await;
}

/// As [`region_with_agent`], with the flag that decides where the region sorts
/// when a plan's `max_regions` has to pick: default-on regions rank first.
async fn region_with_agent_ranked(
    pool: &PgPool,
    target_id: Uuid,
    seen_secs_ago: i64,
    default_selected: bool,
) {
    let region = format!("r{}", Uuid::new_v4().simple());
    sqlx::query("INSERT INTO regions (id, name, default_selected) VALUES ($1, $1, $2)")
        .bind(&region)
        .bind(default_selected)
        .execute(pool)
        .await
        .expect("insert region");
    sqlx::query(
        "INSERT INTO agents (region, name, token_hash, token_prefix, last_seen_at) \
         VALUES ($1, 'a', 'h', 'p', now() - ($2::bigint * interval '1 second'))",
    )
    .bind(&region)
    .bind(seen_secs_ago)
    .execute(pool)
    .await
    .expect("insert agent");
    sqlx::query("INSERT INTO target_regions (target_id, region) VALUES ($1, $2)")
        .bind(target_id)
        .bind(&region)
        .execute(pool)
        .await
        .expect("assign region");
}

fn has(set: &[(OrgId, Uuid)], target_id: Uuid) -> bool {
    set.iter().any(|(_, t)| *t == target_id)
}

/// The ceilings the agent pull is applying, resolved the way the sweep does.
async fn live_caps(pool: &PgPool) -> RegionCaps {
    let cfg = AppConfig::load().expect("config");
    let quotas = QuotaService::new(&cfg, Some(pool.clone()));
    let orgs = PgSilenceStore::new(pool.clone())
        .orgs_with_assigned_targets()
        .await
        .expect("orgs");
    RegionCaps::from(&resolve_plans(&quotas, orgs).await)
}

#[tokio::test]
#[ignore]
async fn unmonitored_reflects_agent_liveness_pg() {
    let Some((db, name)) = common::fresh_test_db("silence").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let store = PgSilenceStore::new(pool.clone());

    // Fresh agent in the only region → not unmonitored.
    let (_org, fresh) = seed_target(&pool, "silfresh").await;
    region_with_agent(&pool, fresh, 10).await;
    assert!(
        !has(
            &store
                .unmonitored(STALE_AFTER, &RegionCaps::default())
                .await
                .unwrap(),
            fresh
        ),
        "a target whose region has a fresh agent is monitored"
    );

    // Stale agent in the only region → unmonitored.
    let (_org, dark) = seed_target(&pool, "sildark").await;
    region_with_agent(&pool, dark, 10_000).await;
    assert!(
        has(
            &store
                .unmonitored(STALE_AFTER, &RegionCaps::default())
                .await
                .unwrap(),
            dark
        ),
        "a target whose only region's agent is stale is unmonitored"
    );

    // Two regions, one fresh → not unmonitored (one live probe is enough).
    let (_org, multi) = seed_target(&pool, "silmulti").await;
    region_with_agent(&pool, multi, 10_000).await; // stale
    region_with_agent(&pool, multi, 10).await; // fresh
    assert!(
        !has(
            &store
                .unmonitored(STALE_AFTER, &RegionCaps::default())
                .await
                .unwrap(),
            multi
        ),
        "one live region keeps a multi-region target monitored"
    );

    // No region assignment at all → never flagged (not unmonitored, by design).
    let (_org, unassigned) = seed_target(&pool, "silunassigned").await;
    assert!(
        !has(
            &store
                .unmonitored(STALE_AFTER, &RegionCaps::default())
                .await
                .unwrap(),
            unassigned
        ),
        "an unassigned target is not flagged silent"
    );

    // Stale agent but inside an active maintenance window → suppressed.
    let (mw_org, maint) = seed_target(&pool, "silmaint").await;
    region_with_agent(&pool, maint, 10_000).await; // stale
    let mw_id: Uuid = sqlx::query_scalar(
        "INSERT INTO maintenance_windows (org_id, title, starts_at, ends_at) \
         VALUES ($1, 'mw', now() - interval '1 hour', now() + interval '1 hour') RETURNING id",
    )
    .bind(mw_org.0)
    .fetch_one(&pool)
    .await
    .expect("insert maintenance window");
    sqlx::query(
        "INSERT INTO maintenance_window_components (org_id, maintenance_id, target_id) \
         VALUES ($1, $2, $3)",
    )
    .bind(mw_org.0)
    .bind(mw_id)
    .bind(maint)
    .execute(&pool)
    .await
    .expect("assign maintenance component");
    assert!(
        !has(
            &store
                .unmonitored(STALE_AFTER, &RegionCaps::default())
                .await
                .unwrap(),
            maint
        ),
        "a target in an active maintenance window is not flagged silent"
    );

    sqlx::query("UPDATE maintenance_windows SET suppress_alerts = false WHERE id = $1")
        .bind(mw_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        has(
            &store
                .unmonitored(STALE_AFTER, &RegionCaps::default())
                .await
                .unwrap(),
            maint
        ),
        "an announcement-only window leaves no-data alerting live"
    );

    drop(pool);
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore]
async fn a_region_past_the_plan_cap_does_not_count_as_coverage() {
    let Some((db, name)) = common::fresh_test_db("silencecap").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let store = PgSilenceStore::new(pool.clone());

    let (_org, target) = seed_target(&pool, "silcap").await;
    // The default-on region ranks first and is dark; the live one is the region
    // a cap of one drops, so nothing is probing this monitor.
    region_with_agent_ranked(&pool, target, 10_000, true).await;
    region_with_agent_ranked(&pool, target, 10, false).await;

    assert!(
        !has(
            &store
                .unmonitored(STALE_AFTER, &RegionCaps::default())
                .await
                .unwrap(),
            target
        ),
        "with no ceiling the live second region still covers the monitor"
    );

    // Throwaway database: retuning the catalog here races nothing.
    sqlx::query("UPDATE plans SET max_regions = 1")
        .execute(&pool)
        .await
        .expect("cap every plan");
    assert!(
        has(
            &store
                .unmonitored(STALE_AFTER, &live_caps(&pool).await)
                .await
                .unwrap(),
            target
        ),
        "a live agent in a region the plan no longer serves is not coverage"
    );

    drop(pool);
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore]
async fn enter_resolve_round_trip_pg() {
    let Some((db, name)) = common::fresh_test_db("silence").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let store = PgSilenceStore::new(pool.clone());
    let (org, target_id) = seed_target(&pool, "silstate").await;
    let now = chrono::Utc::now();

    let open_has = |v: &[uptimepage::storage::silence::OpenSilence], t: Uuid| {
        v.iter().any(|s| s.target_id == t)
    };

    store.enter(org, target_id, now).await.unwrap();
    assert!(
        open_has(&store.list_open().await.unwrap(), target_id),
        "entered silence is open"
    );

    // Idempotent re-enter keeps it open exactly once.
    store.enter(org, target_id, now).await.unwrap();
    let open = store.list_open().await.unwrap();
    assert_eq!(open.iter().filter(|s| s.target_id == target_id).count(), 1);

    // mark_notified flips the flag; a re-enter while open keeps it notified.
    store.mark_notified(org, target_id, now).await.unwrap();
    assert!(
        store
            .list_open()
            .await
            .unwrap()
            .iter()
            .any(|s| s.target_id == target_id && s.notified),
        "notified flag set"
    );

    store.resolve(org, target_id, now).await.unwrap();
    assert!(
        !open_has(&store.list_open().await.unwrap(), target_id),
        "resolved silence is no longer open"
    );

    // Re-entering after resolve reopens a fresh episode with notified cleared.
    store.enter(org, target_id, now).await.unwrap();
    assert!(
        store
            .list_open()
            .await
            .unwrap()
            .iter()
            .any(|s| s.target_id == target_id && !s.notified),
        "fresh episode is unnotified"
    );

    drop(pool);
    common::drop_test_db(&name).await;
}
