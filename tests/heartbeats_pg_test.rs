//! Live-Postgres contract for `heartbeat_monitors`: idempotent token minting,
//! single-statement ping recording (unknown / deleted-target / deleted-org →
//! None), the store-level disabled→enabled re-arm, per-org isolation, the
//! refresh-time row self-heal, the never-pinged dispatch gate, token rotation
//! (overlap, revoke-now, expiry, audit), and migrations 031 and 047.
//!
//! Live-PG ignored: needs `DATABASE_URL`. Migrations auto-apply on first
//! connect. Point it at a throwaway DB to also validate migration 031.

mod common;

use std::time::Duration;

use uptimepage::domain::{
    CheckSpec, ExpectedStatus, HeartbeatCheck, NewTarget, OrgId, PingSignal, TargetUpdate, UserId,
    WriteSource,
};
use uptimepage::storage::admin::AdminRepo;
use uptimepage::storage::{
    HeartbeatStore, PgHeartbeatStore, PostgresTargetStore, RestoreOutcome, TargetStore,
    create_org_with_owner, restore_org, soft_delete_org,
};
use uuid::Uuid;

use common::{default_http_check, make_user, pg_pool_from_env, unique_slug};

async fn two_orgs(pool: &sqlx::PgPool, tag: &str) -> (OrgId, OrgId, UserId, UserId) {
    let user_a = make_user(pool, tag).await;
    let user_b = make_user(pool, tag).await;
    let org_a = create_org_with_owner(pool, user_a, &unique_slug(tag), "A")
        .await
        .unwrap()
        .expect("org a")
        .id;
    let org_b = create_org_with_owner(pool, user_b, &unique_slug(tag), "B")
        .await
        .unwrap()
        .expect("org b")
        .id;
    (org_a, org_b, user_a, user_b)
}

fn heartbeat_target(name: &str, enabled: bool) -> NewTarget {
    NewTarget {
        name: name.into(),
        check: CheckSpec::Heartbeat(HeartbeatCheck {
            period: Duration::from_secs(300),
            grace: Duration::from_secs(60),
            max_runtime: None,
        }),
        interval: Duration::from_secs(60),
        enabled,
        tags: vec![],
        alerts: Default::default(),
        region_policy: Default::default(),
        alert_confirmations: 2,
        notify_recovery: true,
        renotify_interval_secs: 3600,
        group_name: None,
        owner_user_id: None,
        regions: None,
    }
}

async fn make_heartbeat_target(pool: &sqlx::PgPool, org: OrgId, name: &str, enabled: bool) -> Uuid {
    let store = PostgresTargetStore::from_pool(pool.clone(), None);
    store
        .create(
            org,
            heartbeat_target(name, enabled),
            WriteSource::Ui,
            i64::MAX,
            i64::MAX,
        )
        .await
        .expect("create target")
        .id
}

async fn join_org(pool: &sqlx::PgPool, org: OrgId, user: UserId) {
    sqlx::query("INSERT INTO memberships (user_id, org_id, role) VALUES ($1, $2, 'member')")
        .bind(user.0)
        .bind(org.0)
        .execute(pool)
        .await
        .expect("join org");
}

async fn email_of(pool: &sqlx::PgPool, user: UserId) -> String {
    let row: (String,) = sqlx::query_as("SELECT email FROM users WHERE id = $1")
        .bind(user.0)
        .fetch_one(pool)
        .await
        .expect("user email");
    row.0
}

/// Ids the scheduler would actually evaluate this tick.
async fn dispatched_heartbeats(repo: &AdminRepo) -> Vec<Uuid> {
    repo.list_enabled_heartbeat_targets()
        .await
        .unwrap()
        .into_iter()
        .map(|(_, t)| t.id)
        .collect()
}

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/postgres");

/// The nudge sweep and migration 047 are org-unscoped by design, so they run on
/// their own database instead of stomping a parallel test's rows.
async fn isolated_pool(tag: &str) -> Option<(sqlx::PgPool, String)> {
    let (url, name) = common::fresh_test_db(tag).await?;
    let pool = common::open_test_pool(&url).await;
    MIGRATOR.run(&pool).await.expect("migrate isolated db");
    Some((pool, name))
}

/// One sweep against a sender that records what it accepted.
async fn run_nudge(
    pool: &sqlx::PgPool,
) -> (u64, std::sync::Arc<uptimepage::email::InMemoryEmailSender>) {
    use uptimepage::jobs::heartbeat_nudge::{NudgeConfig, nudge_unwired_heartbeats};
    let sender = std::sync::Arc::new(uptimepage::email::InMemoryEmailSender::new());
    let cfg = std::sync::Arc::new(NudgeConfig {
        email: uptimepage::notifier::EmailDelivery {
            sender: sender.clone(),
            from_address: "noreply@test.example".into(),
            from_name: "Uptimepage".into(),
        },
        public_base_url: "https://app.test".into(),
        docs_url: Some("https://docs.test/monitor-types".into()),
    });
    let sent = nudge_unwired_heartbeats(pool, &cfg).await.unwrap();
    (sent, sender)
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
async fn ensure_mints_once_and_ping_round_trips_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b, user_a, user_b) = two_orgs(&pool, "hb-rt").await;
    let store = PgHeartbeatStore::new(pool.clone(), None);
    let target = make_heartbeat_target(&pool, org_a, "backup", true).await;

    let first = store.ensure(org_a, target).await.unwrap().expect("row");
    let token = first.token.clone().expect("plaintext without cipher");
    assert!(first.last_ping_at.is_none());
    assert_eq!(
        first.ping_state().success_at,
        first.armed_at,
        "no ping yet → armed_at"
    );

    // Repeated ensure keeps the same token; a foreign org can't mint or read.
    let again = store.ensure(org_a, target).await.unwrap().expect("row");
    assert_eq!(again.token, first.token);
    assert!(store.ensure(org_b, target).await.unwrap().is_none());
    assert!(store.get(org_b, target).await.unwrap().is_none());

    // One-statement ping: resolves, records, and reads back.
    let accepted = store
        .record_signal_by_token(&token, PingSignal::Success, None)
        .await
        .unwrap()
        .expect("token records");
    let at = accepted.at;
    assert_eq!(accepted.target_id, target);
    let read = store.get(org_a, target).await.unwrap().expect("row");
    assert_eq!(read.last_ping_at, Some(at));
    assert_eq!(
        read.ping_state().success_at,
        at,
        "ping newer than arm point wins"
    );
    assert!(
        store
            .record_signal_by_token("nope", PingSignal::Success, None)
            .await
            .unwrap()
            .is_none()
    );

    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL; run via DATABASE_URL=... cargo test -- --ignored"]
