//! Postgres-backed regression on what reaches the uptime figure.
//! `confirmed_downtime_by_target` must skip target-less (manual) incidents: a
//! NULL target would otherwise fail decoding into `Uuid`. It must also skip a
//! declared incident bound to a monitor until asked to count it, and weigh each
//! incident by the impact its row records.
//!
//! `#[ignore]`d by default; runs under `--run-ignored all` with `DATABASE_URL`
//! set. The harness auto-applies migrations on first connect.

use crate::common;

use common::{make_user, unique_slug};
use sqlx::PgPool;
use uptimepage::domain::{
    CheckSpec, CheckStatus, ExpectedStatus, NewManualIncident, OrgId, Recovered, TargetUpdate,
    UserId, WriteSource, confirmed_downtime_secs,
};
use uptimepage::public_status::{IncidentStore, NewOpenIncident, PgIncidentStore};
use uptimepage::storage::{
    Actor, IncidentNarrationStore, IncidentOpsStore, PgIncidentNarrationStore, PgIncidentOpsStore,
    PostgresTargetStore, TargetStore, TimeRange, create_org_with_owner,
};
use url::Url;
use uuid::Uuid;

async fn seed(pool: &PgPool, prefix: &str) -> (OrgId, UserId, Uuid) {
    let user = make_user(pool, prefix).await;
    let org = create_org_with_owner(pool, user, &unique_slug(prefix), "svc")
        .await
        .expect("create org")
        .expect("org created");
    let check = CheckSpec::Http(common::default_http_check(
        Url::parse("https://example.test/healthz").unwrap(),
        ExpectedStatus::Exact(200),
    ));
    let target_id: Uuid = sqlx::query_scalar(
        "INSERT INTO targets (org_id, name, check_spec, interval_secs) \
         VALUES ($1, 'svc', $2, 30) RETURNING id",
    )
    .bind(org.id.0)
    .bind(serde_json::to_value(check).unwrap())
    .fetch_one(pool)
    .await
    .expect("insert target");
    (org.id, user, target_id)
}

#[tokio::test]
#[ignore]
async fn confirmed_downtime_skips_null_target_incidents_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, _user, target_id) = seed(&pool, "cdt").await;

    // Resolved incident on the target: 10 minutes inside the window.
    sqlx::query(
        "INSERT INTO incidents (org_id, target_id, started_at, ended_at, status_at_start, \
                                check_count, state, visibility, origin) \
         VALUES ($1, $2, now() - interval '50 minute', now() - interval '40 minute', \
                 'down', 1, 'resolved', 'public', 'monitor')",
    )
    .bind(org.0)
    .bind(target_id)
    .execute(&pool)
    .await
    .expect("insert target incident");

    // Target-less manual incident in the same window — must be ignored.
    sqlx::query(
        "INSERT INTO incidents (org_id, target_id, started_at, status_at_start, \
                                check_count, state, visibility, origin) \
         VALUES ($1, NULL, now() - interval '30 minute', 'down', 1, 'triggered', \
                 'internal', 'manual')",
    )
    .bind(org.0)
    .execute(&pool)
    .await
    .expect("insert manual incident");

    let store = PgIncidentNarrationStore::new(pool.clone());
    let now = chrono::Utc::now();
    let range = TimeRange {
        from: now - chrono::Duration::hours(1),
        to: now,
    };
    let map = store
        .confirmed_downtime_by_target(org, range, None)
        .await
        .expect("must not error on a null-target incident");

    assert_eq!(map.len(), 1, "only the real target appears");
    assert_eq!(
        map.get(&target_id).copied(),
        Some(600),
        "10 minutes of clamped downtime"
    );
}

