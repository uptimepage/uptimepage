//! Alert events against their source's incidents: open, repeat, raise, resolve
//! and its stale guard, pause and plan hold, the cap and its summary, the
//! unparsed incident, and two firings racing on one key.

use std::collections::BTreeMap;

use crate::common;
use chrono::{Duration, Utc};
use common::{make_user, unique_slug};
use sqlx::PgPool;
use uptimepage::domain::{
    AlertBinding, AlertGrouping, AlertSource, AlertSourceKind, AlertSourceUpdate, IncidentSeverity,
    NewAlertSource, NotificationReason, OrgId, TargetAlerts, WriteSource,
};
use uptimepage::intake::{AlertEvent, EventAction};
use uptimepage::storage::alert_delivery::Status;
use uptimepage::storage::alert_ingest::{AlertIngest, Ingested, MAX_OPEN_PER_SOURCE};
use uptimepage::storage::alert_sources::{AlertSourceStore, PgAlertSourceStore};
use uptimepage::storage::{IncidentOpsStore, create_org_with_owner};
use uuid::Uuid;

struct Fx {
    pool: PgPool,
    org: OrgId,
    source: AlertSource,
    sources: PgAlertSourceStore,
    ingest: AlertIngest,
}

async fn fx(prefix: &str) -> Option<Fx> {
    let pool = common::pg_pool_from_env().await?;
    let user = make_user(&pool, prefix).await;
    let org = create_org_with_owner(&pool, user, &unique_slug(prefix), "n")
        .await
        .unwrap()
        .unwrap()
        .id;
    let channel: Uuid = sqlx::query_scalar(
        "INSERT INTO notification_channels (org_id, name, kind, config, enabled) \
         VALUES ($1, 'ops', 'webhook', '{\"webhook\":{\"url\":\"http://x/\",\"headers\":{}}}'::jsonb, true) \
         RETURNING id",
    )
    .bind(org.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    let sources = PgAlertSourceStore::new(pool.clone(), None);
    let source = sources
        .create(
            org,
            NewAlertSource {
                name: "Prod Grafana".into(),
                kind: AlertSourceKind::Grafana,
                grouping: AlertGrouping::Group,
                escalation_policy_id: None,
                alerts: TargetAlerts(vec![AlertBinding {
                    channel_id: channel,
                }]),
                notify_recovery: true,
                renotify_interval_secs: 3600,
                signing_secret: None,
            },
            WriteSource::Api,
            Some(user),
            25,
        )
        .await
        .unwrap();
    Some(Fx {
        ingest: AlertIngest::new(pool.clone()),
        pool,
        org,
        source,
        sources,
    })
}

fn event(key: &str, action: EventAction) -> AlertEvent {
    AlertEvent {
        key: key.into(),
        action,
        title: "KubePodCrashLooping".into(),
        detail: Some("Pod restarted 5 times".into()),
        severity: IncidentSeverity::Major,
        firing: u32::from(action == EventAction::Fire) * 3,
        truncated: 0,
        may_open: true,
        at: None,
        labels: BTreeMap::from([("alertname".into(), "KubePodCrashLooping".into())]),
        annotations: BTreeMap::new(),
        links: Vec::new(),
    }
}

impl Fx {
    async fn apply(&self, e: &AlertEvent) -> uptimepage::storage::alert_ingest::Applied {
        self.ingest
            .apply(self.org, &self.source, e)
            .await
            .unwrap()
            .applied()
            .expect("applied under the grouping it was parsed with")
    }

    async fn open_incidents(&self) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM incidents \
             WHERE org_id = $1 AND alert_source_id = $2 AND ended_at IS NULL",
        )
        .bind(self.org.0)
        .bind(self.source.id)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    async fn row(&self, id: Uuid) -> (String, String, bool, i32) {
        sqlx::query_as(
            "SELECT i.state, i.severity, i.page_owed_at IS NOT NULL, a.repeat_count \
             FROM incidents i JOIN incident_alert_state a ON a.incident_id = i.id \
             WHERE i.id = $1 AND i.org_id = $2",
        )
        .bind(id)
        .bind(self.org.0)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    async fn pause(&self, enabled: bool) {
        self.sources
            .update(
                self.org,
                self.source.id,
                AlertSourceUpdate {
                    enabled: Some(enabled),
                    ..Default::default()
                },
                WriteSource::Api,
                None,
            )
            .await
            .unwrap();
    }
}

#[tokio::test]
#[ignore]
async fn a_firing_opens_once_repeats_raise_and_a_resolve_closes_pg() {
    let Some(f) = fx("ingestlife").await else {
        return;
    };
    let opened = f.apply(&event("g1", EventAction::Fire)).await;
    assert_eq!(opened.status, Status::Opened);
    let id = opened.incident.unwrap();
    assert_eq!(opened.signals, vec![(id, NotificationReason::Opened)]);
    let (state, severity, owed, _) = f.row(id).await;
    assert_eq!(
        (state.as_str(), severity.as_str(), owed),
        ("triggered", "major", true)
    );
    let message: String = sqlx::query_scalar(
        "SELECT message FROM incident_events WHERE incident_id = $1 AND org_id = $2 AND kind = 'triggered'",
    )
    .bind(id)
    .bind(f.org.0)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert!(
        message.contains("Prod Grafana"),
        "the timeline names the source: {message}"
    );

    let mut again = event("g1", EventAction::Fire);
    again.firing = 4;
    let repeat = f.apply(&again).await;
    assert_eq!((repeat.status, repeat.incident), (Status::Repeat, Some(id)));
    assert!(repeat.signals.is_empty(), "a repeat pages nobody");
    assert_eq!(f.open_incidents().await, 1);

    again.severity = IncidentSeverity::Critical;
    f.apply(&again).await;
    assert_eq!(f.row(id).await.1, "critical", "a worse severity is raised");

    let closed = f.apply(&event("g1", EventAction::Resolve)).await;
    assert_eq!(closed.status, Status::Resolved);
    assert_eq!(closed.signals, vec![(id, NotificationReason::Resolved)]);
    assert_eq!(f.row(id).await.0, "resolved");
    let again = f.apply(&event("g1", EventAction::Resolve)).await;
    assert_eq!(again.status, Status::Ignored, "nothing open");

    let reopened = f.apply(&event("g1", EventAction::Fire)).await;
    assert_eq!(reopened.status, Status::Opened);
    assert_ne!(
        reopened.incident,
        Some(id),
        "a firing after a close is a new incident"
    );
}

/// An alert starts at 10:00, resolves at 10:05 and fires again at 10:10 with
/// its start still 10:00. A delayed copy of the 10:05 resolve must not close
/// what opened at 10:10, since an earlier incident of the key already closed
/// at that time; one dated before the latest firing is stale too.
#[tokio::test]
#[ignore]
async fn an_old_resolve_leaves_a_newer_incident_open_pg() {
    let Some(f) = fx("ingeststale").await else {
        return;
    };
    let now = Utc::now();
    let started = now - Duration::minutes(10);
    let mut fire = event("g1", EventAction::Fire);
    fire.at = Some(started);
    let mut resolve = event("g1", EventAction::Resolve);
    resolve.at = Some(now - Duration::minutes(5));

    f.apply(&fire).await;
    assert_eq!(f.apply(&resolve).await.status, Status::Resolved);
    let again = f.apply(&fire).await.incident.unwrap();
    assert_eq!(
        f.apply(&resolve).await.status,
        Status::Ignored,
        "a late copy of a resolve already applied"
    );
    let mut older = event("g1", EventAction::Resolve);
    older.at = Some(now - Duration::minutes(7));
    assert_eq!(
        f.apply(&older).await.status,
        Status::Ignored,
        "older than a resolve already applied by more than the skew"
    );

    let mut later = event("g1", EventAction::Fire);
    later.at = Some(now - Duration::seconds(20));
    f.apply(&later).await;
    let mut before_firing = event("g1", EventAction::Resolve);
    before_firing.at = Some(now - Duration::seconds(90));
    assert_eq!(f.apply(&before_firing).await.status, Status::Ignored);
    assert_eq!(f.row(again).await.0, "triggered");

    let mut fresh = event("g1", EventAction::Resolve);
    fresh.at = Some(now);
    assert_eq!(f.apply(&fresh).await.status, Status::Resolved);
}

/// One cycle's recovery comes from a sender 30 seconds fast, the next cycle
/// from one on time. The second recovery is dated before the first, within
/// the skew allowed, and still ends its incident.
#[tokio::test]
#[ignore]
async fn a_recovery_from_a_slower_clock_still_closes_the_next_cycle_pg() {
    let Some(f) = fx("ingestclocks").await else {
        return;
    };
    let t = Utc::now() - Duration::minutes(1);
    let mut fire = event("g1", EventAction::Fire);
    let mut resolve = event("g1", EventAction::Resolve);
    fire.at = Some(t - Duration::minutes(2));
    f.apply(&fire).await;
    resolve.at = Some(t + Duration::seconds(20));
    assert_eq!(f.apply(&resolve).await.status, Status::Resolved);

    fire.at = Some(t);
    let second = f.apply(&fire).await.incident.unwrap();
    resolve.at = Some(t + Duration::seconds(10));
    assert_eq!(f.apply(&resolve).await.status, Status::Resolved);
    assert_eq!(f.row(second).await.0, "resolved");
}

/// An alert resolves and fires again a few seconds later. A late copy of the
/// first resolve must not close the second incident, however soon after it
/// we opened that one.
#[tokio::test]
#[ignore]
async fn a_late_copy_of_a_resolve_leaves_a_quick_refire_open_pg() {
    let Some(f) = fx("ingestflap").await else {
        return;
    };
    let now = Utc::now();
    let mut fire = event("g1", EventAction::Fire);
    fire.at = Some(now - Duration::minutes(2));
    f.apply(&fire).await;
    let mut resolve = event("g1", EventAction::Resolve);
    resolve.at = Some(now - Duration::seconds(20));
    assert_eq!(f.apply(&resolve).await.status, Status::Resolved);
    fire.at = Some(now - Duration::seconds(10));
    let again = f.apply(&fire).await.incident.unwrap();
    assert_eq!(f.apply(&resolve).await.status, Status::Ignored);
    assert_eq!(f.row(again).await.0, "triggered");
}

/// A person reopens an incident its tool resolved. A late copy of that same
/// resolve must not close it again, however many resolves and reopens came
/// between; a newer one still does.
#[tokio::test]
#[ignore]
async fn a_late_copy_of_a_resolve_leaves_a_reopened_incident_open_pg() {
    let Some(f) = fx("ingestreopen").await else {
        return;
    };
    let now = Utc::now();
    let mut fire = event("g1", EventAction::Fire);
    fire.at = Some(now - Duration::minutes(5));
    let id = f.apply(&fire).await.incident.unwrap();
    let ops = uptimepage::storage::PgIncidentOpsStore::new(f.pool.clone());
    let reopen = || async {
        ops.reopen(f.org, id, uptimepage::storage::Actor::System, None)
            .await
            .unwrap();
    };
    let mut first = event("g1", EventAction::Resolve);
    first.at = Some(now - Duration::minutes(1));
    assert_eq!(f.apply(&first).await.status, Status::Resolved);
    reopen().await;
    assert_eq!(f.apply(&first).await.status, Status::Ignored);
    assert_eq!(f.row(id).await.0, "triggered");

    let mut near = event("g1", EventAction::Resolve);
    near.at = Some(now - Duration::seconds(40));
    assert_eq!(f.apply(&near).await.status, Status::Resolved);
    reopen().await;
    assert_eq!(
        f.apply(&first).await.status,
        Status::Ignored,
        "the first resolve is still known after another one closed it"
    );
    assert_eq!(f.row(id).await.0, "triggered");

    let mut newer = event("g1", EventAction::Resolve);
    newer.at = Some(now);
    assert_eq!(f.apply(&newer).await.status, Status::Resolved);
}

/// We were down and the firing reached us minutes late; its resolve, dated
/// before we opened the incident, is still the one that ends it.
#[tokio::test]
#[ignore]
async fn a_firing_that_arrived_late_still_closes_on_its_resolve_pg() {
    let Some(f) = fx("ingestlate").await else {
        return;
    };
    let now = Utc::now();
    let mut fire = event("g1", EventAction::Fire);
    fire.at = Some(now - Duration::minutes(5));
    let id = f.apply(&fire).await.incident.unwrap();
    let mut resolve = event("g1", EventAction::Resolve);
    resolve.at = Some(now - Duration::minutes(3));
    assert_eq!(f.apply(&resolve).await.status, Status::Resolved);
    assert_eq!(f.row(id).await.0, "resolved");
}

/// A person resolves while the tool still fires: the next firing opens a
/// fresh incident rather than counting on the closed one.
#[tokio::test]
#[ignore]
async fn a_firing_after_a_persons_resolve_opens_a_fresh_incident_pg() {
    let Some(f) = fx("ingesthuman").await else {
        return;
    };
    let id = f
        .apply(&event("g1", EventAction::Fire))
        .await
        .incident
        .unwrap();
    uptimepage::storage::PgIncidentOpsStore::new(f.pool.clone())
        .resolve(f.org, id, uptimepage::storage::Actor::System, None)
        .await
        .unwrap();
    let next = f.apply(&event("g1", EventAction::Fire)).await;
    assert_eq!(next.status, Status::Opened);
    assert_ne!(next.incident, Some(id));
}

#[tokio::test]
#[ignore]
async fn a_paused_or_held_source_opens_nothing_but_still_closes_pg() {
    let Some(f) = fx("ingestpause").await else {
        return;
    };
    let id = f
        .apply(&event("open", EventAction::Fire))
        .await
        .incident
        .unwrap();
    f.pause(false).await;
    assert_eq!(
        f.apply(&event("new", EventAction::Fire)).await.status,
        Status::Paused
    );
    assert_eq!(
        f.apply(&event("open", EventAction::Resolve)).await.status,
        Status::Resolved,
        "pausing never strands an incident open"
    );
    assert_eq!(f.row(id).await.0, "resolved");

    f.pause(true).await;
    sqlx::query("UPDATE alert_sources SET plan_hold_at = now() WHERE id = $1 AND org_id = $2")
        .bind(f.source.id)
        .bind(f.org.0)
        .execute(&f.pool)
        .await
        .unwrap();
    assert_eq!(
        f.apply(&event("new", EventAction::Fire)).await.status,
        Status::Held
    );
    assert_eq!(f.open_incidents().await, 0);
}

#[tokio::test]
#[ignore]
async fn past_the_cap_one_summary_pages_and_closes_when_there_is_room_pg() {
    let Some(f) = fx("ingestcap").await else {
        return;
    };
    for i in 0..MAX_OPEN_PER_SOURCE {
        assert_eq!(
            f.apply(&event(&format!("k{i}"), EventAction::Fire))
                .await
                .status,
            Status::Opened
        );
    }
    let first = f.apply(&event("over-1", EventAction::Fire)).await;
    assert_eq!(first.status, Status::Capped);
    let summary = first.incident.unwrap();
    assert_eq!(
        first.signals,
        vec![(summary, NotificationReason::Opened)],
        "the summary pages once"
    );
    let second = f.apply(&event("over-2", EventAction::Fire)).await;
    assert_eq!(second.status, Status::Capped);
    assert!(second.signals.is_empty());
    assert_eq!(f.row(summary).await.3, 1, "each refusal is counted");
    let title: String =
        sqlx::query_scalar("SELECT title FROM incidents WHERE id = $1 AND org_id = $2")
            .bind(summary)
            .bind(f.org.0)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(title, "Prod Grafana hit its limit of 50 open alerts");

    let room = f.apply(&event("k0", EventAction::Resolve)).await;
    assert_eq!(room.status, Status::Resolved);
    assert!(
        room.signals
            .contains(&(summary, NotificationReason::Resolved)),
        "its close is signalled too"
    );
    assert_eq!(
        f.row(summary).await.0,
        "resolved",
        "room under the cap closes the summary"
    );
    assert_eq!(f.open_incidents().await, MAX_OPEN_PER_SOURCE - 1);
}

#[tokio::test]
#[ignore]
async fn an_unreadable_body_opens_one_incident_that_later_ones_fold_into_pg() {
    let Some(f) = fx("ingestunparsed").await else {
        return;
    };
    let first = f
        .ingest
        .apply_unparsed(f.org, &f.source, "not an Alertmanager or Grafana alert")
        .await
        .unwrap()
        .applied()
        .unwrap();
    assert_eq!(first.status, Status::Unparsed);
    assert_eq!(first.signals.len(), 1);
    let again = f
        .ingest
        .apply_unparsed(f.org, &f.source, "again")
        .await
        .unwrap()
        .applied()
        .unwrap();
    assert_eq!(again.incident, first.incident);
    assert!(again.signals.is_empty());
    let title: String =
        sqlx::query_scalar("SELECT title FROM incidents WHERE id = $1 AND org_id = $2")
            .bind(first.incident.unwrap())
            .bind(f.org.0)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(title, "Prod Grafana sent an alert we could not read");
}

#[tokio::test]
#[ignore]
async fn two_firings_racing_on_one_key_open_one_incident_pg() {
    let Some(f) = fx("ingestrace").await else {
        return;
    };
    let e = event("g1", EventAction::Fire);
    let (a, b) = tokio::join!(
        f.ingest.apply(f.org, &f.source, &e),
        f.ingest.apply(f.org, &f.source, &e)
    );
    let status = |r: uptimepage::error::Result<Ingested>| r.unwrap().applied().unwrap().status;
    let mut statuses = [status(a), status(b)];
    statuses.sort_by_key(|s| s.as_str());
    assert_eq!(statuses, [Status::Opened, Status::Repeat]);
    assert_eq!(f.open_incidents().await, 1);
}

#[tokio::test]
#[ignore]
async fn a_source_deleted_mid_request_takes_nothing_pg() {
    let Some(f) = fx("ingestgone").await else {
        return;
    };
    f.sources.delete(f.org, f.source.id, None).await.unwrap();
    assert_eq!(
        f.apply(&event("g1", EventAction::Fire)).await.status,
        Status::Gone
    );
}

#[tokio::test]
#[ignore]
async fn an_org_deleted_mid_request_takes_nothing_pg() {
    let Some(f) = fx("ingestorggone").await else {
        return;
    };
    sqlx::query("UPDATE organizations SET deleted_at = now() WHERE id = $1")
        .bind(f.org.0)
        .execute(&f.pool)
        .await
        .unwrap();
    assert_eq!(
        f.apply(&event("g1", EventAction::Fire)).await.status,
        Status::Gone
    );
    assert_eq!(f.open_incidents().await, 0);
}

/// A resolve with alerts cut from it is not trusted: it keeps an open
/// incident open and never opens one for a group that recovered.
#[tokio::test]
#[ignore]
async fn a_distrusted_resolve_never_opens_an_incident_pg() {
    let Some(f) = fx("ingestcut").await else {
        return;
    };
    let mut cut = event("g1", EventAction::Fire);
    cut.may_open = false;
    assert_eq!(f.apply(&cut).await.status, Status::Ignored);
    assert_eq!(f.open_incidents().await, 0);
    let id = f
        .apply(&event("g1", EventAction::Fire))
        .await
        .incident
        .unwrap();
    assert_eq!(f.apply(&cut).await.status, Status::Repeat);
    assert_eq!(f.row(id).await.0, "triggered");
}

/// A person who lowered the severity is overruled only when the tool says
/// worse than it ever has, not when it repeats itself.
#[tokio::test]
#[ignore]
async fn a_persons_downgrade_survives_the_tool_repeating_itself_pg() {
    let Some(f) = fx("ingestsev").await else {
        return;
    };
    let id = f
        .apply(&event("g1", EventAction::Fire))
        .await
        .incident
        .unwrap();
    sqlx::query("UPDATE incidents SET severity = 'minor' WHERE id = $1 AND org_id = $2")
        .bind(id)
        .bind(f.org.0)
        .execute(&f.pool)
        .await
        .unwrap();
    f.apply(&event("g1", EventAction::Fire)).await;
    assert_eq!(
        f.row(id).await.1,
        "minor",
        "the same major again changes nothing"
    );
    let mut worse = event("g1", EventAction::Fire);
    worse.severity = IncidentSeverity::Critical;
    f.apply(&worse).await;
    assert_eq!(f.row(id).await.1, "critical");
}

/// Alerts in one group can carry times from servers on different clocks.
#[tokio::test]
#[ignore]
async fn a_resolve_a_little_before_the_latest_firing_still_closes_pg() {
    let Some(f) = fx("ingestskew").await else {
        return;
    };
    let now = Utc::now();
    let mut fire = event("g1", EventAction::Fire);
    fire.at = Some(now);
    let id = f.apply(&fire).await.incident.unwrap();
    let mut resolve = event("g1", EventAction::Resolve);
    resolve.at = Some(now - Duration::seconds(30));
    assert_eq!(f.apply(&resolve).await.status, Status::Resolved);
    assert_eq!(f.row(id).await.0, "resolved");
}

/// Two cycles of one alert reach us late, back to back. The second cycle's
/// recovery is newer than the first's on the sender's clock, so it closes,
/// however close together we happened to process them.
#[tokio::test]
#[ignore]
async fn two_late_cycles_each_close_on_their_own_recovery_pg() {
    let Some(f) = fx("ingestcycles").await else {
        return;
    };
    let now = Utc::now();
    let at = |mins: i64| Some(now - Duration::minutes(mins));
    let mut fire = event("g1", EventAction::Fire);
    let mut resolve = event("g1", EventAction::Resolve);

    fire.at = at(10);
    f.apply(&fire).await;
    resolve.at = at(8);
    assert_eq!(f.apply(&resolve).await.status, Status::Resolved);

    fire.at = at(7);
    let second = f.apply(&fire).await.incident.unwrap();
    resolve.at = at(6);
    assert_eq!(
        f.apply(&resolve).await.status,
        Status::Resolved,
        "newer than the first cycle's recovery on the sender's clock"
    );
    assert_eq!(f.row(second).await.0, "resolved");
}

/// A person who raised the incident is never lowered by the tool getting
/// worse than it was, but not as bad as the person judged.
#[tokio::test]
#[ignore]
async fn a_persons_raise_is_never_lowered_by_the_tool_pg() {
    let Some(f) = fx("ingestraise").await else {
        return;
    };
    let mut minor = event("g1", EventAction::Fire);
    minor.severity = IncidentSeverity::Minor;
    let id = f.apply(&minor).await.incident.unwrap();
    sqlx::query("UPDATE incidents SET severity = 'critical' WHERE id = $1 AND org_id = $2")
        .bind(id)
        .bind(f.org.0)
        .execute(&f.pool)
        .await
        .unwrap();
    f.apply(&event("g1", EventAction::Fire)).await;
    assert_eq!(f.row(id).await.1, "critical");
    let raised: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM incident_events \
         WHERE incident_id = $1 AND org_id = $2 AND kind = 'severity_changed'",
    )
    .bind(id)
    .bind(f.org.0)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(raised, 0, "nothing was raised, so nothing is said");
}