async fn enable_rearms_only_disabled_heartbeats_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b, user_a, user_b) = two_orgs(&pool, "hb-arm").await;
    let store = PgHeartbeatStore::new(pool.clone(), None);
    let targets = PostgresTargetStore::from_pool(pool.clone(), None);
    let paused = make_heartbeat_target(&pool, org_a, "paused", false).await;
    let running = make_heartbeat_target(&pool, org_a, "running", true).await;
    let armed_at = |org, id| {
        let store = &store;
        async move { store.get(org, id).await.unwrap().unwrap() }
    };
    store.ensure(org_a, paused).await.unwrap();
    store.ensure(org_a, running).await.unwrap();
    let paused_before = armed_at(org_a, paused).await;
    let running_before = armed_at(org_a, running).await;

    // A foreign org's enable can't re-arm across the tenant boundary.
    targets
        .set_enabled(org_b, &[paused], true, None)
        .await
        .unwrap();
    assert_eq!(
        armed_at(org_a, paused).await.armed_at,
        paused_before.armed_at
    );

    // set_enabled re-arms the disabled→enabled flip only, and the re-arm is
    // NOT a fabricated ping.
    targets
        .set_enabled(org_a, &[paused, running], true, None)
        .await
        .unwrap();
    let paused_after = armed_at(org_a, paused).await;
    assert!(paused_after.armed_at > paused_before.armed_at);
    assert!(paused_after.last_ping_at.is_none());
    assert_eq!(
        armed_at(org_a, running).await.armed_at,
        running_before.armed_at,
        "already-enabled monitors keep their arm point"
    );

    // The PATCH-style update path shares the same contract.
    targets
        .set_enabled(org_a, &[paused], false, None)
        .await
        .unwrap();
    let re_paused = armed_at(org_a, paused).await;
    targets
        .update(
            org_a,
            paused,
            TargetUpdate {
                enabled: Some(true),
                ..Default::default()
            },
            Some(WriteSource::Ui),
            None,
        )
        .await
        .unwrap();
    assert!(armed_at(org_a, paused).await.armed_at > re_paused.armed_at);

    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL; run via DATABASE_URL=... cargo test -- --ignored"]
async fn dead_tokens_stop_recording_and_sync_heals_rows_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b, user_a, user_b) = two_orgs(&pool, "hb-del").await;
    let store = PgHeartbeatStore::new(pool.clone(), None);
    let targets = PostgresTargetStore::from_pool(pool.clone(), None);

    // Target delete cascades the row.
    let doomed = make_heartbeat_target(&pool, org_a, "doomed", true).await;
    let tok_doomed = store
        .ensure(org_a, doomed)
        .await
        .unwrap()
        .unwrap()
        .token
        .unwrap();
    assert!(targets.delete(org_a, doomed, None).await.unwrap());
    assert!(
        store
            .record_signal_by_token(&tok_doomed, PingSignal::Success, None)
            .await
            .unwrap()
            .is_none()
    );

    // Kind switch away: remove() revokes the token.
    let switched = make_heartbeat_target(&pool, org_a, "switched", true).await;
    let tok_switched = store
        .ensure(org_a, switched)
        .await
        .unwrap()
        .unwrap()
        .token
        .unwrap();
    let url = url::Url::parse("https://example.com/").unwrap();
    targets
        .update(
            org_a,
            switched,
            TargetUpdate {
                check: Some(CheckSpec::Http(default_http_check(
                    url,
                    ExpectedStatus::Exact(200),
                ))),
                ..Default::default()
            },
            Some(WriteSource::Ui),
            None,
        )
        .await
        .unwrap();
    assert!(store.remove(org_a, switched).await.unwrap());
    assert!(
        store
            .record_signal_by_token(&tok_switched, PingSignal::Success, None)
            .await
            .unwrap()
            .is_none()
    );

    // Refresh-time self-heal: a heartbeat target with a lost row gets one
    // minted, and its anchor shows up in the snapshot.
    let healed = make_heartbeat_target(&pool, org_a, "healed", true).await;
    sqlx::query("DELETE FROM heartbeat_monitors WHERE target_id = $1")
        .bind(healed)
        .execute(&pool)
        .await
        .unwrap();
    let repo = AdminRepo::new(pool.clone(), None, "heartbeat_refresh");
    let anchors = repo.sync_heartbeat_rows().await.unwrap();
    assert!(anchors.iter().any(|(id, _)| *id == healed));
    assert!(store.get(org_a, healed).await.unwrap().is_some());

    // Soft-deleted org stops recording without dropping the row, and its
    // anchors leave the snapshot.
    let orphan = make_heartbeat_target(&pool, org_b, "orphan", true).await;
    let tok_orphan = store
        .ensure(org_b, orphan)
        .await
        .unwrap()
        .unwrap()
        .token
        .unwrap();
    soft_delete_org(&pool, org_b, user_b).await.unwrap();
    assert!(
        store
            .record_signal_by_token(&tok_orphan, PingSignal::Success, None)
            .await
            .unwrap()
            .is_none()
    );
    let anchors = repo.sync_heartbeat_rows().await.unwrap();
    assert!(anchors.iter().all(|(id, _)| *id != orphan));

    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL; run via DATABASE_URL=... cargo test -- --ignored"]
