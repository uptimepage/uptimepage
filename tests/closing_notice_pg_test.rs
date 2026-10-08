//! The closing notice an incident's end owes its responders, against the real
//! SQL: every close records it in the statement that ends the incident, a
//! reopen withdraws it, and the claim keeps two senders (a signal and a sweep,
//! or two engine instances during a deploy) from both sending it, down to each
//! channel's row. Also the escalation step's claim, which the engine checks
//! before paging so a step an ack, resolve or reopen overtook pages nobody.
//!
//! Each test runs in its own database: the sweep's scan is cross-org.

use crate::common;

use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uptimepage::domain::{
    CheckSpec, CheckStatus, ExpectedStatus, NewIncidentNotification, NewTarget, NotificationReason,
    NotificationStatus, OrgId, UserId, WriteSource,
};
use uptimepage::public_status::{IncidentStore, NewOpenIncident, PgIncidentStore};
use uptimepage::storage::{
    Actor, IncidentOpsStore, PgIncidentOpsStore, PostgresTargetStore, TargetStore,
    create_org_with_owner,
};
use url::Url;
use uuid::Uuid;

use crate::common::MIGRATOR;

const LEASE_SECS: i64 = 60;
const WINDOW_SECS: i64 = 86_400;

struct Fixture {
    pool: PgPool,
    db: String,
    org: OrgId,
    user: UserId,
    targets: PostgresTargetStore,
    ops: PgIncidentOpsStore,
}

async fn fixture(prefix: &str) -> Option<Fixture> {
    let (url, db) = common::fresh_test_db(prefix).await?;
    let pool = common::open_test_pool(&url).await;
    MIGRATOR.run(&pool).await.unwrap();
    let user = common::make_user(&pool, prefix).await;
    let org = create_org_with_owner(&pool, user, &common::unique_slug(prefix), "Co")
        .await
        .unwrap()
        .unwrap()
        .id;
    Some(Fixture {
        targets: PostgresTargetStore::from_pool(pool.clone(), None),
        ops: PgIncidentOpsStore::new(pool.clone()),
        pool,
        db,
        org,
        user,
    })
}

impl Fixture {
    async fn monitor(&self) -> Uuid {
        let new = NewTarget {
            name: "api".into(),
            check: CheckSpec::Http(common::default_http_check(
                Url::parse("https://example.test/healthz").unwrap(),
                ExpectedStatus::Exact(200),
            )),
            interval: Duration::from_secs(60),
            enabled: true,
            tags: vec![],
            alerts: Default::default(),
            region_policy: Default::default(),
            alert_confirmations: 2,
            notify_recovery: true,
            renotify_interval_secs: 3600,
            recovery_period_secs: 0,
            group_name: None,
            owner_user_id: None,
            regions: None,
        };
        self.targets
            .create(self.org, new, WriteSource::Ui, i64::MAX, i64::MAX)
            .await
            .unwrap()
            .id
    }

