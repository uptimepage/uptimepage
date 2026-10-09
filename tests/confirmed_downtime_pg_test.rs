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
use uptimepage::domain::{CheckStatus, NewManualIncident, OrgId, Recovered, UserId};
use uptimepage::public_status::{IncidentStore, NewOpenIncident, PgIncidentStore};
use uptimepage::storage::{
    Actor, IncidentNarrationStore, IncidentOpsStore, PgIncidentNarrationStore, PgIncidentOpsStore,
    TimeRange, create_org_with_owner,
};
use uuid::Uuid;

async fn seed(pool: &PgPool, prefix: &str) -> (OrgId, UserId, Uuid) {
    let user = make_user(pool, prefix).await;
    let org = create_org_with_owner(pool, user, &unique_slug(prefix), "svc")
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
    assert!(writer.close(org, id, at(2)).await.expect("close"));

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