async fn a_never_pinged_heartbeat_is_not_dispatched_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b, user_a, user_b) = two_orgs(&pool, "hb-pending").await;
    let store = PgHeartbeatStore::new(pool.clone(), None);
    let targets = PostgresTargetStore::from_pool(pool.clone(), None);
    let repo = AdminRepo::new(pool.clone(), None, "heartbeat_pending");
    let unwired = make_heartbeat_target(&pool, org_a, "unwired", true).await;
    let wired = make_heartbeat_target(&pool, org_a, "wired", true).await;
    store.ensure(org_a, unwired).await.unwrap();
    let token = store
        .ensure(org_a, wired)
        .await
        .unwrap()
        .unwrap()
        .token
        .unwrap();

    let listed = dispatched_heartbeats(&repo).await;
    assert!(
        !listed.contains(&unwired) && !listed.contains(&wired),
        "neither has pinged, so neither is evaluated"
    );

    // A fail is the job speaking, so it wires the monitor up like a success.
    let accepted = store
        .record_signal_by_token(&token, PingSignal::Fail, Some(1))
        .await
        .unwrap()
        .expect("token resolves");
    let first = store
        .get(org_a, wired)
        .await
        .unwrap()
        .unwrap()
        .first_ping_at;
    assert_eq!(first, Some(accepted.at));
    let listed = dispatched_heartbeats(&repo).await;
    assert!(listed.contains(&wired) && !listed.contains(&unwired));

    // Wired up once is wired up for good.
    store
        .record_signal_by_token(&token, PingSignal::Success, None)
        .await
        .unwrap()
        .unwrap();
    targets
        .set_enabled(org_a, &[wired], false, None)
        .await
        .unwrap();
    targets
        .set_enabled(org_a, &[wired], true, None)
        .await
        .unwrap();
    let after = store.get(org_a, wired).await.unwrap().unwrap();
    assert_eq!(after.first_ping_at, first, "the re-arm is not a first ping");
    assert!(dispatched_heartbeats(&repo).await.contains(&wired));

    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}

/// The shipped text, not a copy that could drift from it. Both statements are
/// guarded, so re-applying is a no-op on anything already handled.
const MIGRATION_047: &str = include_str!("../migrations/postgres/047_heartbeat_first_ping.up.sql");

#[tokio::test]
#[ignore = "requires DATABASE_URL; run via DATABASE_URL=... cargo test -- --ignored"]
async fn migration_047_backfills_wired_rows_and_withdraws_false_incidents_live_pg() {
    let Some((pool, db)) = isolated_pool("hb_mig047").await else {
        return;
    };
    let (org_a, _org_b, _user_a, _user_b) = two_orgs(&pool, "hb-mig047").await;
    let store = PgHeartbeatStore::new(pool.clone(), None);

    let unwired = make_heartbeat_target(&pool, org_a, "unwired", true).await;
    let fail_only = make_heartbeat_target(&pool, org_a, "fail-only", true).await;
    let manual = make_heartbeat_target(&pool, org_a, "manual-incident", true).await;
    for id in [unwired, fail_only, manual] {
        store.ensure(org_a, id).await.unwrap();
    }

    // A monitor that has only ever said "I failed" has still spoken.
    let failed_at: (chrono::DateTime<chrono::Utc>,) = sqlx::query_as(
        "UPDATE heartbeat_monitors \
         SET last_fail_at = now() - INTERVAL '2 hours', last_exit_code = 137, first_ping_at = NULL \
         WHERE target_id = $1 RETURNING last_fail_at",
    )
    .bind(fail_only)
    .fetch_one(&pool)
    .await
    .unwrap();

    let open_incident = |target: Uuid, origin: &'static str| {
        let pool = pool.clone();
        async move {
            let row: (Uuid,) = sqlx::query_as(
                "INSERT INTO incidents (org_id, target_id, started_at, status_at_start, origin, visibility) \
                 VALUES ($1, $2, now() - INTERVAL '90 minutes', 'down', $3, 'public') RETURNING id",
            )
            .bind(org_a.0)
            .bind(target)
            .bind(origin)
            .fetch_one(&pool)
            .await
            .unwrap();
            row.0
        }
    };
    let false_alarm = open_incident(unwired, "monitor").await;
    let declared = open_incident(manual, "manual").await;

    sqlx::raw_sql(MIGRATION_047).execute(&pool).await.unwrap();

    assert_eq!(
        store
            .get(org_a, fail_only)
            .await
            .unwrap()
            .unwrap()
            .first_ping_at,
        Some(failed_at.0),
        "a fail is the job speaking, so the backfill treats it as wired"
    );
    assert!(
        store
            .get(org_a, unwired)
            .await
            .unwrap()
            .unwrap()
            .first_ping_at
            .is_none(),
        "nothing ever arrived, so it stays pending"
    );

    let closed = |id: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (Option<chrono::DateTime<chrono::Utc>>, String)>(
                "SELECT ended_at, state FROM incidents WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let (ended, state) = closed(false_alarm).await;
    assert!(ended.is_some() && state == "resolved", "{state:?}");
    let (still_open, state) = closed(declared).await;
    assert!(
        still_open.is_none() && state == "triggered",
        "a hand-declared incident is not ours to withdraw: {state:?}"
    );

    // A public incident carries the retraction so the page stops showing it
    // open. Nothing else is notified: the writer never re-reads these rows.
    let updates: Vec<(String, String)> =
        sqlx::query_as("SELECT phase, message FROM incident_updates WHERE incident_id = $1")
            .bind(false_alarm)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].0, "resolved");
    assert!(
        updates[0].1.contains("never received a ping"),
        "{:?}",
        updates[0].1
    );
    assert!(
        sqlx::query_as::<_, (i64,)>("SELECT count(*) FROM incident_updates WHERE incident_id = $1")
            .bind(declared)
            .fetch_one(&pool)
            .await
            .unwrap()
            .0
            == 0,
        "the untouched incident gets no update either"
    );

    common::drop_test_db(&db).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL; run via DATABASE_URL=... cargo test -- --ignored"]
