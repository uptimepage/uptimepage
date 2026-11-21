//! Postgres-backed validation of silence detection: the `unmonitored` query
//! (a monitor is silent when every region the plan still serves it from lacks
//! an agent holding a config), the grace a just-restarted agent gets, and the
//! open/resolve state transitions.
//!
//! `#[ignore]`d by default; runs under `--run-ignored all` once `DATABASE_URL`
//! is set. Each test provisions its own throwaway database so concurrent runs
//! can't contend on shared rows.

use crate::common;

use std::time::Duration;

use async_trait::async_trait;
use common::{make_user, unique_slug};
use parking_lot::Mutex;
use sqlx::PgPool;
use uptimepage::config::AppConfig;
use uptimepage::domain::agent_wire::HeldConfig;
use uptimepage::domain::quota::RegionCaps;
use uptimepage::domain::{NotificationReason, OrgId};
use uptimepage::jobs::silence::{SilenceDelivery, sweep};
use uptimepage::quotas::QuotaService;
use uptimepage::quotas::effective::resolve_plans;
use uptimepage::storage::operator::OperatorRepo;
use uptimepage::storage::{GroupSilence, PgSilenceStore, SilenceGroup, SilenceStore, Unmonitored};
use uuid::Uuid;

use crate::common::MIGRATOR;

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
    let region = assigned_region(pool, target_id, default_selected).await;
    add_agent(pool, &region, Some(seen_secs_ago), None).await;
}

/// A new region the target is assigned to.
async fn assigned_region(pool: &PgPool, target_id: Uuid, default_selected: bool) -> String {
    let region = format!("r{}", Uuid::new_v4().simple());
    sqlx::query("INSERT INTO regions (id, name, default_selected) VALUES ($1, $1, $2)")
        .bind(&region)
        .bind(default_selected)
        .execute(pool)
        .await
        .expect("insert region");
    sqlx::query("INSERT INTO target_regions (target_id, region) VALUES ($1, $2)")
        .bind(target_id)
        .bind(&region)
        .execute(pool)
        .await
        .expect("assign region");
    region
}

/// An agent last seen `seen_secs_ago` (never, for `None`), first seen without
/// a config `missing_secs_ago` (holding one, for `None`).
async fn add_agent(
    pool: &PgPool,
    region: &str,
    seen_secs_ago: Option<i64>,
    missing_secs_ago: Option<i64>,
) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO agents (region, name, token_hash, token_prefix, last_seen_at, config_missing_since) \
         VALUES ($1, 'a', 'h', 'p', now() - ($2::bigint * interval '1 second'), \
                 now() - ($3::bigint * interval '1 second')) RETURNING id",
    )
    .bind(region)
    .bind(seen_secs_ago)
    .bind(missing_secs_ago)
    .fetch_one(pool)
    .await
    .expect("insert agent")
}

fn has(set: &[Unmonitored], target_id: Uuid) -> bool {
    set.iter().any(|r| r.target_id == target_id)
}

/// Whether the target is unmonitored and, if so, whether it is in grace.
fn grace(set: &[Unmonitored], target_id: Uuid) -> Option<bool> {
    set.iter()
        .find(|r| r.target_id == target_id)
        .map(|r| r.in_grace)
}

async fn open_ids(store: &PgSilenceStore) -> Vec<Uuid> {
    let mut ids: Vec<Uuid> = store
        .list_open()
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.target_id)
        .collect();
    ids.sort();
    ids
}

/// The row version, which any write to the silence row moves.
async fn silence_xmin(pool: &PgPool, target_id: Uuid) -> String {
    sqlx::query_scalar("SELECT xmin::text FROM monitor_silence_state WHERE target_id = $1")
        .bind(target_id)
        .fetch_one(pool)
        .await
        .expect("silence row")
}

async fn tick(store: &PgSilenceStore, delivery: Option<&dyn SilenceDelivery>) {
    sweep(
        store,
        delivery,
        Duration::from_secs(STALE_AFTER),
        &RegionCaps::default(),
    )
    .await
    .expect("sweep");
}

#[derive(Default)]
struct RecordingDelivery {
    sent: Mutex<Vec<(Uuid, NotificationReason)>>,
}

impl RecordingDelivery {
    fn count(&self, reason: NotificationReason) -> usize {
        self.sent
            .lock()
            .iter()
            .filter(|(_, r)| *r == reason)
            .count()
    }
}

