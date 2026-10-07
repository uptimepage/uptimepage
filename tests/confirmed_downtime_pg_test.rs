//! Postgres-backed regression on what reaches the uptime figure.
//! `confirmed_downtime_by_target` must skip target-less (manual) incidents: its
//! `GROUP BY target_id` would otherwise return a NULL row and fail decoding into
//! `Uuid`. The in-memory store can't reproduce this — its `Incident.target_id` is
//! non-nullable. It must also skip a declared incident bound to a monitor.
//!
//! `#[ignore]`d by default; runs under `--run-ignored all` with `DATABASE_URL`
//! set. The harness auto-applies migrations on first connect.

use crate::common;

use common::{make_user, unique_slug};
use sqlx::PgPool;
use uptimepage::domain::{NewManualIncident, OrgId, UserId};
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
        .confirmed_downtime_by_target(org, range)
        .await
        .expect("must not error on a null-target incident");

    assert_eq!(map.len(), 1, "only the real target appears");
    assert_eq!(
        map.get(&target_id).copied(),
        Some(600),
        "10 minutes of clamped downtime"
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
        .confirmed_downtime_by_target(org, range)
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
        .confirmed_downtime_by_target(org, range)
        .await
        .expect("downtime rollup");
    assert_eq!(
        map.get(&target_id).copied(),
        Some(600),
        "once counted, the declared ten minutes lands like any other"
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
    let spans = store.downtime_spans(org, range).await.expect("spans");
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
