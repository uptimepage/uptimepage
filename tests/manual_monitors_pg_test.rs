//! Live-Postgres contract for manual monitors: the state store and its audit
//! row, the org and kind guards, what the scheduler restates, the agent
//! hand-out leaving them out, and the writer's escalate-only status raise.
//!
//! Live-PG ignored: needs `DATABASE_URL`. Migrations auto-apply on first
//! connect.

use crate::common;

use std::time::Duration;

use uptimepage::domain::{
    CheckSpec, CheckStatus, ExpectedStatus, ManualCheck, ManualStatus, NewTarget, OrgId, UserId,
    WriteSource,
};
use uptimepage::public_status::{IncidentStore, NewOpenIncident, PgIncidentStore};
use uptimepage::scheduler::sources::PassiveTargetSource;
use uptimepage::storage::admin::{AdminRepo, EnabledTargetSource};
use uptimepage::storage::{
    ManualStore, PgManualStore, PostgresTargetStore, TargetStore, create_org_with_owner,
};
use uuid::Uuid;

use common::{default_http_check, make_user, pg_pool_from_env, unique_slug};

async fn org(pool: &sqlx::PgPool, tag: &str) -> (OrgId, UserId) {
    let user = make_user(pool, tag).await;
    let org = create_org_with_owner(pool, user, &unique_slug(tag), "O")
        .await
        .unwrap()
        .expect("org")
        .id;
    (org, user)
}

async fn make_target(pool: &sqlx::PgPool, org: OrgId, check: CheckSpec, enabled: bool) -> Uuid {
    PostgresTargetStore::from_pool(pool.clone(), None)
        .create(
            org,
            NewTarget {
                name: "sip trunks".into(),
                check,
                interval: Duration::from_secs(60),
                enabled,
                tags: vec![],
                alerts: Default::default(),
                region_policy: Default::default(),
                alert_confirmations: 1,
                notify_recovery: true,
                renotify_interval_secs: 3600,
                group_name: None,
                owner_user_id: None,
                regions: None,
            },
            WriteSource::Ui,
            i64::MAX,
            i64::MAX,
        )
        .await
        .expect("create target")
        .id
}

fn manual() -> CheckSpec {
    CheckSpec::Manual(ManualCheck {})
}

async fn audit_rows(pool: &sqlx::PgPool, org: OrgId) -> Vec<serde_json::Value> {
    sqlx::query_scalar(
        "SELECT metadata FROM org_audit_log \
         WHERE org_id = $1 AND action = 'target.state_set' ORDER BY id",
    )
    .bind(org.0)
    .fetch_all(pool)
    .await
    .expect("audit rows")
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
#[ignore = "requires DATABASE_URL; run via DATABASE_URL=... cargo test -- --ignored"]
async fn a_set_lands_once_with_its_audit_row_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, user_a) = org(&pool, "manual-set").await;
    let (org_b, user_b) = org(&pool, "manual-set").await;
    let store = PgManualStore::new(pool.clone());
    let target = make_target(&pool, org_a, manual(), true).await;

    assert!(store.get(org_a, target).await.unwrap().is_none());

    let change = store
        .set(
            org_a,
            target,
            ManualStatus::Down,
            Some("carrier outage".into()),
            Some(user_a),
        )
        .await
        .unwrap()
        .expect("a manual target in its org");
    assert!(change.changed());
    assert_eq!(change.prev.status, ManualStatus::Up, "never set reads up");
    assert_eq!(change.state.status, ManualStatus::Down);
    assert_eq!(change.prev.set_by, None, "nobody set the initial state");
    assert_eq!(change.state.set_by, Some(user_a));
    assert!(change.state.set_at > change.prev.set_at);
    assert_eq!(
        store.get(org_a, target).await.unwrap(),
        Some(change.state.clone())
    );

    let again = store
        .set(
            org_a,
            target,
            ManualStatus::Down,
            Some("carrier outage".into()),
            Some(user_a),
        )
        .await
        .unwrap()
        .expect("still there");
    assert!(!again.changed());
    assert_eq!(
        again.state, change.state,
        "a no-op moves nothing, not even the time"
    );

    let audit = audit_rows(&pool, org_a).await;
    assert_eq!(audit.len(), 1, "the no-op wrote no audit row");
    assert_eq!(audit[0]["from"], "up");
    assert_eq!(audit[0]["to"], "down");
    assert_eq!(audit[0]["note"], "carrier outage");

    assert!(
        store
            .set(org_b, target, ManualStatus::Up, None, Some(user_b))
            .await
            .unwrap()
            .is_none(),
        "another org's target is not found"
    );
    let http = default_http_check(
        "https://example.com/".parse().unwrap(),
        ExpectedStatus::Exact(200),
    );
    let probed = make_target(&pool, org_a, CheckSpec::Http(http), true).await;
    assert!(
        store
            .set(org_a, probed, ManualStatus::Down, None, Some(user_a))
            .await
            .unwrap()
            .is_none(),
        "only a manual target takes a state"
    );
    assert_eq!(
        store.get(org_a, target).await.unwrap().unwrap().status,
        ManualStatus::Down,
        "the refused sets changed nothing"
    );
    let next = store
        .set(org_a, target, ManualStatus::Up, None, None)
        .await
        .unwrap()
        .expect("set");
    assert_eq!(
        next.state.set_by, None,
        "a set replaces who made the last one"
    );

    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL; run via DATABASE_URL=... cargo test -- --ignored"]