#[async_trait]
impl SilenceDelivery for RecordingDelivery {
    async fn notify(&self, _org: OrgId, target_id: Uuid, reason: NotificationReason) -> bool {
        self.sent.lock().push((target_id, reason));
        true
    }
    async fn group_channels(&self, _org: OrgId, _group: &SilenceGroup) -> Vec<Uuid> {
        Vec::new()
    }
    async fn notify_group(&self, _silence: &GroupSilence, _reason: NotificationReason) -> bool {
        true
    }
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

#[tokio::test]
#[ignore]
async fn an_agent_without_a_config_covers_a_monitor_only_inside_its_grace_pg() {
    let Some((db, name)) = common::fresh_test_db("silgrace").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let store = PgSilenceStore::new(pool.clone());

    let (_org, past) = seed_target(&pool, "silpast").await;
    let region = assigned_region(&pool, past, true).await;
    add_agent(&pool, &region, Some(5), Some(10_000)).await;
    let (_org, under) = seed_target(&pool, "silunder").await;
    let region = assigned_region(&pool, under, true).await;
    add_agent(&pool, &region, Some(5), Some(10)).await;
    let (_org, held) = seed_target(&pool, "silheld").await;
    let region = assigned_region(&pool, held, true).await;
    add_agent(&pool, &region, Some(5), None).await;

    let set = store
        .unmonitored(STALE_AFTER, &RegionCaps::default())
        .await
        .unwrap();
    assert_eq!(
        grace(&set, past),
        Some(false),
        "missing past the window is no coverage"
    );
    assert_eq!(
        grace(&set, under),
        Some(true),
        "missing under the window is grace"
    );
    assert_eq!(grace(&set, held), None, "holding a config is coverage");

    tick(&store, None).await;
    assert_eq!(open_ids(&store).await, vec![past], "grace opens no silence");

    drop(pool);
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore]
async fn a_returning_agent_holds_the_silence_open_until_a_pull_shows_a_config_pg() {
    let Some((db, name)) = common::fresh_test_db("silreturn").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let store = PgSilenceStore::new(pool.clone());
    let repo = OperatorRepo::new(pool.clone());

    let (_org, target) = seed_target(&pool, "silreturn").await;
    let region = assigned_region(&pool, target, true).await;
    let agent = add_agent(&pool, &region, Some(10_000), None).await;
    tick(&store, None).await;
    assert_eq!(open_ids(&store).await, vec![target], "region offline");
    let before = silence_xmin(&pool, target).await;

    // A dispatch claim lands before the first pull: its heartbeat is the only
    // write it makes to the agent row.
    repo.touch_agent_seen(agent, STALE_AFTER).await.unwrap();
    let set = store
        .unmonitored(STALE_AFTER, &RegionCaps::default())
        .await
        .unwrap();
    assert_eq!(grace(&set, target), Some(true), "back, and in grace");
    tick(&store, None).await;
    tick(&store, None).await;
    assert_eq!(open_ids(&store).await, vec![target], "grace closes nothing");
    assert_eq!(
        silence_xmin(&pool, target).await,
        before,
        "holding it open writes nothing"
    );

    repo.record_config_evidence(agent, HeldConfig::Loaded(Some(1)), STALE_AFTER)
        .await
        .unwrap();
    tick(&store, None).await;
    assert!(open_ids(&store).await.is_empty(), "a config closes it");

    drop(pool);
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore]
async fn two_sweeps_tell_one_episode_once_each_way_pg() {
    let Some((db, name)) = common::fresh_test_db("silonce").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let (blue, green) = (
        PgSilenceStore::new(pool.clone()),
        PgSilenceStore::new(pool.clone()),
    );
    let repo = OperatorRepo::new(pool.clone());
    let told = RecordingDelivery::default();
    let both = || async {
        tick(&blue, Some(&told)).await;
        tick(&green, Some(&told)).await;
    };

    let (org, target) = seed_target(&pool, "silonce").await;
    // Healthy neighbours keep one silent monitor under the mass threshold.
    for _ in 0..3 {
        sqlx::query(
            "INSERT INTO targets (org_id, name, check_spec, interval_secs) \
             VALUES ($1, 'other', '{}'::jsonb, 30)",
        )
        .bind(org.0)
        .execute(&pool)
        .await
        .unwrap();
    }
    let region = assigned_region(&pool, target, true).await;
    let agent = add_agent(&pool, &region, Some(10_000), None).await;

    both().await;
    assert_eq!(told.count(NotificationReason::NoData), 1, "stale agent");

    // Cold reconnect, then loads that keep failing, inside the grace and past it.
    repo.touch_agent_seen(agent, STALE_AFTER).await.unwrap();
    for _ in 0..3 {
        repo.record_config_evidence(agent, HeldConfig::Nothing, STALE_AFTER)
            .await
            .unwrap();
        both().await;
    }
    sqlx::query("UPDATE agents SET config_missing_since = now() - interval '1 hour' WHERE id = $1")
        .bind(agent)
        .execute(&pool)
        .await
        .unwrap();
    repo.record_config_evidence(agent, HeldConfig::Nothing, STALE_AFTER)
        .await
        .unwrap();
    both().await;
    assert_eq!(open_ids(&blue).await, vec![target]);
    assert_eq!(
        told.count(NotificationReason::NoData),
        1,
        "no second notice"
    );
    assert_eq!(
        told.count(NotificationReason::DataResumed),
        0,
        "no false resume"
    );

    repo.record_config_evidence(agent, HeldConfig::Loaded(Some(1)), STALE_AFTER)
        .await
        .unwrap();
    both().await;
    both().await;
    assert!(open_ids(&blue).await.is_empty());
    assert_eq!(told.count(NotificationReason::NoData), 1);
    assert_eq!(told.count(NotificationReason::DataResumed), 1, "one resume");

    drop(pool);
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore]
async fn a_never_seen_agent_is_probing_for_the_operator_but_no_coverage_for_customers_pg() {
    let Some((db, name)) = common::fresh_test_db("silnever").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let store = PgSilenceStore::new(pool.clone());
    let repo = OperatorRepo::new(pool.clone());

    let (org, target) = seed_target(&pool, "silnever").await;
    let region = assigned_region(&pool, target, true).await;
    let agent = add_agent(&pool, &region, None, None).await;

    let row = repo
        .list_agents()
        .await
        .unwrap()
        .into_iter()
        .find(|a| a.id == agent)
        .expect("agent row");
    assert!(
        row.is_probing(chrono::Utc::now(), Duration::from_secs(STALE_AFTER)),
        "the operator view gives a new agent its creation grace"
    );
    let set = store
        .unmonitored(STALE_AFTER, &RegionCaps::default())
        .await
        .unwrap();
    assert_eq!(
        grace(&set, target),
        Some(false),
        "a customer gets no grace from an agent that never probed"
    );
    assert!(
        store
            .live_regions(org, target, STALE_AFTER)
            .await
            .unwrap()
            .is_empty()
    );

    drop(pool);
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore]
async fn a_region_row_stays_dark_while_the_silence_is_open_pg() {
    let Some((db, name)) = common::fresh_test_db("silrows").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let store = PgSilenceStore::new(pool.clone());

    let (org, target) = seed_target(&pool, "silrows").await;
    let restarting = assigned_region(&pool, target, true).await;
    add_agent(&pool, &restarting, Some(5), Some(10)).await;
    let steady = assigned_region(&pool, target, false).await;
    add_agent(&pool, &steady, Some(5), None).await;
    let live = || async {
        let mut v = store.live_regions(org, target, STALE_AFTER).await.unwrap();
        v.sort();
        v
    };
    let mut both = vec![restarting.clone(), steady.clone()];
    both.sort();

    assert_eq!(live().await, both, "a restart does not grey a row");
    let now = chrono::Utc::now();
    store.enter(org, target, now).await.unwrap();
    assert_eq!(
        live().await,
        vec![steady.clone()],
        "with a silence open, only a region holding a config is live"
    );
    store.resolve(org, target, now).await.unwrap();
    assert_eq!(live().await, both);

    drop(pool);
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore]
async fn with_every_agent_holding_a_config_no_monitor_is_in_grace_pg() {
    let Some((db, name)) = common::fresh_test_db("silsame").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let store = PgSilenceStore::new(pool.clone());

    let (org, fresh) = seed_target(&pool, "silsfresh").await;
    let fresh_region = assigned_region(&pool, fresh, true).await;
    add_agent(&pool, &fresh_region, Some(5), None).await;
    let (_org, stale) = seed_target(&pool, "silsstale").await;
    let region = assigned_region(&pool, stale, true).await;
    add_agent(&pool, &region, Some(10_000), None).await;
    let (_org, never) = seed_target(&pool, "silsnever").await;
    let region = assigned_region(&pool, never, true).await;
    add_agent(&pool, &region, None, None).await;
    let (_org, disabled) = seed_target(&pool, "silsoff").await;
    let region = assigned_region(&pool, disabled, true).await;
    let off = add_agent(&pool, &region, Some(5), None).await;
    sqlx::query("UPDATE agents SET enabled = false WHERE id = $1")
        .bind(off)
        .execute(&pool)
        .await
        .unwrap();

    let set = store
        .unmonitored(STALE_AFTER, &RegionCaps::default())
        .await
        .unwrap();
    let mut silent: Vec<Uuid> = set.iter().map(|r| r.target_id).collect();
    silent.sort();
    let mut expected = vec![stale, never, disabled];
    expected.sort();
    assert_eq!(silent, expected, "the set the liveness rule gave");
    assert!(set.iter().all(|r| !r.in_grace));

    assert_eq!(
        store.live_regions(org, fresh, STALE_AFTER).await.unwrap(),
        vec![fresh_region.clone()]
    );
    store.enter(org, fresh, chrono::Utc::now()).await.unwrap();
    assert_eq!(
        store.live_regions(org, fresh, STALE_AFTER).await.unwrap(),
        vec![fresh_region],
        "an open silence greys no row of an agent holding a config"
    );

    drop(pool);
    common::drop_test_db(&name).await;
}