#[tokio::test]
#[ignore]
async fn uptime_weighs_each_incident_by_its_impact_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, _user, target_id) = seed(&pool, "cdtw").await;
    for (start, end, status, regions_up) in [
        (50, 40, "down", vec!["us-east"]),
        (30, 20, "degraded", vec![]),
    ] {
        sqlx::query(
            "INSERT INTO incidents (org_id, target_id, started_at, ended_at, status_at_start, \
                                    check_count, state, visibility, origin, regions_down, \
                                    regions_up) \
             VALUES ($1, $2, now() - make_interval(mins => $3), \
                     now() - make_interval(mins => $4), $5, 1, 'resolved', 'public', \
                     'monitor', ARRAY['eu-west'], $6)",
        )
        .bind(org.0)
        .bind(target_id)
        .bind(start)
        .bind(end)
        .bind(status)
        .bind(regions_up)
        .execute(&pool)
        .await
        .expect("insert incident");
    }

    let now = chrono::Utc::now();
    let range = TimeRange {
        from: now - chrono::Duration::hours(1),
        to: now,
    };
    let map = PgIncidentNarrationStore::new(pool.clone())
        .confirmed_downtime_by_target(org, range, None)
        .await
        .expect("downtime rollup");
    assert_eq!(
        map.get(&target_id).copied(),
        Some(180),
        "ten minutes of partial outage count as three; degraded counts nothing"
    );
}

/// An incident waiting out its recovery is back up, so its downtime stops
/// where the recovery began.
#[tokio::test]
#[ignore]
async fn a_recovering_incident_stops_counting_where_the_recovery_began_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, _user, target_id) = seed(&pool, "cdtrec").await;
    sqlx::query(
        "INSERT INTO incidents (org_id, target_id, started_at, recovering_since, \
                                status_at_start, check_count, state, visibility, origin) \
         VALUES ($1, $2, now() - interval '50 minute', now() - interval '40 minute', \
                 'down', 1, 'triggered', 'public', 'monitor')",
    )
    .bind(org.0)
    .bind(target_id)
    .execute(&pool)
    .await
    .expect("insert recovering incident");

    let now = chrono::Utc::now();
    let range = TimeRange {
        from: now - chrono::Duration::hours(1),
        to: now,
    };
    let map = PgIncidentNarrationStore::new(pool.clone())
        .confirmed_downtime_by_target(org, range, None)
        .await
        .expect("downtime rollup");
    assert_eq!(map.get(&target_id).copied(), Some(600));
}

/// A failure inside the hold ends the recovery; the stretch it was back up in
/// is kept on the row and stays out of the downtime, once however many
/// writers record it.
#[tokio::test]
#[ignore]
async fn a_stretch_back_up_inside_the_hold_is_not_downtime_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, _user, target_id) = seed(&pool, "cdtrelapse").await;
    let writer = PgIncidentStore::new(pool.clone());
    let now =
        chrono::DurationRound::duration_trunc(chrono::Utc::now(), chrono::Duration::seconds(1))
            .unwrap();
    let at = |mins: i64| now - chrono::Duration::minutes(mins);
    let id = writer
        .insert_open(
            org,
            NewOpenIncident {
                target_id,
                started_at: at(10),
                status_at_start: CheckStatus::Down,
                check_count: 2,
                error_sample: None,
                region: None,
                regions_down: vec![],
                regions_up: vec![],
            },
        )
        .await
        .expect("insert")
        .expect("opened");
    writer
        .set_recovering(org, id, Some(at(8)))
        .await
        .expect("recovering");
    let stretch = Recovered {
        from: at(8),
        until: at(6),
    };
    writer.relapse(org, id, stretch).await.expect("relapse");
    writer
        .relapse(org, id, stretch)
        .await
        .expect("relapse again");
    // One the writer only saw in the checks afterwards, with no mark set.
    let unmarked = Recovered {
        from: at(5),
        until: at(4),
    };
    writer.relapse(org, id, unmarked).await.expect("unmarked");
    assert!(writer.close(org, id, at(2)).await.expect("close").is_some());

    let narration = PgIncidentNarrationStore::new(pool.clone());
    let incidents = narration
        .list_for_target(
            org,
            target_id,
            TimeRange {
                from: at(60),
                to: at(0),
            },
            10,
            0,
            false,
        )
        .await
        .expect("list");
    assert_eq!(
        incidents[0].recovered,
        [stretch, unmarked],
        "each kept once"
    );
    assert_eq!(incidents[0].recovering_since, None);
    let map = narration
        .confirmed_downtime_by_target(
            org,
            TimeRange {
                from: at(60),
                to: at(0),
            },
            None,
        )
        .await
        .expect("downtime rollup");
    assert_eq!(
        map.get(&target_id).copied(),
        Some(5 * 60),
        "8 minutes less the 3 back up"
    );
}