/// People resolve incidents by hand, freeing room under the cap; the tool's
/// next event closes the summary left open, even one that does nothing else.
async fn freed_room_closes_the_summary_on(prefix: &str, next: AlertEvent, status: Status) {
    let Some(f) = fx(prefix).await else {
        return;
    };
    let mut first = None;
    for i in 0..MAX_OPEN_PER_SOURCE {
        let id = f
            .apply(&event(&format!("k{i}"), EventAction::Fire))
            .await
            .incident;
        first = first.or(id);
    }
    let summary = f
        .apply(&event("over", EventAction::Fire))
        .await
        .incident
        .unwrap();
    uptimepage::storage::PgIncidentOpsStore::new(f.pool.clone())
        .resolve(
            f.org,
            first.unwrap(),
            uptimepage::storage::Actor::System,
            None,
        )
        .await
        .unwrap();
    let applied = f.apply(&next).await;
    assert_eq!(applied.status, status);
    assert!(
        applied
            .signals
            .contains(&(summary, NotificationReason::Resolved))
    );
    assert!(applied.changed, "closing the summary is a change");
    assert_eq!(f.row(summary).await.0, "resolved");
}

#[tokio::test]
#[ignore]
async fn a_repeat_closes_a_summary_once_people_freed_room_pg() {
    freed_room_closes_the_summary_on("ingestroom", event("k1", EventAction::Fire), Status::Repeat)
        .await;
}

