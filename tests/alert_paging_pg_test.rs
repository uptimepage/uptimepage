//! The engine's scans find alert incidents the way they find a monitor's: owed
//! first pages, reminders and damped holds, each waiting while the source is
//! paused or held, and none once the source is gone.

use crate::common;
use common::{make_user, unique_slug};
use sqlx::PgPool;
use uptimepage::domain::{
    AlertBinding, AlertGrouping, AlertSourceKind, NewAlertSource, OrgId, TargetAlerts, WriteSource,
};
use uptimepage::storage::alert_sources::{AlertSourceStore, PgAlertSourceStore};
use uptimepage::storage::{IncidentOpsStore, PgIncidentOpsStore, create_org_with_owner};
use uuid::Uuid;

struct Seeded {
    pool: PgPool,
    org: OrgId,
    source: Uuid,
    channel: Uuid,
    ops: PgIncidentOpsStore,
}

async fn seeded(pool: PgPool, prefix: &str) -> Seeded {
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
    let source = PgAlertSourceStore::new(pool.clone(), None)
        .create(
            org,
            NewAlertSource {
                name: "prom".into(),
                kind: AlertSourceKind::Alertmanager,
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
        .unwrap()
        .id;
    Seeded {
        ops: PgIncidentOpsStore::new(pool.clone()),
        pool,
        org,
        source,
        channel,
    }
}

impl Seeded {
    async fn incident(&self, owed: bool) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO incidents \
                 (org_id, started_at, status_at_start, origin, title, alert_source_id, dedup_key, \
                  page_owed_at) \
             VALUES ($1, now() - interval '2 hours', 'down', 'alert', 'KubePodCrashLooping', $2, \
                     $3, CASE WHEN $4 THEN now() - interval '1 minute' END) RETURNING id",
        )
        .bind(self.org.0)
        .bind(self.source)
        .bind(Uuid::now_v7().to_string())
        .bind(owed)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    async fn notification(
        &self,
        incident: Uuid,
        channel: Option<Uuid>,
        transport: &str,
        ago: &str,
    ) {
        sqlx::query(&format!(
            "INSERT INTO incident_notifications \
                 (org_id, incident_id, escalation_level, channel_id, transport, reason, status, \
                  attempt, episode, created_at) \
             VALUES ($1, $2, 0, $3, $4, 'opened', 'sent', 1, 0, now() - interval '{ago}')"
        ))
        .bind(self.org.0)
        .bind(incident)
        .bind(channel)
        .bind(transport)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    async fn set_source(&self, set: &str) {
        sqlx::query(&format!(
            "UPDATE alert_sources SET {set} WHERE id = $1 AND org_id = $2"
        ))
        .bind(self.source)
        .bind(self.org.0)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    async fn owed(&self, id: Uuid) -> bool {
        self.ops
            .due_for_owed_page(chrono::Utc::now(), 1000)
            .await
            .unwrap()
            .iter()
            .any(|o| o.id == id)
    }

    async fn reminded(&self, id: Uuid) -> bool {
        self.ops
            .due_for_renotify(chrono::Utc::now(), 1000)
            .await
            .unwrap()
            .iter()
            .any(|d| d.id == id)
    }

    async fn released(&self, id: Uuid) -> bool {
        self.ops
            .due_for_flap_release(chrono::Utc::now(), chrono::Duration::minutes(10), 1000)
            .await
            .unwrap()
            .iter()
            .any(|d| d.id == id)
    }
}

#[tokio::test]
#[ignore]
async fn an_owed_alert_page_waits_while_its_source_is_paused_or_held_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let s = seeded(pool, "alertowed").await;
    let id = s.incident(true).await;
    assert!(s.owed(id).await, "an owed alert page is found");

    s.set_source("enabled = false").await;
    assert!(!s.owed(id).await, "paused");
    s.set_source("enabled = true, plan_hold_at = now()").await;
    assert!(!s.owed(id).await, "held by the plan");
    s.set_source("plan_hold_at = NULL").await;
    assert!(s.owed(id).await);

    sqlx::query("UPDATE incidents SET alert_source_id = NULL WHERE id = $1 AND org_id = $2")
        .bind(id)
        .bind(s.org.0)
        .execute(&s.pool)
        .await
        .unwrap();
    assert!(!s.owed(id).await, "nothing pages once the source is gone");
}

#[tokio::test]
#[ignore]
async fn an_alert_is_reminded_on_its_sources_interval_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let s = seeded(pool, "alertremind").await;
    let id = s.incident(false).await;
    s.notification(id, Some(s.channel), "webhook", "2 hours")
        .await;
    assert!(s.reminded(id).await, "due after the source's interval");

    s.set_source("renotify_interval_secs = 86400").await;
    assert!(!s.reminded(id).await, "the source's own interval decides");
    s.set_source("renotify_interval_secs = 3600, enabled = false")
        .await;
    assert!(!s.reminded(id).await, "paused");
}

#[tokio::test]
#[ignore]
async fn a_damped_alert_is_released_once_its_hold_runs_out_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let s = seeded(pool, "alertrelease").await;
    let id = s.incident(false).await;
    s.notification(id, None, "damped", "20 minutes").await;
    assert!(s.released(id).await, "a hold past its window is released");

    s.set_source("enabled = false").await;
    assert!(!s.released(id).await, "paused, it waits");
}