/// Declaring is communication, not measurement, so it stays out until asked in.
#[tokio::test]
#[ignore]
async fn a_declared_incident_stays_out_of_uptime_until_asked_in_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user, target_id) = seed(&pool, "cdtdecl").await;
    let ops = PgIncidentOpsStore::new(pool.clone());
    let narration = PgIncidentNarrationStore::new(pool.clone());

    let declared = ops
        .declare(
            org,
            NewManualIncident {
                title: "payments failing, site up".into(),
                target_id: Some(target_id),
                ..Default::default()
            },
            Actor::User(user),
        )
        .await
        .expect("declare");
    assert!(
        !declared.counts_as_downtime,
        "a declaration defaults to leaving uptime alone"
    );

    sqlx::query(
        "UPDATE incidents SET started_at = now() - interval '40 minute', \
                              ended_at = now() - interval '30 minute', \
                              state = 'resolved' \
         WHERE id = $1 AND org_id = $2",
    )
    .bind(declared.id)
    .bind(org.0)
    .execute(&pool)
    .await
    .expect("close declared incident");

    let now = chrono::Utc::now();
    let range = TimeRange {
        from: now - chrono::Duration::hours(1),
        to: now,
    };
    let map = narration
        .confirmed_downtime_by_target(org, range, None)
        .await
        .expect("downtime rollup");
    assert!(
        !map.contains_key(&target_id),
        "declared downtime must not reach the uptime figure: {map:?}"
    );

    IncidentNarrationStore::patch_narration(
        &narration,
        org,
        declared.id,
        uptimepage::domain::IncidentNarrationUpdate {
            counts_as_downtime: Some(true),
            ..Default::default()
        },
    )
    .await
    .expect("patch")
    .expect("incident exists");

    let map = narration
        .confirmed_downtime_by_target(org, range, None)
        .await
        .expect("downtime rollup");
    assert_eq!(
        map.get(&target_id).copied(),
        Some(180),
        "once counted, a declared major incident weighs as the partial outage the page shows"
    );

    IncidentNarrationStore::patch_narration(
        &narration,
        org,
        declared.id,
        uptimepage::domain::IncidentNarrationUpdate {
            severity: Some(uptimepage::domain::IncidentSeverity::Critical),
            ..Default::default()
        },
    )
    .await
    .expect("patch")
    .expect("incident exists");
    let map = narration
        .confirmed_downtime_by_target(org, range, None)
        .await
        .expect("downtime rollup");
    assert_eq!(
        map.get(&target_id).copied(),
        Some(600),
        "a critical declaration is a major outage, counted in full"
    );
}

/// Moving a monitor incident's accounting would contradict its own check rows.
#[tokio::test]
#[ignore]
async fn a_monitor_opened_incident_cannot_be_excluded_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, _user, target_id) = seed(&pool, "cdtmon").await;
    let incident_id: Uuid = sqlx::query_scalar(
        "INSERT INTO incidents (org_id, target_id, started_at, ended_at, status_at_start, \
                                check_count, state, visibility, origin) \
         VALUES ($1, $2, now() - interval '40 minute', now() - interval '30 minute', \
                 'down', 3, 'resolved', 'public', 'monitor') RETURNING id",
    )
    .bind(org.0)
    .bind(target_id)
    .fetch_one(&pool)
    .await
    .expect("insert monitor incident");

    let narration = PgIncidentNarrationStore::new(pool.clone());
    let patched = IncidentNarrationStore::patch_narration(
        &narration,
        org,
        incident_id,
        uptimepage::domain::IncidentNarrationUpdate {
            counts_as_downtime: Some(false),
            ..Default::default()
        },
    )
    .await
    .expect("patch")
    .expect("incident exists");
    assert!(
        patched.counts_as_downtime,
        "the SQL guard holds even if a caller skips the handler's 422"
    );

    let now = chrono::Utc::now();
    let map = narration
        .confirmed_downtime_by_target(
            org,
            TimeRange {
                from: now - chrono::Duration::hours(1),
                to: now,
            },
            None,
        )
        .await
        .expect("downtime rollup");
    assert_eq!(map.get(&target_id).copied(), Some(600));
}