    async fn open_incident(&self, target: Uuid, origin: &str) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO incidents (org_id, target_id, started_at, status_at_start, origin) \
             VALUES ($1, $2, now() - interval '30 minutes', 'down', $3) RETURNING id",
        )
        .bind(self.org.0)
        .bind(target)
        .bind(origin)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    /// A monitor whose incident a person resolved, owing its notice.
    async fn resolved(&self) -> Uuid {
        let target = self.monitor().await;
        let id = self.open_incident(target, "monitor").await;
        self.ops
            .resolve(self.org, id, Actor::User(self.user), None)
            .await
            .unwrap();
        id
    }

    async fn notice_at(&self, id: Uuid) -> Option<DateTime<Utc>> {
        sqlx::query_scalar("SELECT closing_notice_at FROM incidents WHERE id = $1 AND org_id = $2")
            .bind(id)
            .bind(self.org.0)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn ended_at(&self, id: Uuid) -> Option<DateTime<Utc>> {
        sqlx::query_scalar("SELECT ended_at FROM incidents WHERE id = $1 AND org_id = $2")
            .bind(id)
            .bind(self.org.0)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn rows(&self, id: Uuid) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM incident_notifications WHERE incident_id = $1 AND org_id = $2",
        )
        .bind(id)
        .bind(self.org.0)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    fn row(&self, id: Uuid, reason: NotificationReason) -> NewIncidentNotification {
        NewIncidentNotification {
            org: self.org,
            incident_id: id,
            escalation_level: Some(0),
            target_user_id: None,
            channel_id: None,
            transport: "webhook".into(),
            reason,
            status: NotificationStatus::Queued,
            attempt: 1,
            error: None,
            sent_at: None,
            episode: 0,
        }
    }

    /// Move the notice (and, with `ended`, the incident's end) into the past.
    async fn backdate(&self, id: Uuid, secs: i64, ended: bool) {
        sqlx::query(
            "UPDATE incidents \
             SET closing_notice_at = closing_notice_at - make_interval(secs => $3::double precision), \
                 ended_at = CASE WHEN $4 \
                     THEN ended_at - make_interval(secs => $3::double precision) \
                     ELSE ended_at END \
             WHERE id = $1 AND org_id = $2",
        )
        .bind(id)
        .bind(self.org.0)
        .bind(secs as f64)
        .bind(ended)
        .execute(&self.pool)
        .await
        .unwrap();
    }
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn every_close_owes_its_notice_and_a_reopen_withdraws_it_pg() {
    let Some(f) = fixture("closenotice").await else {
        return;
    };

    // The writer's recovery dates the notice by its commit: the check that
    // recovered can be minutes older.
    let writer = PgIncidentStore::new(f.pool.clone());
    let recovering = f.monitor().await;
    let recovered = writer
        .insert_open(
            f.org,
            NewOpenIncident {
                target_id: recovering,
                started_at: Utc::now() - chrono::Duration::minutes(10),
                status_at_start: CheckStatus::Down,
                check_count: 2,
                error_sample: None,
                region: None,
                regions_down: vec![],
                regions_up: vec![],
            },
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(f.notice_at(recovered).await, None, "open owes nothing");
    let checked_at = Utc::now() - chrono::Duration::minutes(3);
    assert!(writer.close(f.org, recovered, checked_at).await.unwrap());
    let owed = f.notice_at(recovered).await.expect("the recovery owes it");
    assert!(owed > f.ended_at(recovered).await.unwrap());

    let resolved = f.resolved().await;
    assert!(f.notice_at(resolved).await.is_some());
    f.ops
        .reopen(f.org, resolved, Actor::User(f.user), None)
        .await
        .unwrap();
    assert_eq!(f.notice_at(resolved).await, None, "a reopen withdraws it");

    let auto = f.open_incident(f.monitor().await, "monitor").await;
    f.ops.auto_resolve(f.org, auto).await.unwrap();
    assert!(f.notice_at(auto).await.is_some());

    // A delete owes the notice only for what it closed: an incident already
    // over settled its own, and a declared one stays open.
    let deleted = f.monitor().await;
    let earlier = f.open_incident(deleted, "monitor").await;
    f.ops
        .resolve(f.org, earlier, Actor::User(f.user), None)
        .await
        .unwrap();
    let earlier_owed = f.notice_at(earlier).await;
    let open = f.open_incident(deleted, "monitor").await;
    let declared = f.open_incident(deleted, "manual").await;
    assert!(
        f.targets
            .delete(f.org, deleted, Some(f.user))
            .await
            .unwrap()
    );
    assert!(f.notice_at(open).await.is_some(), "the delete closed it");
    assert_eq!(f.notice_at(earlier).await, earlier_owed, "untouched");
    assert_eq!(f.notice_at(declared).await, None, "still open");
    f.ops
        .resolve(f.org, declared, Actor::User(f.user), None)
        .await
        .unwrap();
    assert!(f.notice_at(declared).await.is_some());

    common::drop_test_db(&f.db).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_notice_is_held_by_one_sender_at_a_time_pg() {
    let Some(f) = fixture("closeclaim").await else {
        return;
    };
    let id = f.resolved().await;

    let other = create_org_with_owner(
        &f.pool,
        common::make_user(&f.pool, "closeclaim").await,
        &common::unique_slug("closeclaim"),
        "Other",
    )
    .await
    .unwrap()
    .unwrap()
    .id;
    assert_eq!(
        f.ops
            .claim_closing_notice(other, id, LEASE_SECS)
            .await
            .unwrap(),
        None,
        "another org cannot take it"
    );

    let claim = f
        .ops
        .claim_closing_notice(f.org, id, LEASE_SECS)
        .await
        .unwrap()
        .expect("owed");
    assert_eq!(
        f.ops
            .claim_closing_notice(f.org, id, LEASE_SECS)
            .await
            .unwrap(),
        None
    );
    let due = f
        .ops
        .due_closing_notices(0, WINDOW_SECS, LEASE_SECS, 10)
        .await
        .unwrap();
    assert!(due.claimed.is_empty(), "a held notice is not due");

    f.ops.settle_closing_notice(other, id, claim).await.unwrap();
    assert!(f.notice_at(id).await.is_some(), "settled only by its org");
    f.ops.settle_closing_notice(f.org, id, claim).await.unwrap();
    assert_eq!(f.notice_at(id).await, None);
    assert_eq!(
        f.ops
            .claim_closing_notice(f.org, id, LEASE_SECS)
            .await
            .unwrap(),
        None,
        "sent once"
    );

    common::drop_test_db(&f.db).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_sweep_waits_out_the_grace_and_withdraws_what_ended_outside_the_window_pg() {
    let Some(f) = fixture("closesweep").await else {
        return;
    };
    let fresh = f.resolved().await;
    let lost = f.resolved().await;
    f.backdate(lost, 60, false).await;
    let stale = f.resolved().await;
    f.backdate(stale, 2 * WINDOW_SECS, true).await;

    let due = f
        .ops
        .due_closing_notices(30, WINDOW_SECS, LEASE_SECS, 10)
        .await
        .unwrap();
    let claimed: Vec<Uuid> = due.claimed.iter().map(|n| n.id).collect();
    assert_eq!(claimed, vec![lost]);
    assert_eq!(due.claimed[0].org, f.org);
    assert_eq!(due.expired, 1);
    assert_eq!(f.notice_at(stale).await, None, "withdrawn unsent");
    assert!(
        f.notice_at(fresh).await.is_some(),
        "its signal may still land"
    );
    let lapsed = due.claimed[0].claim;
    assert!(lapsed > Utc::now());

    let again = f
        .ops
        .due_closing_notices(30, WINDOW_SECS, LEASE_SECS, 10)
        .await
        .unwrap();
    assert!(again.claimed.is_empty() && again.expired == 0, "leased");

    // The sender died: its lease runs out and the notice falls due again. Its
    // late settle must not clear what the next sender holds.
    f.backdate(lost, 2 * LEASE_SECS, false).await;
    let retaken = f
        .ops
        .due_closing_notices(30, WINDOW_SECS, LEASE_SECS, 10)
        .await
        .unwrap();
    assert_eq!(retaken.claimed.len(), 1);
    f.ops
        .settle_closing_notice(f.org, lost, lapsed)
        .await
        .unwrap();
    assert!(f.notice_at(lost).await.is_some());
    f.ops
        .settle_closing_notice(f.org, lost, retaken.claimed[0].claim)
        .await
        .unwrap();
    assert_eq!(f.notice_at(lost).await, None);

    common::drop_test_db(&f.db).await;
}

/// Two engine instances sweeping at once split the owed notices between them
/// and never take the same one.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn two_sweeps_never_take_the_same_notice_pg() {
    let Some(f) = fixture("closeskip").await else {
        return;
    };
    let mut owed = Vec::new();
    for _ in 0..12 {
        let id = f.resolved().await;
        f.backdate(id, 60, false).await;
        owed.push(id);
    }
    let second = PgIncidentOpsStore::new(f.pool.clone());
    let (a, b) = tokio::join!(
        f.ops.due_closing_notices(30, WINDOW_SECS, LEASE_SECS, 12),
        second.due_closing_notices(30, WINDOW_SECS, LEASE_SECS, 12),
    );
    let mut taken: Vec<Uuid> = a
        .unwrap()
        .claimed
        .into_iter()
        .chain(b.unwrap().claimed)
        .map(|n| n.id)
        .collect();
    taken.sort();
    owed.sort();
    assert_eq!(taken, owed, "each taken exactly once");

    common::drop_test_db(&f.db).await;
}

/// Each row of the notice renews the claim it went out under. A sender whose
/// lease lapsed and was taken over records nothing, nor does one whose notice
/// a reopen withdrew, nor another org.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_closing_row_is_recorded_only_under_the_claim_that_holds_the_notice_pg() {
    let Some(f) = fixture("closefence").await else {
        return;
    };
    let id = f.resolved().await;
    let resolved = f.row(id, NotificationReason::Resolved);
    let claim = f
        .ops
        .claim_closing_notice(f.org, id, LEASE_SECS)
        .await
        .unwrap()
        .expect("owed");
    let (_, renewed) = f
        .ops
        .record_closing_notification(resolved.clone(), claim, LEASE_SECS)
        .await
        .unwrap()
        .expect("held");
    assert_eq!(f.notice_at(id).await, Some(renewed));

    let mut foreign = resolved.clone();
    foreign.org = OrgId(Uuid::now_v7());
    assert_eq!(
        f.ops
            .record_closing_notification(foreign, renewed, LEASE_SECS)
            .await
            .unwrap(),
        None,
        "another org cannot record under it"
    );

    f.backdate(id, 2 * LEASE_SECS, false).await;
    let taken = f
        .ops
        .due_closing_notices(0, WINDOW_SECS, LEASE_SECS, 10)
        .await
        .unwrap();
    assert_eq!(taken.claimed.len(), 1);
    assert_eq!(
        f.ops
            .record_closing_notification(resolved.clone(), renewed, LEASE_SECS)
            .await
            .unwrap(),
        None
    );
    assert_eq!(f.rows(id).await, 1, "the lapsed sender recorded nothing");
    let (_, held) = f
        .ops
        .record_closing_notification(resolved.clone(), taken.claimed[0].claim, LEASE_SECS)
        .await
        .unwrap()
        .expect("the new holder's");
    assert_eq!(f.rows(id).await, 2);

    f.ops
        .reopen(f.org, id, Actor::User(f.user), None)
        .await
        .unwrap();
    assert_eq!(
        f.ops
            .record_closing_notification(resolved, held, LEASE_SECS)
            .await
            .unwrap(),
        None
    );
    assert_eq!(f.rows(id).await, 2);

    common::drop_test_db(&f.db).await;
}

/// A sender recording a row as its lease lapses races another instance taking
/// the notice over: one or the other wins, never both.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_takeover_and_a_late_row_never_both_win_pg() {
    let Some(f) = fixture("closerace").await else {
        return;
    };
    let mut held = Vec::new();
    for _ in 0..12 {
        let id = f.resolved().await;
        f.ops
            .claim_closing_notice(f.org, id, LEASE_SECS)
            .await
            .unwrap()
            .expect("owed");
        held.push(id);
    }
    // Lapsed by the clock, still the sender's own token.
    sqlx::query(
        "UPDATE incidents SET closing_notice_at = now() - interval '1 second' \
         WHERE org_id = $1 AND closing_notice_at IS NOT NULL",
    )
    .bind(f.org.0)
    .execute(&f.pool)
    .await
    .unwrap();
    let mut lapsed = Vec::new();
    for id in held {
        lapsed.push((id, f.notice_at(id).await.unwrap()));
    }

    let other = PgIncidentOpsStore::new(f.pool.clone());
    let late = futures::future::join_all(lapsed.iter().map(|(id, claim)| {
        f.ops.record_closing_notification(
            f.row(*id, NotificationReason::Resolved),
            *claim,
            LEASE_SECS,
        )
    }));
    let (late, taken) = tokio::join!(
        late,
        other.due_closing_notices(0, WINDOW_SECS, LEASE_SECS, 12)
    );
    let taken: Vec<Uuid> = taken.unwrap().claimed.into_iter().map(|n| n.id).collect();
    for ((id, _), recorded) in lapsed.iter().zip(late) {
        let recorded = recorded.unwrap().is_some();
        assert_ne!(recorded, taken.contains(id), "exactly one of them won");
        assert_eq!(f.rows(*id).await, i64::from(recorded));
    }

    common::drop_test_db(&f.db).await;
}

/// Each page keeps the episode it was decided for, whichever way it was
/// recorded.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_page_keeps_the_episode_it_was_sent_for_pg() {
    let Some(f) = fixture("closeepisode").await else {
        return;
    };
    let id = f.resolved().await;
    let claim = f
        .ops
        .claim_closing_notice(f.org, id, LEASE_SECS)
        .await
        .unwrap()
        .expect("owed");
    let mut closing = f.row(id, NotificationReason::Resolved);
    closing.episode = 2;
    f.ops
        .record_closing_notification(closing, claim, LEASE_SECS)
        .await
        .unwrap()
        .expect("held");
    let mut opened = f.row(id, NotificationReason::Opened);
    opened.episode = 3;
    f.ops.record_notification(opened).await.unwrap();

    let mut sent: Vec<(i64, NotificationReason)> = f
        .ops
        .notifications_for(f.org, id)
        .await
        .unwrap()
        .into_iter()
        .map(|n| (n.episode, n.reason))
        .collect();
    sent.sort_by_key(|(episode, _)| *episode);
    assert_eq!(
        sent,
        vec![
            (2, NotificationReason::Resolved),
            (3, NotificationReason::Opened)
        ]
    );

    common::drop_test_db(&f.db).await;
}