async fn the_nudge_tells_the_owner_once_about_an_unwired_heartbeat_live_pg() {
    use uptimepage::email::EmailTemplate;
    use uptimepage::jobs::heartbeat_nudge::{NudgeConfig, nudge_unwired_heartbeats};

    let Some((pool, db)) = isolated_pool("hb_nudge").await else {
        return;
    };
    let (org_a, _org_b, _user_a, _user_b) = two_orgs(&pool, "hb-nudge").await;
    let store = PgHeartbeatStore::new(pool.clone(), None);
    let targets = PostgresTargetStore::from_pool(pool.clone(), None);

    // The monitor names an owner of its own; the org owner must not be used.
    let monitor_owner = make_user(&pool, "hb-nudge-owner").await;
    join_org(&pool, org_a, monitor_owner).await;
    let owner_email = email_of(&pool, monitor_owner).await;

    let stale = make_heartbeat_target(&pool, org_a, "stale", true).await;
    let fresh = make_heartbeat_target(&pool, org_a, "fresh", true).await;
    let paused = make_heartbeat_target(&pool, org_a, "paused", true).await;
    let wired = make_heartbeat_target(&pool, org_a, "wired", true).await;
    let token = store
        .ensure(org_a, wired)
        .await
        .unwrap()
        .unwrap()
        .token
        .unwrap();
    for id in [stale, fresh, paused] {
        store.ensure(org_a, id).await.unwrap();
    }
    store
        .record_signal_by_token(&token, PingSignal::Success, None)
        .await
        .unwrap()
        .unwrap();
    targets
        .set_enabled(org_a, &[paused], false, None)
        .await
        .unwrap();
    sqlx::query("UPDATE targets SET owner_user_id = $2 WHERE id = $1")
        .bind(stale)
        .bind(monitor_owner.0)
        .execute(&pool)
        .await
        .unwrap();
    // Only `stale` and `paused` are old enough to qualify on age alone.
    sqlx::query(
        "UPDATE heartbeat_monitors SET created_at = now() - INTERVAL '5 days' \
         WHERE target_id = ANY($1)",
    )
    .bind(vec![stale, paused, wired])
    .execute(&pool)
    .await
    .unwrap();

    let sender = std::sync::Arc::new(uptimepage::email::InMemoryEmailSender::new());
    let cfg = std::sync::Arc::new(NudgeConfig {
        email: uptimepage::notifier::EmailDelivery {
            sender: sender.clone(),
            from_address: "noreply@test.example".into(),
            from_name: "Uptimepage".into(),
        },
        public_base_url: "https://app.test".into(),
        docs_url: Some("https://docs.test/monitor-types".into()),
    });

    assert_eq!(nudge_unwired_heartbeats(&pool, &cfg).await.unwrap(), 1);
    let sent = sender.sent();
    assert_eq!(sent.len(), 1, "one monitor qualified, so one mail");
    assert_eq!(sent[0].to.address, owner_email, "the monitor's own owner");
    let EmailTemplate::HeartbeatNeverPinged {
        monitor_name,
        monitor_url,
        ..
    } = &sent[0].template
    else {
        panic!("wrong template: {:?}", sent[0].template);
    };
    assert_eq!(monitor_name, "stale");
    assert_eq!(
        monitor_url.as_deref(),
        Some(format!("https://app.test/targets/{stale}").as_str())
    );

    // A second sweep is silent: this is a reminder, not a recurring alert.
    sender.clear();
    assert_eq!(nudge_unwired_heartbeats(&pool, &cfg).await.unwrap(), 0);
    assert!(sender.is_empty());

    // The ones that did not qualify stay eligible for the day they earn it.
    for (id, why) in [
        (fresh, "created moments ago"),
        (paused, "deliberately paused"),
        (wired, "has pinged"),
    ] {
        assert!(nudged_at(&pool, id).await.is_none(), "{why}");
    }

    common::drop_test_db(&db).await;
}

/// A monitor with no owner of its own still reaches somebody: the org's owner.
/// Without this the four ownerless heartbeats in a typical org would be the
/// exact monitors the reminder was written for and never hear about it.
#[tokio::test]
#[ignore = "requires DATABASE_URL; run via DATABASE_URL=... cargo test -- --ignored"]
async fn the_nudge_falls_back_to_the_org_owner_and_skips_dead_orgs_live_pg() {
    let Some((pool, db)) = isolated_pool("hb_nudge_fb").await else {
        return;
    };
    let (org_a, org_b, user_a, user_b) = two_orgs(&pool, "hb-nudge-fb").await;
    let store = PgHeartbeatStore::new(pool.clone(), None);
    let org_owner_email = email_of(&pool, user_a).await;

    let ownerless = make_heartbeat_target(&pool, org_a, "ownerless", true).await;
    let doomed = make_heartbeat_target(&pool, org_b, "doomed", true).await;
    for (org, id) in [(org_a, ownerless), (org_b, doomed)] {
        store.ensure(org, id).await.unwrap();
    }
    sqlx::query("UPDATE heartbeat_monitors SET created_at = now() - INTERVAL '5 days'")
        .execute(&pool)
        .await
        .unwrap();
    soft_delete_org(&pool, org_b, user_b).await.unwrap();

    let (sent, sender) = run_nudge(&pool).await;
    assert_eq!(sent, 1, "the dead org's monitor is not anyone's problem");
    let sent = sender.sent();
    assert_eq!(sent[0].to.address, org_owner_email);

    assert!(nudged_at(&pool, doomed).await.is_none());

    common::drop_test_db(&db).await;
}

