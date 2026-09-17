//! Postgres-backed validation of `PgIncidentStore`'s race-safety guarantees.
//! The in-memory store unit tests cover the decision state machine; these
//! exercise the real SQL that protects a 2-instance deploy: the `ON CONFLICT`
//! open is arbitrated by the partial UNIQUE index, and the close `RETURNING`
//! reports only the call that actually flipped the row. An arbiter that does
//! not match the index would error on the *first* insert, so these tests also
//! pin the index/predicate contract.
//!
//! `#[ignore]`d by default; runs under `--run-ignored all` once `DATABASE_URL`
//! is set. The harness auto-applies all migrations on first connect.

mod common;

use common::{make_user, unique_slug};
use sqlx::PgPool;
use uptimepage::domain::{CheckStatus, OrgId};
use uptimepage::public_status::{IncidentStore, NewOpenIncident, PgIncidentStore};
use uptimepage::storage::create_org_with_owner;
use uuid::Uuid;

async fn seed(pool: &PgPool, prefix: &str) -> (OrgId, Uuid) {
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
    (org.id, target_id)
}

fn new_open(target_id: Uuid) -> NewOpenIncident {
    NewOpenIncident {
        target_id,
        started_at: chrono::Utc::now() - chrono::Duration::minutes(2),
        status_at_start: CheckStatus::Down,
        check_count: 2,
        error_sample: None,
        region: None,
        regions_down: vec![],
        regions_up: vec![],
    }
}

async fn open_incident_count(pool: &PgPool, org: OrgId, target_id: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM incidents \
         WHERE org_id = $1 AND target_id = $2 AND ended_at IS NULL",
    )
    .bind(org.0)
    .bind(target_id)
    .fetch_one(pool)
    .await
    .expect("count open incidents")
}

#[tokio::test]
#[ignore]
async fn insert_open_is_single_winner_under_conflict_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, target_id) = seed(&pool, "iwopen").await;
    let store = PgIncidentStore::new(pool.clone());

    // First open wins and returns its id (also proves the ON CONFLICT arbiter
    // matches idx_incidents_org_open — a mismatch would error here).
    let first = store
        .insert_open(org, new_open(target_id))
        .await
        .expect("insert 1");
    assert!(first.is_some(), "first open must return the new id");

    // Second open for the same target conflicts → no row → no page.
    let second = store
        .insert_open(org, new_open(target_id))
        .await
        .expect("insert 2");
    assert!(
        second.is_none(),
        "second open must yield None (DB held the unique open)"
    );

    assert_eq!(
        open_incident_count(&pool, org, target_id).await,
        1,
        "exactly one open incident may exist per target"
    );
}

#[tokio::test]
#[ignore]
async fn close_reports_only_the_call_that_flipped_the_row_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, target_id) = seed(&pool, "iwclose").await;
    let store = PgIncidentStore::new(pool.clone());

    let id = store
        .insert_open(org, new_open(target_id))
        .await
        .expect("open")
        .expect("opened id");
    let ended = chrono::Utc::now();

    assert!(
        store.close(org, id, ended).await.expect("close 1"),
        "the call that flips ended_at must report true"
    );
    assert!(
        !store.close(org, id, ended).await.expect("close 2"),
        "a re-close (or race loser) must report false and not re-page"
    );
    assert_eq!(
        open_incident_count(&pool, org, target_id).await,
        0,
        "the incident is resolved"
    );
}

/// A declaration left open for weeks used to hold the monitor's only open slot,
/// so a real outage under it opened nothing, paged nobody, and was swallowed by
/// a window the operator had excluded from uptime.
#[tokio::test]
#[ignore]
async fn an_open_declaration_does_not_block_a_real_incident_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, target_id) = seed(&pool, "iwdecl").await;
    sqlx::query(
        "INSERT INTO incidents (org_id, target_id, started_at, status_at_start, \
                                check_count, state, visibility, origin, counts_as_downtime) \
         VALUES ($1, $2, now() - interval '14 day', 'down', 0, 'triggered', \
                 'internal', 'manual', false)",
    )
    .bind(org.0)
    .bind(target_id)
    .execute(&pool)
    .await
    .expect("declare");

    let store = PgIncidentStore::new(pool.clone());
    assert!(
        store
            .open_for_target(org, target_id)
            .await
            .expect("open_for_target")
            .is_none(),
        "the writer must not adopt a declaration as its own open incident"
    );

    let opened = store
        .insert_open(org, new_open(target_id))
        .await
        .expect("insert_open");
    assert!(
        opened.is_some(),
        "a real outage still opens its own incident"
    );
    assert_eq!(open_incident_count(&pool, org, target_id).await, 2);

    let second = store
        .insert_open(org, new_open(target_id))
        .await
        .expect("insert_open");
    assert!(
        second.is_none(),
        "one open monitor incident per target still holds"
    );
}