/// The dashboard counts every incident but paints only what dents uptime.
#[tokio::test]
#[ignore]
async fn count_overlapping_counts_every_incident_spans_only_downtime_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, _user, target_id) = seed(&pool, "cdtspan").await;
    for sql in [
        "INSERT INTO incidents (org_id, target_id, started_at, ended_at, status_at_start, \
                                check_count, state, visibility, origin) \
         VALUES ($1, $2, now() - interval '40 minute', now() - interval '30 minute', \
                 'degraded', 3, 'resolved', 'public', 'monitor')",
        "INSERT INTO incidents (org_id, target_id, started_at, status_at_start, \
                                check_count, state, visibility, origin, counts_as_downtime) \
         VALUES ($1, $2, now() - interval '20 minute', 'down', 1, 'triggered', \
                 'internal', 'manual', false)",
        "INSERT INTO incidents (org_id, target_id, started_at, ended_at, status_at_start, \
                                check_count, state, visibility, origin) \
         VALUES ($1, $2, now() - interval '3 hour', now() - interval '2 hour', \
                 'down', 3, 'resolved', 'public', 'monitor')",
    ] {
        sqlx::query(sql)
            .bind(org.0)
            .bind(target_id)
            .execute(&pool)
            .await
            .expect("insert incident");
    }
    sqlx::query(
        "INSERT INTO incidents (org_id, target_id, started_at, status_at_start, \
                                check_count, state, visibility, origin) \
         VALUES ($1, NULL, now() - interval '10 minute', 'down', 1, 'triggered', \
                 'internal', 'manual')",
    )
    .bind(org.0)
    .execute(&pool)
    .await
    .expect("insert target-less incident");

    let store = PgIncidentNarrationStore::new(pool.clone());
    let now = chrono::Utc::now();
    let range = TimeRange {
        from: now - chrono::Duration::hours(1),
        to: now,
    };
    assert_eq!(
        store.count_overlapping(org, range).await.expect("count"),
        3,
        "monitor, declared and target-less incidents in the hour; not the older one"
    );
    let spans = store.downtime_spans(org, range, None).await.expect("spans");
    assert_eq!(
        spans.len(),
        1,
        "only the monitor incident counts as downtime"
    );
    assert_eq!(spans[0].target_id, target_id);
    assert_eq!(spans[0].origin, "monitor");
    assert_eq!(spans[0].status_at_start, "degraded");
    assert!(spans[0].ended_at.is_some());
}