/// A provider outage must not burn the only reminder a monitor ever gets.
#[tokio::test]
#[ignore = "requires DATABASE_URL; run via DATABASE_URL=... cargo test -- --ignored"]
async fn a_failed_nudge_is_not_stamped_and_retries_live_pg() {
    use uptimepage::email::{EmailError, EmailResult, EmailSender, MessageId, TransactionalEmail};
    use uptimepage::jobs::heartbeat_nudge::{NudgeConfig, nudge_unwired_heartbeats};

    struct DeadSender;
    #[async_trait::async_trait]
    impl EmailSender for DeadSender {
        async fn send(&self, _: TransactionalEmail) -> EmailResult<MessageId> {
            Err(EmailError::Transport("provider unreachable".into()))
        }
    }

    let Some((pool, db)) = isolated_pool("hb_nudge_fail").await else {
        return;
    };
    let (org_a, _org_b, _user_a, _user_b) = two_orgs(&pool, "hb-nudge-fail").await;
    let store = PgHeartbeatStore::new(pool.clone(), None);
    let target = make_heartbeat_target(&pool, org_a, "unreachable", true).await;
    store.ensure(org_a, target).await.unwrap();
    sqlx::query("UPDATE heartbeat_monitors SET created_at = now() - INTERVAL '5 days'")
        .execute(&pool)
        .await
        .unwrap();

    let dead = std::sync::Arc::new(NudgeConfig {
        email: uptimepage::notifier::EmailDelivery {
            sender: std::sync::Arc::new(DeadSender),
            from_address: "noreply@test.example".into(),
            from_name: "Uptimepage".into(),
        },
        public_base_url: "https://app.test".into(),
        docs_url: None,
    });
    assert_eq!(nudge_unwired_heartbeats(&pool, &dead).await.unwrap(), 0);
    assert!(
        nudged_at(&pool, target).await.is_none(),
        "an undelivered reminder is not delivered"
    );

    // Next tick, provider back: the monitor is still eligible.
    let (sent, _) = run_nudge(&pool).await;
    assert_eq!(sent, 1);

    common::drop_test_db(&db).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL; run via DATABASE_URL=... cargo test -- --ignored"]
async fn restore_org_rearms_heartbeats_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b, user_a, user_b) = two_orgs(&pool, "hb-restore").await;
    let store = PgHeartbeatStore::new(pool.clone(), None);
    let target = make_heartbeat_target(&pool, org_a, "cron", true).await;
    let token = store
        .ensure(org_a, target)
        .await
        .unwrap()
        .unwrap()
        .token
        .unwrap();
    store
        .record_signal_by_token(&token, PingSignal::Success, None)
        .await
        .unwrap()
        .unwrap();
    let wired_at = store
        .get(org_a, target)
        .await
        .unwrap()
        .unwrap()
        .first_ping_at;
    assert!(wired_at.is_some());

    // Freeze the anchor two hours back: after a deletion window this is stale
    // enough that an un-armed monitor would false-Down on the first eval.
    sqlx::query(
        "UPDATE heartbeat_monitors \
         SET armed_at = now() - INTERVAL '2 hours', last_ping_at = now() - INTERVAL '2 hours' \
         WHERE target_id = $1",
    )
    .bind(target)
    .execute(&pool)
    .await
    .unwrap();

    soft_delete_org(&pool, org_a, user_a).await.unwrap();
    let outcome = restore_org(
        &pool,
        org_a,
        user_a,
        30,
        &common::plan_for(&pool, org_a).await,
    )
    .await
    .unwrap();
    assert!(matches!(outcome, RestoreOutcome::Restored(_)));

    let hb = store.get(org_a, target).await.unwrap().expect("row");
    let arm_age = chrono::Utc::now().signed_duration_since(hb.armed_at);
    assert!(
        arm_age.num_seconds() < 60,
        "restore must re-arm; armed_at age {}s",
        arm_age.num_seconds()
    );
    // The re-arm is not a fabricated ping: real ping history stays put.
    let ping = hb.last_ping_at.expect("backdated ping");
    assert!(ping < chrono::Utc::now() - chrono::Duration::minutes(30));
    assert_eq!(
        hb.ping_state().success_at,
        hb.armed_at,
        "fresh arm point wins the anchor"
    );
    assert_eq!(
        hb.first_ping_at, wired_at,
        "a restore re-arms; it must not unwire a job that has already reported"
    );

    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL; run via DATABASE_URL=... cargo test -- --ignored"]
async fn cross_tenant_row_insert_is_blocked_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b, user_a, user_b) = two_orgs(&pool, "hb-xtenant").await;
    let target = make_heartbeat_target(&pool, org_a, "t", true).await;

    // A raw insert binding org_a's target under org_b trips the org-match
    // trigger, so a request-supplied target_id can't cross tenants.
    let res = sqlx::query(
        "INSERT INTO heartbeat_monitors (target_id, org_id, token_hash, token_enc) \
         VALUES ($1, $2, 'x-hash', 'x-enc')",
    )
    .bind(target)
    .bind(org_b.0)
    .execute(&pool)
    .await;
    assert!(
        res.is_err(),
        "cross-tenant heartbeat insert must be rejected"
    );

    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL; run via DATABASE_URL=... cargo test -- --ignored"]
async fn ensure_after_ping_preserves_state_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b, user_a, user_b) = two_orgs(&pool, "hb-keep").await;
    let store = PgHeartbeatStore::new(pool.clone(), None);
    let target = make_heartbeat_target(&pool, org_a, "keep", true).await;
    let token = store
        .ensure(org_a, target)
        .await
        .unwrap()
        .unwrap()
        .token
        .unwrap();
    let at = store
        .record_signal_by_token(&token, PingSignal::Success, None)
        .await
        .unwrap()
        .expect("ping records")
        .at;

    // A re-save hits the existence check, so it neither re-mints the token nor
    // clobbers the recorded ping.
    let again = store.ensure(org_a, target).await.unwrap().expect("row");
    assert_eq!(again.token.as_deref(), Some(token.as_str()));
    assert_eq!(again.last_ping_at, Some(at));

    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}

/// A run is timed against the row as it stood *before* the update, which only
/// the CTE snapshot sees. Pinned against real Postgres: it lives in the SQL.
#[tokio::test]
#[ignore = "requires DATABASE_URL; run via DATABASE_URL=... cargo test -- --ignored"]
async fn signals_close_the_run_they_opened_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b, user_a, user_b) = two_orgs(&pool, "hb-sig").await;
    let store = PgHeartbeatStore::new(pool.clone(), None);
    let target = make_heartbeat_target(&pool, org_a, "nightly", true).await;
    let token = store
        .ensure(org_a, target)
        .await
        .unwrap()
        .unwrap()
        .token
        .unwrap();

    let started = store
        .record_signal_by_token(&token, PingSignal::Start, None)
        .await
        .unwrap()
        .expect("start records");
    assert_eq!(started.org_id, org_a);
    assert!(started.run_ms.is_none(), "a start closes nothing");
    assert!(started.state.run_open_since().is_some());

    let failed = store
        .record_signal_by_token(&token, PingSignal::Fail, Some(137))
        .await
        .unwrap()
        .expect("fail records");
    assert!(failed.run_ms.is_some(), "the fail timed the open run");
    assert_eq!(failed.state.failing().and_then(|f| f.exit_code), Some(137));
    assert!(failed.state.run_open_since().is_none());

    let recovered = store
        .record_signal_by_token(&token, PingSignal::Success, None)
        .await
        .unwrap()
        .expect("success records");
    assert!(recovered.run_ms.is_none(), "nothing was open to time");
    assert!(recovered.state.failing().is_none(), "success clears it");

    // A start does not advance the silence anchor: it opens a run, it does not
    // report one.
    let row = store.get(org_a, target).await.unwrap().expect("row");
    assert_eq!(row.last_ping_at, Some(recovered.at));
    assert!(row.last_start_at.unwrap() < recovered.at);
    assert_eq!(
        row.last_exit_code,
        Some(137),
        "the exit code outlives the fail"
    );

    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}