async fn the_scheduler_restates_what_was_set_and_agents_never_see_it_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, user_a) = org(&pool, "manual-sched").await;
    let store = PgManualStore::new(pool.clone());
    let never_set = make_target(&pool, org_a, manual(), true).await;
    let set_down = make_target(&pool, org_a, manual(), true).await;
    let paused = make_target(&pool, org_a, manual(), false).await;
    store
        .set(org_a, set_down, ManualStatus::Down, None, Some(user_a))
        .await
        .unwrap()
        .expect("set");

    let repo = AdminRepo::new(pool.clone(), None, "manual_test");
    let passive = uptimepage::worker::PassiveRuntimes {
        heartbeat: std::sync::Arc::default(),
        manual: std::sync::Arc::default(),
    };
    let source = PassiveTargetSource::new(
        AdminRepo::new(pool.clone(), None, "manual_test"),
        passive.clone(),
    );
    let handed_out: Vec<_> = source
        .list_all_enabled_targets()
        .await
        .unwrap()
        .into_iter()
        .filter(|(o, _)| *o == org_a)
        .map(|(_, t)| t)
        .collect();
    let ids: Vec<Uuid> = handed_out.iter().map(|t| t.id).collect();
    assert!(ids.contains(&never_set) && ids.contains(&set_down));
    assert!(!ids.contains(&paused), "a paused monitor is not restated");
    assert!(
        handed_out
            .iter()
            .all(|t| t.interval == Duration::from_secs(60)),
        "restated once a minute"
    );
    assert_eq!(
        passive.manual.state(never_set).unwrap().status,
        ManualStatus::Up
    );
    assert_eq!(
        passive.manual.state(set_down).unwrap().status,
        ManualStatus::Down
    );

    let regions: Vec<String> =
        sqlx::query_scalar("SELECT region FROM target_regions WHERE target_id = $1")
            .bind(set_down)
            .fetch_all(&pool)
            .await
            .unwrap();
    for region in regions {
        let pulled = repo
            .list_enabled_targets_for_region(&region, true, &Default::default())
            .await
            .unwrap();
        assert!(
            pulled.iter().all(|(_, t)| t.id != set_down),
            "an agent in {region} is never handed a manual monitor"
        );
        assert!(
            !repo
                .assigned_targets_for_region(&region)
                .await
                .unwrap()
                .contains_key(&set_down),
            "nor are its results accepted from one"
        );
    }

    cleanup(&pool, &[org_a], &[user_a]).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL; run via DATABASE_URL=... cargo test -- --ignored"]