/// A paused monitor's incident stays open, but nobody checked it while paused,
/// so the pause is not downtime, whether it is still running or was resumed.
#[tokio::test]
#[ignore]
async fn a_paused_stretch_is_not_downtime_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user, target_id) = seed(&pool, "cdtpause").await;
    let writer = PgIncidentStore::new(pool.clone());
    let targets = PostgresTargetStore::from_pool(pool.clone(), None);
    let narration = PgIncidentNarrationStore::new(pool.clone());
    let now =
        chrono::DurationRound::duration_trunc(chrono::Utc::now(), chrono::Duration::seconds(1))
            .unwrap();
    let at = |mins: i64| now - chrono::Duration::minutes(mins);
    let window = TimeRange {
        from: at(120),
        to: at(0),
    };
    let id = writer
        .insert_open(
            org,
            NewOpenIncident {
                target_id,
                started_at: at(60),
                status_at_start: CheckStatus::Down,
                check_count: 2,
                error_sample: None,
                region: None,
                regions_down: vec![],
                regions_up: vec![],
            },
        )
        .await
        .expect("insert")
        .expect("opened");
    let downtime = async || {
        let rollup = narration
            .confirmed_downtime_by_target(org, window, None)
            .await
            .expect("downtime rollup")
            .get(&target_id)
            .copied();
        let incidents = narration
            .list_for_target(org, target_id, window, 10, 0, false)
            .await
            .expect("list");
        assert!(incidents[0].ended_at.is_none(), "the incident stays open");
        let per_monitor = confirmed_downtime_secs(&incidents, window.from, window.to, window.to);
        assert_eq!(rollup, Some(per_monitor), "both readers agree");
        per_monitor
    };

    targets
        .set_enabled(org, &[target_id], false, Some(user))
        .await
        .expect("pause");
    sqlx::query(
        "UPDATE incidents SET paused_since = $2 WHERE id = $1 AND paused_since IS NOT NULL",
    )
    .bind(id)
    .bind(at(40))
    .execute(&pool)
    .await
    .expect("date the pause");
    assert_eq!(
        downtime().await,
        20 * 60,
        "down from 60 until the pause at 40"
    );

    let resume = TargetUpdate {
        enabled: Some(true),
        ..Default::default()
    };
    targets
        .update(
            org,
            target_id,
            resume,
            Some(WriteSource::Ui),
            Some(user),
            uptimepage::domain::WriteCaps::UNLIMITED,
        )
        .await
        .expect("resume")
        .expect("found");
    sqlx::query("UPDATE incidents SET paused_until = ARRAY[$2::timestamptz] WHERE id = $1 AND paused_from = ARRAY[$3::timestamptz]")
        .bind(id)
        .bind(at(10))
        .bind(at(40))
        .execute(&pool)
        .await
        .expect("date the resume");
    assert_eq!(
        downtime().await,
        30 * 60,
        "down again from the resume at 10, the 30 paused left out"
    );
}

/// Reopening an incident on a monitor paused since it closed counts nothing
/// past that close.
#[tokio::test]
#[ignore]
async fn an_incident_reopened_on_a_paused_monitor_is_paused_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user, target_id) = seed(&pool, "cdtpausereopen").await;
    let writer = PgIncidentStore::new(pool.clone());
    let now =
        chrono::DurationRound::duration_trunc(chrono::Utc::now(), chrono::Duration::seconds(1))
            .unwrap();
    let at = |mins: i64| now - chrono::Duration::minutes(mins);
    let id = writer
        .insert_open(
            org,
            NewOpenIncident {
                target_id,
                started_at: at(60),
                status_at_start: CheckStatus::Down,
                check_count: 2,
                error_sample: None,
                region: None,
                regions_down: vec![],
                regions_up: vec![],
            },
        )
        .await
        .expect("insert")
        .expect("opened");
    assert!(
        writer
            .close(org, id, at(50))
            .await
            .expect("close")
            .is_some()
    );
    PostgresTargetStore::from_pool(pool.clone(), None)
        .set_enabled(org, &[target_id], false, Some(user))
        .await
        .expect("pause");
    PgIncidentOpsStore::new(pool.clone())
        .reopen(org, id, Actor::User(user), None)
        .await
        .expect("reopen");
    let downtime = PgIncidentNarrationStore::new(pool.clone())
        .confirmed_downtime_by_target(
            org,
            TimeRange {
                from: at(120),
                to: at(0),
            },
            None,
        )
        .await
        .expect("downtime rollup");
    assert_eq!(downtime.get(&target_id).copied(), Some(10 * 60));
}