/// The ping handler writes the first result off these two flags, so the SQL
/// that computes them is what keeps a freshly-wired job from looking dead for
/// a registry refresh plus an interval.
#[tokio::test]
#[ignore = "requires DATABASE_URL; run via DATABASE_URL=... cargo test -- --ignored"]
async fn accepted_ping_reports_the_wiring_ping_once_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b, user_a, user_b) = two_orgs(&pool, "hb-first").await;
    let store = PgHeartbeatStore::new(pool.clone(), None);

    let live = make_heartbeat_target(&pool, org_a, "nightly", true).await;
    let token = store
        .ensure(org_a, live)
        .await
        .unwrap()
        .expect("row")
        .token
        .expect("plaintext without cipher");

    let wiring = store
        .record_signal_by_token(&token, PingSignal::Success, None)
        .await
        .unwrap()
        .expect("token records");
    assert!(wiring.first, "the ping that ends the pending wait");
    assert!(wiring.enabled);

    let routine = store
        .record_signal_by_token(&token, PingSignal::Success, None)
        .await
        .unwrap()
        .expect("token records");
    assert!(!routine.first, "only the wiring ping reports first");

    // A start counts as wiring too: it un-pends the monitor just the same.
    let paused = make_heartbeat_target(&pool, org_a, "paused", false).await;
    let paused_token = store
        .ensure(org_a, paused)
        .await
        .unwrap()
        .expect("row")
        .token
        .expect("plaintext without cipher");
    let on_paused = store
        .record_signal_by_token(&paused_token, PingSignal::Start, None)
        .await
        .unwrap()
        .expect("token records");
    assert!(on_paused.first);
    assert!(
        !on_paused.enabled,
        "a paused monitor accepts the ping and reports no health"
    );

    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}

/// The page promises the schedule starts at the first ping. A monitor created
/// weeks before the job was wired must therefore not be judged on the silence
/// it sat in, whichever signal breaks it.
#[tokio::test]
#[ignore = "requires DATABASE_URL; run via DATABASE_URL=... cargo test -- --ignored"]
async fn the_wiring_ping_starts_the_schedule_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b, user_a, user_b) = two_orgs(&pool, "hb-arm").await;
    let store = PgHeartbeatStore::new(pool.clone(), None);

    let announced = make_heartbeat_target(&pool, org_a, "announced", true).await;
    let announced_token = token_of(&store, org_a, announced).await;
    backdate_arm(&pool, announced).await;
    let accepted = store
        .record_signal_by_token(&announced_token, PingSignal::Start, None)
        .await
        .unwrap()
        .expect("token records");
    assert!(
        accepted.state.success_at >= accepted.at - chrono::Duration::seconds(5),
        "a start that wires the monitor arms it: {:?} vs ping at {:?}",
        accepted.state.success_at,
        accepted.at
    );
    let read = store.get(org_a, announced).await.unwrap().expect("row");
    assert!(read.last_ping_at.is_none(), "a start is not a success");
    assert!(
        read.ping_state().failing().is_none(),
        "three weeks unwired is not a failed run"
    );

    // A first ping that reports failure keeps its verdict: arming it here
    // would clear the failure the same statement recorded.
    let broken = make_heartbeat_target(&pool, org_a, "broken", true).await;
    let broken_token = token_of(&store, org_a, broken).await;
    backdate_arm(&pool, broken).await;
    let failed = store
        .record_signal_by_token(&broken_token, PingSignal::Fail, Some(1))
        .await
        .unwrap()
        .expect("token records");
    assert!(
        failed.state.failing().is_some(),
        "the job said it failed on its first run"
    );

    // A later re-arm is unaffected by any of this.
    let routine = store
        .record_signal_by_token(&broken_token, PingSignal::Success, None)
        .await
        .unwrap()
        .expect("token records");
    assert!(routine.state.failing().is_none(), "a success clears it");

    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}

/// Removing a membership clears the monitor's owner with it, so the reminder
/// reaches the org rather than the person who left.
#[tokio::test]
#[ignore = "requires DATABASE_URL; run via DATABASE_URL=... cargo test -- --ignored"]
async fn the_nudge_reaches_the_org_after_the_owner_leaves_live_pg() {
    let Some((pool, db)) = isolated_pool("hb_nudge_left").await else {
        return;
    };
    let (org_a, _org_b, user_a, _user_b) = two_orgs(&pool, "hb-nudge-left").await;
    let store = PgHeartbeatStore::new(pool.clone(), None);
    let leaver = make_user(&pool, "hb-nudge-leaver").await;
    join_org(&pool, org_a, leaver).await;
    let leaver_email = email_of(&pool, leaver).await;
    let org_owner_email = email_of(&pool, user_a).await;

    let target = make_heartbeat_target(&pool, org_a, "left-behind", true).await;
    store.ensure(org_a, target).await.unwrap();
    sqlx::query("UPDATE targets SET owner_user_id = $2 WHERE id = $1")
        .bind(target)
        .bind(leaver.0)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE heartbeat_monitors SET created_at = now() - INTERVAL '5 days'")
        .execute(&pool)
        .await
        .unwrap();

    let outcome = uptimepage::storage::orgs::remove_member(&pool, org_a, user_a, leaver)
        .await
        .unwrap();
    let owner_after: (Option<Uuid>,) =
        sqlx::query_as("SELECT owner_user_id FROM targets WHERE id = $1")
            .bind(target)
            .fetch_one(&pool)
            .await
            .unwrap();

    let (sent, sender) = run_nudge(&pool).await;
    let to = sender.sent()[0].to.address.clone();
    common::drop_test_db(&db).await;

    assert_eq!(outcome, uptimepage::storage::orgs::RemoveOutcome::Removed);
    assert_eq!(owner_after.0, None, "the owner left with the membership");
    assert_eq!(sent, 1);
    assert_ne!(to, leaver_email, "they left this org");
    assert_eq!(to, org_owner_email, "the org's owner instead");
}