#[tokio::test]
#[ignore]
async fn widen_is_a_union_and_leaves_a_closed_incident_alone_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, target_id) = seed(&pool, "iwwiden").await;
    let store = PgIncidentStore::new(pool.clone());

    let mut new = new_open(target_id);
    new.regions_down = vec!["fra".into()];
    new.regions_up = vec!["us".into(), "hel".into(), "sg".into()];
    let id = store
        .insert_open(org, new)
        .await
        .expect("open")
        .expect("opened id");

    // Two writers each saw a different region confirm; both land, neither
    // repeats a region already confirmed.
    store
        .widen(org, id, &["us".into(), "fra".into()])
        .await
        .expect("widen");
    store.widen(org, id, &["hel".into()]).await.expect("widen");
    let open = store
        .open_for_target(org, target_id)
        .await
        .expect("read")
        .expect("still open");
    assert_eq!(open.regions_down, ["fra", "us", "hel"]);
    let (up,): (Vec<String>,) = sqlx::query_as("SELECT regions_up FROM incidents WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(up, ["sg"]);

    assert!(
        store
            .close(org, id, chrono::Utc::now())
            .await
            .expect("close")
    );
    store
        .widen(org, id, &["sg".into()])
        .await
        .expect("widen after close is a no-op");
    let (down,): (Vec<String>,) =
        sqlx::query_as("SELECT regions_down FROM incidents WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        down,
        ["fra", "us", "hel"],
        "a closed incident keeps its breakdown"
    );
}

/// A human resolved the outage while it was still failing. The next tick may
/// still hold the evidence that opened it; neither the read nor the write
/// side may turn that into a second incident. A declared incident is not a
/// line: it is the operator's narrative, not the monitor's verdict.
#[tokio::test]
#[ignore]
async fn a_resolution_is_the_line_the_next_open_must_start_after_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, target_id) = seed(&pool, "iwline").await;
    let store = PgIncidentStore::new(pool.clone());
    let now = now_us();
    let at = |secs_ago: i64| now - chrono::Duration::seconds(secs_ago);
    let started = |secs_ago: i64| NewOpenIncident {
        started_at: at(secs_ago),
        ..new_open(target_id)
    };

    assert!(
        store
            .last_closed_for_pairs(&[(org, target_id)])
            .await
            .expect("last_closed_for_pairs")
            .is_empty(),
        "nothing has closed yet"
    );
    let first = store
        .insert_open(org, started(600))
        .await
        .expect("insert_open")
        .expect("first open");
    assert!(store.close(org, first, at(300)).await.expect("close"));

    // A declaration resolved later than the monitor incident does not move
    // the line: only monitor-origin closes count.
    sqlx::query(
        "INSERT INTO incidents (org_id, target_id, started_at, ended_at, status_at_start, \
                                check_count, state, visibility, origin, counts_as_downtime) \
         VALUES ($1, $2, $3, $4, 'down', 0, 'resolved', 'internal', 'manual', false)",
    )
    .bind(org.0)
    .bind(target_id)
    .bind(at(250))
    .bind(at(100))
    .execute(&pool)
    .await
    .expect("declare and resolve");

    let closed = store
        .last_closed_for_pairs(&[(org, target_id)])
        .await
        .expect("last_closed_for_pairs");
    assert_eq!(closed.get(&(org, target_id)).copied(), Some(at(300)));

    assert!(
        store
            .insert_open(org, started(600))
            .await
            .expect("insert_open")
            .is_none(),
        "the evidence that opened the resolved incident cannot open another"
    );
    assert!(
        store
            .insert_open(org, started(300))
            .await
            .expect("insert_open")
            .is_none(),
        "evidence from the instant of the resolution is still covered by it"
    );
    let fresh = store
        .insert_open(org, started(200))
        .await
        .expect("insert_open");
    assert!(fresh.is_some(), "evidence after the resolution opens");
    assert_eq!(open_incident_count(&pool, org, target_id).await, 1);
}