/// The capped key's own resolve finds nothing open, and still closes it.
#[tokio::test]
#[ignore]
async fn an_ignored_resolve_closes_a_summary_once_people_freed_room_pg() {
    freed_room_closes_the_summary_on(
        "ingestroomres",
        event("over", EventAction::Resolve),
        Status::Ignored,
    )
    .await;
}

/// The delivery log keeps a repeat's body only when it changed something.
#[tokio::test]
#[ignore]
async fn a_repeat_says_whether_it_changed_the_incident_pg() {
    let Some(f) = fx("ingestchanged").await else {
        return;
    };
    let fire = event("c1", EventAction::Fire);
    assert_eq!(f.apply(&fire).await.status, Status::Opened);
    let same = f.apply(&fire).await;
    assert_eq!((same.status, same.changed), (Status::Repeat, false));
    let more = f
        .apply(&AlertEvent {
            firing: 5,
            ..fire.clone()
        })
        .await;
    assert_eq!((more.status, more.changed), (Status::Repeat, true));
    let worse = f
        .apply(&AlertEvent {
            firing: 5,
            severity: IncidentSeverity::Critical,
            ..fire.clone()
        })
        .await;
    assert!(worse.changed, "a raise is a change");
}

/// A body read under one grouping, applied after the grouping changed, would
/// open an incident under a key its later events never match, or miss the one
/// they opened. Every event is refused before anything is written, for the
/// caller to parse again.
#[tokio::test]
#[ignore]
async fn an_event_cut_under_a_grouping_since_changed_opens_nothing_pg() {
    let Some(f) = fx("ingestregroup").await else {
        return;
    };
    let read_before_the_change = AlertSource {
        grouping: AlertGrouping::Alert,
        ..f.source.clone()
    };
    let fire = event("g1", EventAction::Fire);
    let mut resolve = fire.clone();
    resolve.action = EventAction::Resolve;
    for e in [&fire, &resolve] {
        let refused = f
            .ingest
            .apply(f.org, &read_before_the_change, e)
            .await
            .unwrap();
        assert_eq!(refused, Ingested::Regrouped(AlertGrouping::Group));
    }
    let unread = f
        .ingest
        .apply_unparsed(f.org, &read_before_the_change, "no fingerprint")
        .await
        .unwrap();
    assert_eq!(
        unread,
        Ingested::Regrouped(AlertGrouping::Group),
        "whether a body reads at all can depend on the grouping"
    );
    assert_eq!(f.open_incidents().await, 0);
    assert_eq!(
        f.apply(&event("g1", EventAction::Fire)).await.status,
        Status::Opened
    );
}