/// A stamp that fails leaves the rest of the batch to be swept.
#[tokio::test]
#[ignore = "requires DATABASE_URL; run via DATABASE_URL=... cargo test -- --ignored"]
async fn a_lost_stamp_does_not_strand_the_rest_of_the_sweep_live_pg() {
    let Some((pool, db)) = isolated_pool("hb_nudge_stamp").await else {
        return;
    };
    let (org_a, _org_b, _user_a, _user_b) = two_orgs(&pool, "hb-nudge-stamp").await;
    let store = PgHeartbeatStore::new(pool.clone(), None);
    let first = make_heartbeat_target(&pool, org_a, "aaa-first", true).await;
    let second = make_heartbeat_target(&pool, org_a, "bbb-second", true).await;
    for id in [first, second] {
        store.ensure(org_a, id).await.unwrap();
    }
    // Ordered by created_at, so `first` is swept first and is the one that fails.
    sqlx::query(
        "UPDATE heartbeat_monitors \
            SET created_at = now() - CASE WHEN target_id = $1 \
                                          THEN INTERVAL '6 days' ELSE INTERVAL '5 days' END",
    )
    .bind(first)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(&format!(
        "CREATE FUNCTION refuse_stamp() RETURNS trigger AS $$ \
         BEGIN RAISE EXCEPTION 'stamp refused'; END $$ LANGUAGE plpgsql; \
         CREATE TRIGGER refuse_stamp BEFORE UPDATE OF nudged_at ON heartbeat_monitors \
         FOR EACH ROW WHEN (NEW.target_id = '{first}') EXECUTE FUNCTION refuse_stamp();"
    ))
    .execute(&pool)
    .await
    .unwrap();

    let (sent, sender) = run_nudge(&pool).await;
    let mailed = sender.sent().len();
    let first_stamp = nudged_at(&pool, first).await;
    let second_stamp = nudged_at(&pool, second).await;

    // Stamp writable again: only the unstamped one is still eligible, and it
    // gets the duplicate the lost stamp bought.
    sqlx::raw_sql("DROP TRIGGER refuse_stamp ON heartbeat_monitors; DROP FUNCTION refuse_stamp();")
        .execute(&pool)
        .await
        .unwrap();
    let (resent, _) = run_nudge(&pool).await;
    let first_retry_stamp = nudged_at(&pool, first).await;
    common::drop_test_db(&db).await;

    assert_eq!(sent, 2, "both mails went out");
    assert_eq!(mailed, 2);
    assert!(first_stamp.is_none(), "the stamp is what failed");
    assert!(
        second_stamp.is_some(),
        "the monitor behind it still got stamped"
    );
    assert_eq!(resent, 1);
    assert!(first_retry_stamp.is_some());
}

async fn nudged_at(pool: &sqlx::PgPool, target: Uuid) -> Option<chrono::DateTime<chrono::Utc>> {
    let row: (Option<chrono::DateTime<chrono::Utc>>,) =
        sqlx::query_as("SELECT nudged_at FROM heartbeat_monitors WHERE target_id = $1")
            .bind(target)
            .fetch_one(pool)
            .await
            .unwrap();
    row.0
}

/// A store that reads but refuses writes would re-send the whole batch forever.
#[tokio::test]
#[ignore = "requires DATABASE_URL; run via DATABASE_URL=... cargo test -- --ignored"]
async fn a_store_that_refuses_every_stamp_stops_the_sweep_live_pg() {
    use uptimepage::jobs::heartbeat_nudge::STAMP_FAILURE_LIMIT;

    let Some((pool, db)) = isolated_pool("hb_nudge_wedge").await else {
        return;
    };
    let (org_a, _org_b, _user_a, _user_b) = two_orgs(&pool, "hb-nudge-wedge").await;
    let store = PgHeartbeatStore::new(pool.clone(), None);
    const POPULATION: usize = 5;
    assert!(
        (STAMP_FAILURE_LIMIT as usize) < POPULATION,
        "the sweep must run out of patience before it runs out of monitors"
    );
    for n in 0..POPULATION {
        let id = make_heartbeat_target(&pool, org_a, &format!("job-{n}"), true).await;
        store.ensure(org_a, id).await.unwrap();
    }
    sqlx::query("UPDATE heartbeat_monitors SET created_at = now() - INTERVAL '5 days'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(
        "CREATE FUNCTION refuse_stamp() RETURNS trigger AS $$ \
         BEGIN RAISE EXCEPTION 'stamp refused'; END $$ LANGUAGE plpgsql; \
         CREATE TRIGGER refuse_stamp BEFORE UPDATE OF nudged_at ON heartbeat_monitors \
         FOR EACH ROW EXECUTE FUNCTION refuse_stamp();",
    )
    .execute(&pool)
    .await
    .unwrap();

    let (sent, _) = run_nudge(&pool).await;
    common::drop_test_db(&db).await;

    assert_eq!(
        sent, STAMP_FAILURE_LIMIT as u64,
        "the sweep stops instead of mailing everyone again next tick"
    );
    assert!(sent < POPULATION as u64);
}

/// Wired long after somebody clicked save.
async fn backdate_arm(pool: &sqlx::PgPool, target: Uuid) {
    sqlx::query(
        "UPDATE heartbeat_monitors SET armed_at = now() - interval '21 days' WHERE target_id = $1",
    )
    .bind(target)
    .execute(pool)
    .await
    .unwrap();
}

async fn token_of(store: &PgHeartbeatStore, org: OrgId, target: Uuid) -> String {
    store
        .ensure(org, target)
        .await
        .unwrap()
        .expect("row")
        .token
        .expect("plaintext without cipher")
}