async fn escalation_marks_an_open_incident_down_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, user_a) = org(&pool, "manual-esc").await;
    let target = make_target(&pool, org_a, manual(), true).await;
    let incidents = PgIncidentStore::new(pool.clone());
    let id = incidents
        .insert_open(
            org_a,
            NewOpenIncident {
                target_id: target,
                started_at: chrono::Utc::now() - chrono::Duration::minutes(2),
                status_at_start: CheckStatus::Degraded,
                check_count: 1,
                error_sample: Some("marked degraded".into()),
                region: None,
                regions_down: vec![],
                regions_up: vec![],
            },
        )
        .await
        .unwrap()
        .expect("opened");
    let worst = || async {
        incidents
            .open_for_target(org_a, target)
            .await
            .unwrap()
            .expect("open")
            .worst_status
    };
    assert_eq!(worst().await, CheckStatus::Degraded);

    incidents
        .escalate(org_a, id, Some("marked down".into()))
        .await
        .unwrap();
    assert_eq!(worst().await, CheckStatus::Down);
    incidents
        .escalate(org_a, id, Some("a later cause".into()))
        .await
        .unwrap();
    let cause: Option<String> =
        sqlx::query_scalar("SELECT error_sample FROM incidents WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        cause.as_deref(),
        Some("marked down"),
        "an incident already down is left alone"
    );
    let notes: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT kind, actor_type, message FROM incident_events WHERE incident_id = $1",
    )
    .bind(id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(notes.len(), 1, "one note for the one escalation: {notes:?}");
    assert_eq!(
        (notes[0].0.as_str(), notes[0].1.as_str()),
        ("note", "system")
    );
    assert!(
        notes[0]
            .2
            .as_deref()
            .is_some_and(|m| m.contains("raised to down")),
        "{notes:?}"
    );

    cleanup(&pool, &[org_a], &[user_a]).await;
}

/// The note is the incident's cause for as long as it is open: a worse state
/// or a new note replaces it, and up leaves the record of what it was.
#[tokio::test]
#[ignore = "requires DATABASE_URL; run via DATABASE_URL=... cargo test -- --ignored"]
async fn the_open_incident_carries_the_latest_note_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, user_a) = org(&pool, "manual-cause").await;
    let target = make_target(&pool, org_a, manual(), true).await;
    let store = PgManualStore::new(pool.clone());
    store
        .set(
            org_a,
            target,
            ManualStatus::Degraded,
            Some("one trunk of two".into()),
            Some(user_a),
        )
        .await
        .unwrap()
        .expect("set");
    let id = PgIncidentStore::new(pool.clone())
        .insert_open(
            org_a,
            NewOpenIncident {
                target_id: target,
                started_at: chrono::Utc::now() - chrono::Duration::seconds(5),
                status_at_start: CheckStatus::Degraded,
                check_count: 1,
                error_sample: Some("marked degraded: one trunk of two".into()),
                region: None,
                regions_down: vec![],
                regions_up: vec![],
            },
        )
        .await
        .unwrap()
        .expect("opened");
    let cause = || async {
        sqlx::query_scalar::<_, Option<String>>("SELECT error_sample FROM incidents WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
    };

    store
        .set(
            org_a,
            target,
            ManualStatus::Down,
            Some("all trunks down".into()),
            Some(user_a),
        )
        .await
        .unwrap()
        .expect("set");
    assert_eq!(
        cause().await.as_deref(),
        Some("marked down: all trunks down")
    );

    store
        .set(
            org_a,
            target,
            ManualStatus::Down,
            Some("carrier ticket 4411".into()),
            Some(user_a),
        )
        .await
        .unwrap()
        .expect("set");
    assert_eq!(
        cause().await.as_deref(),
        Some("marked down: carrier ticket 4411"),
        "a note-only change reaches the incident"
    );

    store
        .set(org_a, target, ManualStatus::Up, None, Some(user_a))
        .await
        .unwrap()
        .expect("set");
    assert_eq!(
        cause().await.as_deref(),
        Some("marked down: carrier ticket 4411"),
        "up leaves the cause the outage had"
    );

    cleanup(&pool, &[org_a], &[user_a]).await;
}
