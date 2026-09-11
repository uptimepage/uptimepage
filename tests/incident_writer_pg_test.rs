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
