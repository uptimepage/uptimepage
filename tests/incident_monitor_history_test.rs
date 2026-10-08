//! An incident outlives its monitor. Deleting a monitor keeps its incidents,
//! their timeline and the alerts sent for them, closes whatever was still open
//! and stops it paging.

use crate::common;

use std::time::Duration;

use chrono::Utc;
use sqlx::PgPool;
use uptimepage::domain::{
    CheckSpec, ExpectedStatus, MONITOR_DELETED_MESSAGE, NewTarget, OrgId, UserId, WriteSource,
};
use uptimepage::error::AppError;
use uptimepage::error::codes;
use uptimepage::storage::{
    Actor, IncidentBriefFilter, IncidentNarrationStore, IncidentOpsStore, LifecycleOutcome,
    PgIncidentNarrationStore, PgIncidentOpsStore, PostgresTargetStore, TargetStore,
    create_org_with_owner,
};
use url::Url;
use uuid::Uuid;

use crate::common::MIGRATOR;

fn http_target(name: &str) -> NewTarget {
    NewTarget {
        name: name.into(),
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
    }
}

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
    let org = create_org_with_owner(&pool, user, &common::unique_slug("inchist"), "Co")
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
    async fn monitor(&self, name: &str) -> Uuid {
        self.targets
            .create(
                self.org,
                http_target(name),
                WriteSource::Ui,
                i64::MAX,
                i64::MAX,
            )
            .await
            .unwrap()
            .id
    }

    /// An open incident that is due to escalate, the state a page is in while
    /// nobody has taken it.
    async fn open_incident(&self, target: Uuid) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO incidents (org_id, target_id, started_at, status_at_start, next_escalation_at) \
             VALUES ($1, $2, now() - interval '30 minutes', 'down', now() - interval '1 minute') \
             RETURNING id",
        )
        .bind(self.org.0)
        .bind(target)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    async fn resolved_incident(&self, target: Uuid) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO incidents (org_id, target_id, started_at, ended_at, state, status_at_start) \
             VALUES ($1, $2, now() - interval '2 days', now() - interval '1 day', 'resolved', 'down') \
             RETURNING id",
        )
        .bind(self.org.0)
        .bind(target)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    /// A declared incident naming the monitor, as an operator raises one.
    async fn declared_incident(&self, target: Uuid) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO incidents \
                 (org_id, target_id, started_at, status_at_start, origin, title, next_escalation_at) \
             VALUES ($1, $2, now() - interval '1 hour', 'down', 'manual', 'Payments degraded', \
                     now() + interval '5 minutes') \
             RETURNING id",
        )
        .bind(self.org.0)
        .bind(target)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    /// A status page carrying the monitor, under `public_name` when given.
    async fn page_showing(&self, target: Uuid, public_name: Option<&str>) -> Uuid {
        let page: Uuid = sqlx::query_scalar(
            "INSERT INTO status_pages (org_id, slug, name) VALUES ($1, $2, 'Status') RETURNING id",
        )
        .bind(self.org.0)
        .bind(common::unique_slug("inchist"))
        .fetch_one(&self.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO status_page_components (org_id, status_page_id, target_id, public_name) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(self.org.0)
        .bind(page)
        .bind(target)
        .bind(public_name)
        .execute(&self.pool)
        .await
        .unwrap();
        page
    }

    async fn notification(&self, incident: Uuid, status: &str) -> Uuid {
        self.notice(incident, "opened", status).await
    }

    async fn notice(&self, incident: Uuid, reason: &str, status: &str) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO incident_notifications (org_id, incident_id, transport, reason, status) \
             VALUES ($1, $2, 'email', $3, $4) RETURNING id",
        )
        .bind(self.org.0)
        .bind(incident)
        .bind(reason)
        .bind(status)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    async fn notification_status(&self, id: Uuid) -> String {
        sqlx::query_scalar("SELECT status FROM incident_notifications WHERE id = $1")
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn monitor_deleted_events(&self, incident: Uuid) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM incident_events \
             WHERE incident_id = $1 AND kind = 'monitor_deleted'",
        )
        .bind(incident)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    async fn audit_metadata(&self, action: &str) -> serde_json::Value {
        sqlx::query_scalar("SELECT metadata FROM org_audit_log WHERE org_id = $1 AND action = $2")
            .bind(self.org.0)
            .bind(action)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn deleting_a_monitor_keeps_its_incidents_and_closes_the_open_one() {
    let Some(f) = fixture("inc_hist_one").await else {
        return;
    };
    let monitor = f.monitor("checkout").await;
    let past = f.resolved_incident(monitor).await;
    let open = f.open_incident(monitor).await;
    let queued = f.notification(open, "queued").await;
    let failed = f.notification(open, "failed").await;
    let sent = f.notification(open, "sent").await;
    let owed_recovery = f.notice(past, "resolved", "failed").await;

    assert!(
        f.targets
            .delete(f.org, monitor, Some(f.user))
            .await
            .unwrap()
    );

    let closed = f
        .ops
        .get(f.org, open)
        .await
        .unwrap()
        .expect("open incident kept");
    assert!(closed.monitor_deleted());
    assert!(closed.closed_by_monitor_delete);
    assert_eq!(closed.target_ref, Some(monitor));
    assert_eq!(closed.target_name.as_deref(), Some("checkout"));
    assert_eq!(closed.target_kind.as_deref(), Some("http"));
    assert_eq!(closed.state.as_db_str(), "resolved");
    assert!(closed.ended_at.is_some());
    assert_eq!(
        closed.resolved_by,
        Some(f.user),
        "resolved by whoever deleted it"
    );
    assert!(
        closed.next_escalation_at.is_none(),
        "nothing left to escalate"
    );
    assert_eq!(f.monitor_deleted_events(open).await, 1);

    let history = f
        .ops
        .get(f.org, past)
        .await
        .unwrap()
        .expect("past incident kept");
    assert!(history.monitor_deleted());
    assert!(
        history.resolved_by.is_none(),
        "an already-closed incident is left as it was"
    );
    assert_eq!(f.monitor_deleted_events(past).await, 0);
    assert!(!history.closed_by_monitor_delete);
    assert_eq!(
        f.ops.closed_with_monitors(f.org, &[monitor]).await.unwrap(),
        vec![open],
        "only what the delete closed hears about it"
    );

    assert_eq!(f.notification_status(queued).await, "suppressed");
    assert_eq!(f.notification_status(failed).await, "suppressed");
    assert_eq!(
        f.notification_status(sent).await,
        "sent",
        "the alert record survives"
    );
    assert_eq!(
        f.notification_status(owed_recovery).await,
        "failed",
        "a recovery that really happened is still owed"
    );

    let meta = f.audit_metadata("target.deleted").await;
    assert_eq!(meta["incidents_closed"], 1);

    let due = f.ops.due_for_escalation(Utc::now(), 100, 30).await.unwrap();
    assert!(
        due.iter().all(|d| d.due.id != open),
        "a closed incident never escalates"
    );

    common::drop_test_db(&f.db).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn bulk_delete_closes_every_monitors_open_incident() {
    let Some(f) = fixture("inc_hist_bulk").await else {
        return;
    };
    let a = f.monitor("api").await;
    let b = f.monitor("worker").await;
    let open_a = f.open_incident(a).await;
    let open_b = f.open_incident(b).await;

    let deleted = f
        .targets
        .delete_bulk(f.org, &[a, b], Some(f.user))
        .await
        .unwrap();
    assert_eq!(deleted.len(), 2);

    for (id, name) in [(open_a, "api"), (open_b, "worker")] {
        let inc = f.ops.get(f.org, id).await.unwrap().expect("incident kept");
        assert_eq!(inc.state.as_db_str(), "resolved");
        assert_eq!(inc.target_name.as_deref(), Some(name));
        assert_eq!(f.monitor_deleted_events(id).await, 1);
    }
    let meta = f.audit_metadata("target.bulk_deleted").await;
    assert_eq!(meta["incidents_closed"], 2);

    common::drop_test_db(&f.db).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn an_incident_whose_monitor_was_deleted_cannot_reopen() {
    let Some(f) = fixture("inc_hist_reopen").await else {
        return;
    };
    let monitor = f.monitor("db").await;
    let past = f.resolved_incident(monitor).await;
    f.targets
        .delete(f.org, monitor, Some(f.user))
        .await
        .unwrap();

    let err = f
        .ops
        .reopen(f.org, past, Actor::User(f.user), None)
        .await
        .expect_err("nothing could page about or close a reopened orphan");
    assert!(
        matches!(err, AppError::Conflict { code, .. } if code == codes::INCIDENT_MONITOR_DELETED),
        "{err:?}"
    );

    common::drop_test_db(&f.db).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn an_open_orphan_left_by_a_raw_delete_is_never_escalated() {
    let Some(f) = fixture("inc_hist_orphan").await else {
        return;
    };
    let monitor = f.monitor("legacy").await;
    let open = f.open_incident(monitor).await;
    // Bypasses the store, so nothing closes the incident first.
    sqlx::query("DELETE FROM targets WHERE id = $1 AND org_id = $2")
        .bind(monitor)
        .bind(f.org.0)
        .execute(&f.pool)
        .await
        .unwrap();

    let inc = f
        .ops
        .get(f.org, open)
        .await
        .unwrap()
        .expect("incident kept");
    assert!(inc.monitor_deleted());
    assert_eq!(inc.state.as_db_str(), "triggered");
    let due = f.ops.due_for_escalation(Utc::now(), 100, 30).await.unwrap();
    assert!(due.iter().all(|d| d.due.id != open));

    common::drop_test_db(&f.db).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_rename_follows_onto_the_monitors_incidents() {
    let Some(f) = fixture("inc_hist_rename").await else {
        return;
    };
    let monitor = f.monitor("old name").await;
    let past = f.resolved_incident(monitor).await;
    sqlx::query("UPDATE targets SET name = 'new name' WHERE id = $1 AND org_id = $2")
        .bind(monitor)
        .bind(f.org.0)
        .execute(&f.pool)
        .await
        .unwrap();

    let inc = f.ops.get(f.org, past).await.unwrap().unwrap();
    assert_eq!(inc.target_name.as_deref(), Some("new name"));

    common::drop_test_db(&f.db).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn an_incident_cannot_name_another_orgs_monitor() {
    let Some(f) = fixture("inc_hist_xorg").await else {
        return;
    };
    let other_user = common::make_user(&f.pool, "inc_hist_xorg_b").await;
    let other = create_org_with_owner(&f.pool, other_user, &common::unique_slug("b"), "B")
        .await
        .unwrap()
        .unwrap()
        .id;
    let foreign: Uuid = PostgresTargetStore::from_pool(f.pool.clone(), None)
        .create(
            other,
            http_target("theirs"),
            WriteSource::Ui,
            i64::MAX,
            i64::MAX,
        )
        .await
        .unwrap()
        .id;

    let err = sqlx::query(
        "INSERT INTO incidents (org_id, target_id, started_at, status_at_start) \
         VALUES ($1, $2, now(), 'down')",
    )
    .bind(f.org.0)
    .bind(foreign)
    .execute(&f.pool)
    .await
    .expect_err("cross-org incident refused");
    let db = err.as_database_error().expect("database error");
    assert_eq!(db.constraint(), Some("incidents_target_id_fkey"));

    common::drop_test_db(&f.db).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn purging_an_org_takes_its_monitor_incidents_with_it() {
    let Some(f) = fixture("inc_hist_purge").await else {
        return;
    };
    let monitor = f.monitor("api").await;
    f.resolved_incident(monitor).await;
    let open = f.open_incident(monitor).await;
    f.notification(open, "queued").await;

    // The purge job's statement: the monitor and its incidents go in one
    // cascade, which clears the incident's link before it deletes the row.
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org.0)
        .execute(&f.pool)
        .await
        .expect("org purge");
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM incidents WHERE org_id = $1")
        .bind(f.org.0)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(left, 0);

    common::drop_test_db(&f.db).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn incident_lists_name_a_deleted_monitor() {
    let Some(f) = fixture("inc_hist_list").await else {
        return;
    };
    let monitor = f.monitor("payments").await;
    let past = f.resolved_incident(monitor).await;
    f.targets
        .delete(f.org, monitor, Some(f.user))
        .await
        .unwrap();

    let briefs = PgIncidentNarrationStore::new(f.pool.clone())
        .list_briefs(
            f.org,
            IncidentBriefFilter {
                open_only: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let brief = briefs.iter().find(|b| b.id == past).expect("listed");
    assert_eq!(brief.target_id, None);
    assert_eq!(brief.target_name, "payments");

    let metrics = f.ops.metrics(f.org, 30).await.unwrap();
    let noisy = metrics
        .top_monitors
        .iter()
        .find(|m| m.name == "payments")
        .expect("a deleted monitor still counts");
    assert_eq!(noisy.target_id, None);

    common::drop_test_db(&f.db).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn two_deleted_monitors_that_shared_a_name_stay_apart_in_reports() {
    let Some(f) = fixture("inc_hist_twins").await else {
        return;
    };
    let first = f.monitor("api").await;
    f.resolved_incident(first).await;
    f.resolved_incident(first).await;
    f.targets.delete(f.org, first, Some(f.user)).await.unwrap();
    let second = f.monitor("api").await;
    f.resolved_incident(second).await;
    f.targets.delete(f.org, second, Some(f.user)).await.unwrap();
    let live = f.monitor("api").await;
    f.resolved_incident(live).await;

    let metrics = f.ops.metrics(f.org, 30).await.unwrap();
    let mut apis: Vec<(Option<Uuid>, u64)> = metrics
        .top_monitors
        .iter()
        .filter(|m| m.name == "api")
        .map(|m| (m.target_id, m.count))
        .collect();
    apis.sort();
    assert_eq!(apis, vec![(None, 1), (None, 2), (Some(live), 1)]);

    common::drop_test_db(&f.db).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_declared_incident_stays_open_when_its_monitor_is_deleted() {
    let Some(f) = fixture("inc_hist_declared").await else {
        return;
    };
    let monitor = f.monitor("payments").await;
    let declared = f.declared_incident(monitor).await;
    let queued = f.notification(declared, "queued").await;
    f.targets
        .delete(f.org, monitor, Some(f.user))
        .await
        .unwrap();

    let inc = f.ops.get(f.org, declared).await.unwrap().expect("kept");
    assert_eq!(
        inc.state.as_db_str(),
        "triggered",
        "a person declared it, a person closes it"
    );
    assert!(inc.monitor_deleted());
    assert!(!inc.closed_by_monitor_delete);
    assert!(!inc.paging_enabled, "nothing left to page through");
    assert!(inc.next_escalation_at.is_none());
    assert_eq!(inc.target_name.as_deref(), Some("payments"));
    assert_eq!(f.monitor_deleted_events(declared).await, 1);
    assert_eq!(
        f.notification_status(queued).await,
        "suppressed",
        "paging went with the monitor"
    );
    assert!(
        f.ops
            .closed_with_monitors(f.org, &[monitor])
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        f.audit_metadata("target.deleted").await["incidents_closed"],
        0
    );

    f.ops
        .resolve(f.org, declared, Actor::User(f.user), None)
        .await
        .unwrap();
    let reopened = f
        .ops
        .reopen(f.org, declared, Actor::User(f.user), None)
        .await
        .unwrap();
    assert!(matches!(reopened, LifecycleOutcome::Updated(_)));

    common::drop_test_db(&f.db).await;
}

/// A public incident stays on the page that showed its monitor, under the name
/// the page gave it. The one the delete closed says so plainly; the declared
/// one is still open and still a person's to narrate.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_public_incident_stays_on_its_page_and_says_monitoring_was_removed() {
    let Some(f) = fixture("inc_hist_public").await else {
        return;
    };
    let monitor = f.monitor("api-internal").await;
    let page = f.page_showing(monitor, Some("API")).await;
    let closed = f.open_incident(monitor).await;
    let declared = f.declared_incident(monitor).await;
    sqlx::query("UPDATE incidents SET visibility = 'public' WHERE id = ANY($1)")
        .bind(vec![closed, declared])
        .execute(&f.pool)
        .await
        .unwrap();
    f.targets
        .delete(f.org, monitor, Some(f.user))
        .await
        .unwrap();

    let mut pins: Vec<(Uuid, Uuid, Option<String>)> = sqlx::query_as(
        "SELECT incident_id, status_page_id, component_name FROM incident_status_pages \
         WHERE org_id = $1",
    )
    .bind(f.org.0)
    .fetch_all(&f.pool)
    .await
    .unwrap();
    pins.sort();
    let mut want = vec![
        (closed, page, Some("API".to_string())),
        (declared, page, Some("API".to_string())),
    ];
    want.sort();
    assert_eq!(pins, want);

    let mut updates: Vec<(Uuid, String, String)> = sqlx::query_as(
        "SELECT incident_id, phase, message FROM incident_updates WHERE org_id = $1",
    )
    .bind(f.org.0)
    .fetch_all(&f.pool)
    .await
    .unwrap();
    updates.sort();
    let closing = |id| {
        (
            id,
            "resolved".to_string(),
            MONITOR_DELETED_MESSAGE.to_string(),
        )
    };
    assert_eq!(updates, vec![closing(closed)], "no recovery is claimed");

    // Saving the declared incident's pages again keeps the name the page had.
    f.ops
        .publish(
            f.org,
            declared,
            None,
            None,
            Some(vec![page]),
            Actor::User(f.user),
        )
        .await
        .unwrap()
        .expect("published");
    let name: Option<String> = sqlx::query_scalar(
        "SELECT component_name FROM incident_status_pages WHERE incident_id = $1",
    )
    .bind(declared)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(name.as_deref(), Some("API"));

    common::drop_test_db(&f.db).await;
}

/// Published once, then taken off the page before it ended: its subscribers
/// were told it opened, so the closing update still reaches them, through the
/// page that showed it.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn an_incident_taken_off_its_page_still_closes_for_its_subscribers() {
    let Some(f) = fixture("inc_hist_unpub").await else {
        return;
    };
    let monitor = f.monitor("api-internal").await;
    let page = f.page_showing(monitor, Some("API")).await;
    let open = f.open_incident(monitor).await;
    sqlx::query(
        "INSERT INTO incident_events (org_id, incident_id, kind, actor_type) \
         VALUES ($1, $2, 'published', 'system')",
    )
    .bind(f.org.0)
    .bind(open)
    .execute(&f.pool)
    .await
    .unwrap();
    f.targets
        .delete(f.org, monitor, Some(f.user))
        .await
        .unwrap();

    let pins: Vec<(Uuid, Option<String>)> = sqlx::query_as(
        "SELECT status_page_id, component_name FROM incident_status_pages WHERE incident_id = $1",
    )
    .bind(open)
    .fetch_all(&f.pool)
    .await
    .unwrap();
    assert_eq!(pins, vec![(page, Some("API".to_string()))]);
    let updates: Vec<String> =
        sqlx::query_scalar("SELECT message FROM incident_updates WHERE incident_id = $1")
            .bind(open)
            .fetch_all(&f.pool)
            .await
            .unwrap();
    assert_eq!(updates, vec![MONITOR_DELETED_MESSAGE.to_string()]);

    common::drop_test_db(&f.db).await;
}

/// A monitor's incident lives on only where the monitor was shown: never on a
/// page that did not carry it, and nowhere if it was never public.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_deleted_monitors_incident_stays_off_pages_that_never_showed_it() {
    let Some(f) = fixture("inc_hist_pages").await else {
        return;
    };
    let monitor = f.monitor("api-internal").await;
    let page = f.page_showing(monitor, Some("API")).await;
    let public = f.open_incident(monitor).await;
    sqlx::query("UPDATE incidents SET visibility = 'public' WHERE id = $1")
        .bind(public)
        .execute(&f.pool)
        .await
        .unwrap();
    let internal = f.resolved_incident(monitor).await;
    let other = f.page_showing(f.monitor("billing").await, None).await;
    f.targets
        .delete(f.org, monitor, Some(f.user))
        .await
        .unwrap();

    let refused = |res: Result<_, AppError>| {
        let err = res.expect_err("refused");
        assert!(
            matches!(err, AppError::Conflict { code, .. } if code == codes::INCIDENT_MONITOR_DELETED),
            "{err:?}"
        );
    };
    let me = Actor::User(f.user);
    refused(
        f.ops
            .publish(f.org, public, None, None, Some(vec![page, other]), me)
            .await,
    );
    refused(f.ops.publish(f.org, internal, None, None, None, me).await);
    refused(
        f.ops
            .publish(f.org, internal, None, None, Some(vec![page]), me)
            .await,
    );
    f.ops
        .publish(f.org, public, None, None, Some(vec![page]), me)
        .await
        .unwrap()
        .expect("still on its own page");

    common::drop_test_db(&f.db).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_close_by_monitor_delete_stays_out_of_resolution_metrics() {
    let Some(f) = fixture("inc_hist_mttr").await else {
        return;
    };
    let monitor = f.monitor("legacy").await;
    f.open_incident(monitor).await;
    f.targets
        .delete(f.org, monitor, Some(f.user))
        .await
        .unwrap();

    let m = f.ops.metrics(f.org, 30).await.unwrap();
    assert_eq!(m.total, 1);
    assert_eq!((m.human_resolved, m.auto_resolved), (0, 0));
    assert_eq!(m.closed_with_monitor, 1);
    assert_eq!(m.mttr_secs, None);

    common::drop_test_db(&f.db).await;
}

/// Until another session in this test's database is waiting on a row lock.
async fn wait_for_lock_wait(pool: &PgPool) -> bool {
    for _ in 0..250 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting > 0 {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// A reopen that passed its check before the delete reached the incident
/// commits first, and the delete closes the reopened incident with the rest.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_reopen_racing_the_delete_is_closed_with_the_rest() {
    let Some(f) = fixture("inc_hist_race_reopen").await else {
        return;
    };
    let monitor = f.monitor("api").await;
    let past = f.resolved_incident(monitor).await;

    // The reopen's own steps: row lock, then the update, not yet committed.
    let mut reopen = f.pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM incidents WHERE id = $1 AND org_id = $2 FOR UPDATE")
        .bind(past)
        .bind(f.org.0)
        .execute(&mut *reopen)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE incidents SET state = 'triggered', ended_at = NULL, duration_secs = NULL \
         WHERE id = $1 AND org_id = $2",
    )
    .bind(past)
    .bind(f.org.0)
    .execute(&mut *reopen)
    .await
    .unwrap();

    let store = PostgresTargetStore::from_pool(f.pool.clone(), None);
    let (org, user) = (f.org, f.user);
    let delete = tokio::spawn(async move { store.delete(org, monitor, Some(user)).await });
    assert!(
        wait_for_lock_wait(&f.pool).await,
        "the delete waits on the reopen"
    );
    reopen.commit().await.unwrap();
    assert!(delete.await.unwrap().unwrap());

    let inc = f.ops.get(f.org, past).await.unwrap().unwrap();
    assert!(inc.monitor_deleted());
    assert_eq!(
        inc.state.as_db_str(),
        "resolved",
        "no open incident outlives its monitor"
    );
    assert_eq!(f.monitor_deleted_events(past).await, 1);

    common::drop_test_db(&f.db).await;
}

/// An incident opening while its monitor is renamed ends up with the new name.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn an_incident_opened_during_a_rename_gets_the_new_name() {
    let Some(f) = fixture("inc_hist_race_rename").await else {
        return;
    };
    let monitor = f.monitor("old name").await;

    let mut rename = f.pool.begin().await.unwrap();
    sqlx::query("UPDATE targets SET name = 'new name' WHERE id = $1 AND org_id = $2")
        .bind(monitor)
        .bind(f.org.0)
        .execute(&mut *rename)
        .await
        .unwrap();

    let pool = f.pool.clone();
    let org = f.org;
    let insert = tokio::spawn(async move {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO incidents (org_id, target_id, started_at, status_at_start) \
             VALUES ($1, $2, now(), 'down') RETURNING id",
        )
        .bind(org.0)
        .bind(monitor)
        .fetch_one(&pool)
        .await
    });
    // Either the insert waits for the rename or it has already finished; both
    // must end with the new name.
    while !insert.is_finished() && !wait_for_lock_wait(&f.pool).await {}
    rename.commit().await.unwrap();
    let id = insert.await.unwrap().unwrap();

    let inc = f.ops.get(f.org, id).await.unwrap().unwrap();
    assert_eq!(inc.target_name.as_deref(), Some("new name"));

    common::drop_test_db(&f.db).await;
}