#[tokio::test]
#[ignore = "requires DATABASE_URL; run via DATABASE_URL=... cargo test -- --ignored"]
async fn rotation_keeps_the_row_and_overlaps_the_old_token_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b, user_a, user_b) = two_orgs(&pool, "hb-rot").await;
    let store = PgHeartbeatStore::new(pool.clone(), None);
    let target = make_heartbeat_target(&pool, org_a, "rotated", true).await;
    let old_token = token_of(&store, org_a, target).await;
    store
        .record_signal_by_token(&old_token, PingSignal::Success, None)
        .await
        .unwrap()
        .expect("wire the monitor up");
    let wired = store.get(org_a, target).await.unwrap().expect("row");

    assert!(
        store
            .rotate(org_b, target, false, Some(user_b))
            .await
            .unwrap()
            .is_none()
    );

    let rotated = store
        .rotate(org_a, target, false, Some(user_a))
        .await
        .unwrap()
        .expect("row");
    let new_token = rotated.token.clone().expect("plaintext without cipher");
    assert_ne!(new_token, old_token);
    assert_eq!(rotated.first_ping_at, wired.first_ping_at);
    assert_eq!(rotated.armed_at, wired.armed_at, "rotation is not a re-arm");
    assert!(rotated.open_overlap().is_some());
    assert!(rotated.prev_token_last_used_at.is_none());

    let on_prev = store
        .record_signal_by_token(&old_token, PingSignal::Success, None)
        .await
        .unwrap()
        .expect("superseded token still records");
    let read = store.get(org_a, target).await.unwrap().expect("row");
    assert_eq!(read.prev_token_last_used_at, Some(on_prev.at));
    assert_eq!(read.last_ping_at, Some(on_prev.at));
    store
        .record_signal_by_token(&new_token, PingSignal::Success, None)
        .await
        .unwrap()
        .expect("new token records");

    let audits: Vec<(Option<Uuid>,)> = sqlx::query_as(
        "SELECT actor_id FROM org_audit_log \
         WHERE org_id = $1 AND action = 'heartbeat.token_rotated'",
    )
    .bind(org_a.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(audits.len(), 1, "one rotation, one audit row");
    assert_eq!(audits[0].0, Some(user_a.0));

    assert!(
        store
            .revoke_previous(org_a, target, Some(user_a))
            .await
            .unwrap()
    );
    assert!(
        store
            .record_signal_by_token(&old_token, PingSignal::Success, None)
            .await
            .unwrap()
            .is_none(),
        "revoked token 404s like an unknown one"
    );
    assert!(
        !store
            .revoke_previous(org_a, target, Some(user_a))
            .await
            .unwrap(),
        "nothing left to end"
    );

    let revokes: Vec<(Option<Uuid>,)> = sqlx::query_as(
        "SELECT actor_id FROM org_audit_log \
         WHERE org_id = $1 AND action = 'heartbeat.prev_token_revoked'",
    )
    .bind(org_a.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        revokes.len(),
        1,
        "one overlap ended, and the no-op call audits nothing"
    );
    assert_eq!(revokes[0].0, Some(user_a.0));

    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL; run via DATABASE_URL=... cargo test -- --ignored"]
async fn overlap_expiry_is_by_timestamp_and_the_sweep_only_tidies_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b, user_a, user_b) = two_orgs(&pool, "hb-exp").await;
    let store = PgHeartbeatStore::new(pool.clone(), None);
    let target = make_heartbeat_target(&pool, org_a, "expiring", true).await;
    let old_token = token_of(&store, org_a, target).await;
    let rotated = store
        .rotate(org_a, target, false, Some(user_a))
        .await
        .unwrap()
        .expect("row");

    sqlx::query(
        "UPDATE heartbeat_monitors \
         SET prev_token_expires_at = now() - interval '1 minute' WHERE target_id = $1",
    )
    .bind(target)
    .execute(&pool)
    .await
    .unwrap();

    assert!(
        store
            .record_signal_by_token(&old_token, PingSignal::Success, None)
            .await
            .unwrap()
            .is_none()
    );
    let read = store.get(org_a, target).await.unwrap().expect("row");
    assert!(read.open_overlap().is_none());

    assert!(
        uptimepage::storage::heartbeats::purge_expired_prev_tokens(&pool)
            .await
            .unwrap()
            >= 1
    );
    let bare: (Option<String>, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
        "SELECT prev_token_hash, prev_token_expires_at FROM heartbeat_monitors \
         WHERE target_id = $1",
    )
    .bind(target)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(bare, (None, None));

    let current = rotated.token.expect("plaintext without cipher");
    let re = store
        .rotate(org_a, target, true, Some(user_a))
        .await
        .unwrap()
        .expect("row");
    assert!(re.open_overlap().is_none());
    assert!(
        store
            .record_signal_by_token(&current, PingSignal::Success, None)
            .await
            .unwrap()
            .is_none()
    );

    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}

/// Rotating a leaked secret must not cost the record that proves the outage
/// happened, which is the whole reason this exists instead of delete-recreate.
#[tokio::test]
#[ignore = "requires DATABASE_URL; run via DATABASE_URL=... cargo test -- --ignored"]
async fn rotation_keeps_the_incidents_shares_and_page_binding_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b, user_a, user_b) = two_orgs(&pool, "hb-keep").await;
    let store = PgHeartbeatStore::new(pool.clone(), None);
    let target = make_heartbeat_target(&pool, org_a, "wired-up", true).await;
    let old_token = token_of(&store, org_a, target).await;
    store
        .record_signal_by_token(&old_token, PingSignal::Success, None)
        .await
        .unwrap()
        .expect("wire the monitor up");

    let (incident,): (Uuid,) = sqlx::query_as(
        "INSERT INTO incidents (org_id, target_id, started_at, status_at_start, origin, visibility) \
         VALUES ($1, $2, now() - INTERVAL '2 hours', 'down', 'monitor', 'public') RETURNING id",
    )
    .bind(org_a.0)
    .bind(target)
    .fetch_one(&pool)
    .await
    .unwrap();

    let share_hash = format!("share-{}", Uuid::new_v4().simple());
    let (share,): (Uuid,) = sqlx::query_as(
        "INSERT INTO monitor_shares (org_id, target_id, token_hash, token_enc, created_by) \
         VALUES ($1, $2, $3, 'sealed', $4) RETURNING id",
    )
    .bind(org_a.0)
    .bind(target)
    .bind(&share_hash)
    .bind(user_a.0)
    .fetch_one(&pool)
    .await
    .unwrap();

    let (page,): (Uuid,) = sqlx::query_as(
        "INSERT INTO status_pages (org_id, slug, name) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(org_a.0)
    .bind(unique_slug("hb-keep"))
    .bind("Keep")
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO status_page_components (org_id, status_page_id, target_id, public_name) \
         VALUES ($1, $2, $3, 'Nightly backup')",
    )
    .bind(org_a.0)
    .bind(page)
    .bind(target)
    .execute(&pool)
    .await
    .unwrap();

    let before = store.get(org_a, target).await.unwrap().expect("row");
    let rotated = store
        .rotate(org_a, target, false, Some(user_a))
        .await
        .unwrap()
        .expect("row");
    assert_ne!(
        rotated.token.clone().expect("plaintext without cipher"),
        old_token
    );
    assert_eq!(rotated.first_ping_at, before.first_ping_at);
    assert_eq!(rotated.armed_at, before.armed_at);

    let (open_incidents,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM incidents WHERE id = $1 AND target_id = $2")
            .bind(incident)
            .bind(target)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(open_incidents, 1, "the outage record survives the rotation");

    let (live_hash, revoked): (String, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT token_hash, revoked_at FROM monitor_shares WHERE id = $1")
            .bind(share)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        live_hash, share_hash,
        "share links rotate on their own clock"
    );
    assert!(revoked.is_none());

    let (bound,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM status_page_components \
         WHERE status_page_id = $1 AND target_id = $2",
    )
    .bind(page)
    .bind(target)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(bound, 1, "the monitor keeps its place on the status page");

    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}