/// Now as the row will hand it back: Postgres keeps microseconds.
fn now_us() -> chrono::DateTime<chrono::Utc> {
    use chrono::Timelike;
    let now = chrono::Utc::now();
    now.with_nanosecond(now.nanosecond() / 1000 * 1000)
        .expect("in range")
}

async fn closed_incident(
    pool: &PgPool,
    org: OrgId,
    target_id: Uuid,
    started_at: chrono::DateTime<chrono::Utc>,
    ended_at: chrono::DateTime<chrono::Utc>,
) {
    sqlx::query(
        "INSERT INTO incidents (org_id, target_id, started_at, ended_at, status_at_start, \
                                check_count, state, visibility, origin, counts_as_downtime) \
         VALUES ($1, $2, $3, $4, 'down', 2, 'resolved', 'internal', 'monitor', true)",
    )
    .bind(org.0)
    .bind(target_id)
    .bind(started_at)
    .bind(ended_at)
    .execute(pool)
    .await
    .expect("insert closed incident");
}

/// Incidents reopened from one stale run all share a start, so the line is
/// the greatest close, not the close on the latest-started row.
#[tokio::test]
#[ignore]
async fn the_line_is_the_greatest_close_when_starts_repeat_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, target_id) = seed(&pool, "iwtie").await;
    let store = PgIncidentStore::new(pool.clone());
    let now = now_us();
    let at = |secs_ago: i64| now - chrono::Duration::seconds(secs_ago);

    // The way a resolve-reopen loop leaves them: one start, three closes,
    // inserted latest-close first so row order cannot stand in for time.
    for closed_ago in [300, 500, 400] {
        closed_incident(&pool, org, target_id, at(600), at(closed_ago)).await;
    }
    let closed = store
        .last_closed_for_pairs(&[(org, target_id)])
        .await
        .expect("last_closed_for_pairs");
    assert_eq!(closed.get(&(org, target_id)).copied(), Some(at(300)));

    // Evidence after the greatest close opens; evidence the middle close
    // covers, which the wrong row would have let through, does not.
    let stale = NewOpenIncident {
        started_at: at(350),
        ..new_open(target_id)
    };
    assert!(
        store
            .insert_open(org, stale)
            .await
            .expect("insert_open")
            .is_none()
    );
    let fresh = NewOpenIncident {
        started_at: at(200),
        ..new_open(target_id)
    };
    assert!(
        store
            .insert_open(org, fresh)
            .await
            .expect("insert_open")
            .is_some()
    );
}

/// The writer loaded stale evidence, then a human resolved the incident
/// while the writer's insert was already waiting on the open-incident
/// conflict. The insert's own snapshot predates the resolution, so only a
/// check after the insert can see it.
#[tokio::test]
#[ignore]
async fn a_resolution_that_lands_while_the_insert_waits_still_wins_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, target_id) = seed(&pool, "iwrace").await;
    let store = std::sync::Arc::new(PgIncidentStore::new(pool.clone()));
    let now = now_us();
    let started_at = now - chrono::Duration::seconds(600);
    let first = store
        .insert_open(
            org,
            NewOpenIncident {
                started_at,
                ..new_open(target_id)
            },
        )
        .await
        .expect("insert_open")
        .expect("first open");

    // The resolve is in flight: its row lock holds any insert that conflicts
    // on the open-incident index until it commits.
    let mut resolving = pool.begin().await.expect("begin resolve");
    sqlx::query(
        "UPDATE incidents SET ended_at = now(), state = 'resolved', resolved_by = NULL \
         WHERE id = $1",
    )
    .bind(first)
    .execute(&mut *resolving)
    .await
    .expect("resolve uncommitted");

    let stale_writer = {
        let store = store.clone();
        tokio::spawn(async move {
            store
                .insert_open(
                    org,
                    NewOpenIncident {
                        started_at,
                        ..new_open(target_id)
                    },
                )
                .await
                .expect("insert_open")
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        !stale_writer.is_finished(),
        "the insert must wait on the resolve"
    );
    resolving.commit().await.expect("commit resolve");

    let reopened = stale_writer.await.expect("writer task");
    assert!(
        reopened.is_none(),
        "the evidence the resolution covered must not reopen"
    );
    let total: i64 =
        sqlx::query_scalar("SELECT count(*) FROM incidents WHERE org_id = $1 AND target_id = $2")
            .bind(org.0)
            .bind(target_id)
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(total, 1);
    assert_eq!(open_incident_count(&pool, org, target_id).await, 0);
}