/// A pause that lands while the writer is opening an incident from evidence it
/// read before wins: a paused monitor opens nothing.
#[tokio::test]
#[ignore]
async fn a_paused_monitor_opens_no_incident_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user, target_id) = seed(&pool, "cdtpauseopen").await;
    PostgresTargetStore::from_pool(pool.clone(), None)
        .set_enabled(org, &[target_id], false, Some(user))
        .await
        .expect("pause");
    let opened = PgIncidentStore::new(pool.clone())
        .insert_open(
            org,
            NewOpenIncident {
                target_id,
                started_at: chrono::Utc::now(),
                status_at_start: CheckStatus::Down,
                check_count: 2,
                error_sample: None,
                region: None,
                regions_down: vec![],
                regions_up: vec![],
            },
        )
        .await
        .expect("insert");
    assert_eq!(opened, None);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM incidents WHERE target_id = $1")
        .bind(target_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

/// The shipped text, not a copy that could drift from it. Its column adds are
/// guarded, so it re-applies over the migrated test database and the backfill
/// runs against the rows seeded below.
const MIGRATION_107: &str = include_str!("../migrations/postgres/107_incident_pauses.up.sql");

/// The backfill dates pauses from the audit log: the one running on an open
/// or since-resolved incident, and each resumed one, never pairing a pause
/// with a resume from a later cycle.
#[tokio::test]
#[ignore]
async fn migration_107_backfills_pauses_from_the_audit_log_pg() {
    let Some((db, name)) = common::fresh_test_db("cdt_mig107").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    let now =
        chrono::DurationRound::duration_trunc(chrono::Utc::now(), chrono::Duration::seconds(1))
            .unwrap();
    let at = |mins: i64| now - chrono::Duration::minutes(mins);
    let incident = |org: OrgId, target: Uuid, started: i64, ended: Option<i64>| {
        let pool = pool.clone();
        async move {
            let id: Uuid = sqlx::query_scalar(
                "INSERT INTO incidents (org_id, target_id, started_at, ended_at, status_at_start, \
                                        check_count, state, visibility, origin) \
                 VALUES ($1, $2, $3, $4, 'down', 1, 'triggered', 'internal', 'monitor') RETURNING id",
            )
            .bind(org.0)
            .bind(target)
            .bind(at(started))
            .bind(ended.map(at))
            .fetch_one(&pool)
            .await
            .expect("insert incident");
            id
        }
    };
    let logged = |org: OrgId, target: Uuid, action: &'static str, mins: i64| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO org_audit_log (org_id, action, metadata, occurred_at) VALUES \
                 ($1, $2, jsonb_build_object('targets', jsonb_build_array(jsonb_build_object('id', $3::uuid))), $4)",
            )
            .bind(org.0)
            .bind(action)
            .bind(target)
            .bind(at(mins))
            .execute(&pool)
            .await
            .expect("insert audit row");
        }
    };
    let pause = |target: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query("UPDATE targets SET enabled = false WHERE id = $1")
                .bind(target)
                .execute(&pool)
                .await
                .expect("pause");
        }
    };

    let (org_open, _, open_target) = seed(&pool, "mig107open").await;
    let open = incident(org_open, open_target, 180, None).await;
    pause(open_target).await;
    logged(org_open, open_target, "target.paused", 120).await;

    let (org_closed, _, closed_target) = seed(&pool, "mig107closed").await;
    let closed = incident(org_closed, closed_target, 600, Some(60)).await;
    pause(closed_target).await;
    logged(org_closed, closed_target, "target.paused", 240).await;

    let (org_cycles, _, cycles_target) = seed(&pool, "mig107cycles").await;
    let cycles = incident(org_cycles, cycles_target, 600, Some(60)).await;
    logged(org_cycles, cycles_target, "target.paused", 540).await;
    logged(org_cycles, cycles_target, "target.paused", 300).await;
    logged(org_cycles, cycles_target, "target.resumed", 30).await;

    let (org_watched, _, watched_target) = seed(&pool, "mig107watched").await;
    let watched = incident(org_watched, watched_target, 180, None).await;

    sqlx::raw_sql(MIGRATION_107)
        .execute(&pool)
        .await
        .expect("apply 107");

    type Pauses = (
        Option<chrono::DateTime<chrono::Utc>>,
        Vec<chrono::DateTime<chrono::Utc>>,
        Vec<chrono::DateTime<chrono::Utc>>,
    );
    let pauses = |id: Uuid| {
        let pool = pool.clone();
        async move {
            let row: Pauses = sqlx::query_as(
                "SELECT paused_since, paused_from, paused_until FROM incidents WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("read pauses");
            row
        }
    };
    assert_eq!(pauses(open).await, (Some(at(120)), vec![], vec![]));
    assert_eq!(
        pauses(closed).await,
        (Some(at(240)), vec![], vec![]),
        "resolved while still paused"
    );
    assert_eq!(
        pauses(cycles).await,
        (None, vec![at(300)], vec![at(30)]),
        "the pause whose resume the log lost stays downtime"
    );
    assert_eq!(pauses(watched).await, (None, vec![], vec![]));

    common::drop_test_db(&name).await;
}