/// An escalation step comes back with the timer value it was claimed with,
/// the one the incident holds until an ack, a resolve or a new episode's
/// first page moves it, and the episode it was claimed in.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_claimed_escalation_step_carries_its_timer_and_episode_pg() {
    let Some(f) = fixture("escclaim").await else {
        return;
    };
    let id = f.open_incident(f.monitor().await, "monitor").await;
    f.ops
        .resolve(f.org, id, Actor::User(f.user), None)
        .await
        .unwrap();
    f.ops
        .reopen(f.org, id, Actor::User(f.user), None)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE incidents SET next_escalation_at = now() - interval '1 second' \
         WHERE id = $1 AND org_id = $2",
    )
    .bind(id)
    .bind(f.org.0)
    .execute(&f.pool)
    .await
    .unwrap();
    let timer = |f: &Fixture| {
        let (pool, org) = (f.pool.clone(), f.org);
        async move {
            sqlx::query_scalar::<_, Option<DateTime<Utc>>>(
                "SELECT next_escalation_at FROM incidents WHERE id = $1 AND org_id = $2",
            )
            .bind(id)
            .bind(org.0)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };

    let taken = f
        .ops
        .due_for_escalation(Utc::now(), 10, LEASE_SECS)
        .await
        .unwrap();
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].due.org, f.org);
    assert_eq!(timer(&f).await, Some(taken[0].claim));
    assert_eq!(taken[0].episode, 1);

    f.ops
        .acknowledge(f.org, id, Actor::User(f.user), None, None)
        .await
        .unwrap();
    assert_eq!(timer(&f).await, None);

    common::drop_test_db(&f.db).await;
}
