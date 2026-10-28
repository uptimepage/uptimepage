//! The escalation engine paging a group that alerts as one, against Postgres:
//! routing at open, attach, the group's own page and recovery, the owed sweep,
//! parking, refusals on members, and members leaving by delete.

use crate::common;

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use sqlx::PgPool;
use url::Url;
use uuid::Uuid;

use uptimepage::config::EscalationConfig;
use uptimepage::domain::{
    AlertBinding, ChannelConfig, CheckSpec, CheckStatus, ExpectedStatus, MonitorGroupUpdate,
    NewMonitorGroup, NewNotificationChannel, NewTarget, NotificationReason, OrgId, TargetAlerts,
    WebhookConfig, WriteSource,
};
use uptimepage::error::{AppError, codes};
use uptimepage::escalation::{EngineDeps, EscalationEngine, IncidentSignal};
use uptimepage::public_status::{IncidentStore, NewOpenIncident, PgIncidentStore};
use uptimepage::storage::orgs::PgOrgDirectory;
use uptimepage::storage::{
    Actor, IncidentOpsStore, MonitorGroupStore, NotificationChannelStore, PgContactStore,
    PgEscalationPolicyStore, PgIncidentOpsStore, PgLinkedAppStore, PgMaintenanceStore,
    PgMonitorGroupStore, PgNotificationChannelStore, PgOnCallStore, PostgresTargetStore,
    TargetStore, create_org_with_owner,
};

use crate::common::MIGRATOR;

struct Fixture {
    pool: PgPool,
    db: String,
    org: OrgId,
    targets: PostgresTargetStore,
    groups: PgMonitorGroupStore,
    ops: PgIncidentOpsStore,
    writer: PgIncidentStore,
    engine: EscalationEngine,
}

impl Fixture {
    async fn new(prefix: &str) -> Option<Self> {
        let (url, db) = common::fresh_test_db(prefix).await?;
        let pool = common::open_test_pool(&url).await;
        MIGRATOR.run(&pool).await.unwrap();
        let user = common::make_user(&pool, "gp").await;
        let org = create_org_with_owner(&pool, user, &common::unique_slug("gp"), "Co")
            .await
            .unwrap()
            .unwrap()
            .id;
        let engine = engine(&pool);
        Some(Self {
            org,
            targets: PostgresTargetStore::from_pool(pool.clone(), None),
            groups: PgMonitorGroupStore::new(pool.clone()),
            ops: PgIncidentOpsStore::new(pool.clone()),
            writer: PgIncidentStore::new(pool.clone()),
            engine,
            pool,
            db,
        })
    }

    async fn done(self) {
        self.pool.close().await;
        common::drop_test_db(&self.db).await;
    }

    async fn channel(&self, name: &str) -> Uuid {
        PgNotificationChannelStore::new(self.pool.clone(), None)
            .create(
                self.org,
                NewNotificationChannel {
                    name: name.into(),
                    config: ChannelConfig::Webhook(WebhookConfig {
                        url: "http://127.0.0.1:1/notify".into(),
                        headers: Default::default(),
                        secret: None,
                    }),
                    enabled: true,
                    auto_bind_tags: Vec::new(),
                    acknowledge_button: false,
                    resolve_button: false,
                },
                WriteSource::Ui,
                i64::MAX,
                None,
            )
            .await
            .unwrap()
            .id
    }

    async fn group(&self, name: &str, alert_as_one: bool, channel: Uuid) -> Uuid {
        let mut new = NewMonitorGroup::named(name);
        new.alert_as_one = alert_as_one;
        new.alerts = TargetAlerts(vec![AlertBinding {
            channel_id: channel,
        }]);
        self.groups
            .create(self.org, new, WriteSource::Ui, None, i64::MAX)
            .await
            .unwrap()
            .id
    }

    async fn monitor(&self, name: &str, group: &str, own: Option<Uuid>) -> Uuid {
        let new = NewTarget {
            name: name.into(),
            check: CheckSpec::Http(common::default_http_check(
                Url::parse("https://example.com/").unwrap(),
                ExpectedStatus::Exact(200),
            )),
            interval: Duration::from_secs(30),
            enabled: true,
            tags: vec![],
            alerts: TargetAlerts(
                own.into_iter()
                    .map(|channel_id| AlertBinding { channel_id })
                    .collect(),
            ),
            region_policy: Default::default(),
            alert_confirmations: 2,
            notify_recovery: true,
            renotify_interval_secs: 3600,
            recovery_period_secs: None,
            group_name: Some(group.into()),
            group_id: None,
            owner_user_id: None,
            regions: None,
        };
        self.targets
            .create(
                self.org,
                new,
                WriteSource::Ui,
                None,
                uptimepage::domain::WriteCaps::UNLIMITED,
            )
            .await
            .unwrap()
            .id
    }

    /// Opens the monitor's incident as the writer does.
    async fn down(&self, target: Uuid) -> Uuid {
        self.open(target, CheckStatus::Down).await
    }

    async fn open(&self, target: Uuid, status: CheckStatus) -> Uuid {
        self.writer
            .insert_open(
                self.org,
                NewOpenIncident {
                    target_id: target,
                    started_at: Utc::now() - chrono::Duration::minutes(2),
                    status_at_start: status,
                    check_count: 2,
                    error_sample: Some("connection refused".into()),
                    region: None,
                    regions_down: Vec::new(),
                    regions_up: Vec::new(),
                },
            )
            .await
            .unwrap()
            .expect("opened")
    }

    async fn signal(&self, incident: Uuid, reason: NotificationReason) {
        self.engine
            .handle(IncidentSignal {
                org: self.org,
                incident_id: incident,
                reason,
            })
            .await
            .unwrap();
    }

    /// As a dropped signal leaves it: owed long enough for the sweep.
    async fn lose_signal(&self, incident: Uuid) {
        sqlx::query(
            "UPDATE incidents SET page_owed_at = now() - interval '1 hour' \
             WHERE id = $1 AND page_owed_at IS NOT NULL",
        )
        .bind(incident)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    async fn open_group_incident(&self, group: Uuid) -> Option<Uuid> {
        sqlx::query_scalar(
            "SELECT id FROM incidents WHERE org_id = $1 AND group_id = $2 \
             AND origin = 'group' AND ended_at IS NULL",
        )
        .bind(self.org.0)
        .bind(group)
        .fetch_optional(&self.pool)
        .await
        .unwrap()
    }

    /// `(reason, channel, member_to_seq)` of each row with a channel.
    async fn pages(&self, incident: Uuid) -> Vec<(String, Uuid, Option<i32>)> {
        sqlx::query_as(
            "SELECT reason, channel_id, member_to_seq FROM incident_notifications \
             WHERE incident_id = $1 AND channel_id IS NOT NULL ORDER BY created_at, id",
        )
        .bind(incident)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    async fn first_page(&self, incident: Uuid) -> Uuid {
        sqlx::query_scalar(
            "SELECT id FROM incident_notifications \
             WHERE incident_id = $1 AND channel_id IS NOT NULL ORDER BY created_at, id LIMIT 1",
        )
        .bind(incident)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    async fn column<T>(&self, incident: Uuid, column: &str) -> T
    where
        T: for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres> + Send + Unpin,
    {
        sqlx::query_scalar(&format!("SELECT {column} FROM incidents WHERE id = $1"))
            .bind(incident)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
}

fn engine(pool: &PgPool) -> EscalationEngine {
    let (_tx, rx) = tokio::sync::mpsc::channel(4);
    EscalationEngine::new(
        rx,
        EngineDeps {
            ops: Arc::new(PgIncidentOpsStore::new(pool.clone())),
            policies: Arc::new(PgEscalationPolicyStore::new(pool.clone())),
            on_call: Arc::new(PgOnCallStore::new(pool.clone())),
            contacts: Arc::new(PgContactStore::new(pool.clone())),
            linked_apps: Arc::new(PgLinkedAppStore::new(pool.clone())),
            targets: Arc::new(PostgresTargetStore::from_pool(pool.clone(), None)),
            groups: Arc::new(PgMonitorGroupStore::new(pool.clone())),
            channels: Arc::new(PgNotificationChannelStore::new(pool.clone(), None)),
            maintenance: Arc::new(PgMaintenanceStore::new(pool.clone())),
            orgs: Arc::new(PgOrgDirectory::new(pool.clone())),
            http: uptimepage::http_outbound::build_outbound_client(
                uptimepage::security::SsrfGuard::relaxed_for_tests(),
            ),
            cfg: EscalationConfig::default(),
            base_url: String::new(),
            alert_channel_stop_secret: String::new(),
            incident_ack_secret: String::new(),
            app_link_secret: String::new(),
            central_bot: None,
            pressed_apps: Vec::new(),
            central_whatsapp: None,
            email: None,
        },
    )
}

fn conflict(result: uptimepage::error::Result<impl std::fmt::Debug>) -> &'static str {
    match result {
        Err(AppError::Conflict { code, .. }) => code,
        other => panic!("expected a conflict, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_group_alerting_as_one_pages_once_and_recovers_once() {
    let Some(f) = Fixture::new("gp_once").await else {
        return;
    };
    let ops_channel = f.channel("ops").await;
    let own = f.channel("own").await;
    let group = f.group("DNS", true, ops_channel).await;
    let ns = f.monitor("ns", "DNS", Some(own)).await;
    let a = f.monitor("a", "DNS", Some(own)).await;

    let ns_down = f.down(ns).await;
    assert_eq!(
        f.column::<Option<Uuid>>(ns_down, "routed_group_id").await,
        Some(group)
    );
    f.signal(ns_down, NotificationReason::Opened).await;
    let incident = f.open_group_incident(group).await.expect("group incident");
    assert_eq!(
        f.pages(incident).await,
        vec![("opened".to_string(), ops_channel, Some(1))]
    );
    assert!(f.pages(ns_down).await.is_empty());
    let row = f.first_page(incident).await;
    assert_eq!(f.ops.notice_members(f.org, row).await.unwrap(), vec!["ns"]);

    let a_down = f.down(a).await;
    f.signal(a_down, NotificationReason::Opened).await;
    assert_eq!(f.pages(incident).await.len(), 1);
    assert!(f.pages(a_down).await.is_empty());
    assert_eq!(f.ops.notice_members(f.org, row).await.unwrap(), vec!["ns"]);

    let closed = f.writer.close(f.org, a_down, Utc::now()).await.unwrap();
    assert_eq!(closed.and_then(|c| c.group_incident), None);
    let closed = f.writer.close(f.org, ns_down, Utc::now()).await.unwrap();
    assert_eq!(closed.and_then(|c| c.group_incident), Some(incident));
    f.signal(incident, NotificationReason::Resolved).await;
    f.signal(ns_down, NotificationReason::Resolved).await;
    assert_eq!(
        f.pages(incident).await,
        vec![
            ("opened".to_string(), ops_channel, Some(1)),
            ("resolved".to_string(), ops_channel, None),
        ]
    );
    assert!(f.pages(ns_down).await.is_empty());
    assert!(f.pages(a_down).await.is_empty());
    f.done().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_lost_signal_still_reaches_the_group_through_the_owed_sweep() {
    let Some(f) = Fixture::new("gp_owed").await else {
        return;
    };
    let ops_channel = f.channel("ops").await;
    let group = f.group("DNS", true, ops_channel).await;
    let ns = f.monitor("ns", "DNS", None).await;
    let ns_down = f.down(ns).await;
    f.lose_signal(ns_down).await;
    f.engine.sweep_once().await;
    let incident = f.open_group_incident(group).await.expect("group incident");
    assert_eq!(f.pages(incident).await.len(), 1);
    assert_eq!(
        f.column::<Option<chrono::DateTime<Utc>>>(ns_down, "page_owed_at")
            .await,
        None
    );

    // The group's own page lost after the attach: the sweep pays it.
    let other = f.group("Mail", true, ops_channel).await;
    let mx = f.monitor("mx", "Mail", None).await;
    let mx_down = f.down(mx).await;
    f.ops.attach_member(f.org, mx_down).await.unwrap();
    let mail = f.open_group_incident(other).await.expect("group incident");
    assert!(f.pages(mail).await.is_empty());
    f.lose_signal(mail).await;
    f.engine.sweep_once().await;
    assert_eq!(f.pages(mail).await.len(), 1);
    f.done().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_route_is_fixed_when_the_incident_opens() {
    let Some(f) = Fixture::new("gp_route").await else {
        return;
    };
    let ops_channel = f.channel("ops").await;
    let own = f.channel("own").await;
    let group = f.group("DNS", false, ops_channel).await;
    let ns = f.monitor("ns", "DNS", Some(own)).await;
    let a = f.monitor("a", "DNS", Some(own)).await;
    let flip = |on: bool| MonitorGroupUpdate {
        alert_as_one: Some(on),
        ..Default::default()
    };

    let before = f.down(ns).await;
    f.groups
        .update(f.org, group, flip(true), WriteSource::Ui, None)
        .await
        .unwrap();
    f.signal(before, NotificationReason::Opened).await;
    assert_eq!(f.pages(before).await, vec![("opened".into(), own, None)]);
    assert_eq!(f.open_group_incident(group).await, None);

    let after = f.down(a).await;
    f.groups
        .update(f.org, group, flip(false), WriteSource::Ui, None)
        .await
        .unwrap();
    f.signal(after, NotificationReason::Opened).await;
    assert!(f.pages(after).await.is_empty());
    let incident = f.open_group_incident(group).await.expect("group incident");
    assert_eq!(f.pages(incident).await.len(), 1);
    f.done().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn ack_resolve_and_reopen_follow_the_group_incident() {
    let Some(f) = Fixture::new("gp_acts").await else {
        return;
    };
    let ops_channel = f.channel("ops").await;
    let group = f.group("DNS", true, ops_channel).await;
    let ns = f.monitor("ns", "DNS", None).await;
    let ns_down = f.down(ns).await;
    f.signal(ns_down, NotificationReason::Opened).await;
    let incident = f.open_group_incident(group).await.expect("group incident");

    let ack = f
        .ops
        .acknowledge(f.org, ns_down, Actor::System, None, None)
        .await;
    assert!(
        matches!(ack, Err(AppError::Conflict { code, .. }) if code == codes::USE_GROUP_INCIDENT)
    );
    assert_eq!(
        conflict(f.ops.resolve(f.org, ns_down, Actor::System, None).await),
        codes::USE_GROUP_INCIDENT
    );
    assert_eq!(
        conflict(
            f.ops
                .publish(f.org, incident, None, None, None, Actor::System)
                .await
        ),
        codes::GROUP_INCIDENT_INTERNAL
    );

    f.ops
        .resolve(f.org, incident, Actor::System, None)
        .await
        .unwrap();
    assert!(
        f.column::<Option<chrono::DateTime<Utc>>>(ns_down, "ended_at")
            .await
            .is_some()
    );
    assert_eq!(
        f.column::<Option<chrono::DateTime<Utc>>>(ns_down, "closing_notice_at")
            .await,
        None
    );
    assert_eq!(
        conflict(f.ops.reopen(f.org, incident, Actor::System, None).await),
        codes::GROUP_INCIDENT_NO_REOPEN
    );
    assert_eq!(
        conflict(f.ops.reopen(f.org, ns_down, Actor::System, None).await),
        codes::USE_GROUP_INCIDENT
    );
    f.done().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_group_waits_while_every_member_down_is_paused() {
    let Some(f) = Fixture::new("gp_park").await else {
        return;
    };
    let ops_channel = f.channel("ops").await;
    let group = f.group("DNS", true, ops_channel).await;
    let ns = f.monitor("ns", "DNS", None).await;
    let ns_down = f.down(ns).await;
    f.ops.attach_member(f.org, ns_down).await.unwrap();
    let incident = f.open_group_incident(group).await.expect("group incident");
    f.targets
        .set_enabled(f.org, &[ns], false, None)
        .await
        .unwrap();
    f.lose_signal(incident).await;
    f.engine.sweep_once().await;
    f.signal(incident, NotificationReason::Opened).await;
    assert!(f.pages(incident).await.is_empty());

    f.targets
        .set_enabled(f.org, &[ns], true, None)
        .await
        .unwrap();
    f.engine.sweep_once().await;
    assert_eq!(f.pages(incident).await.len(), 1);
    f.done().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn deleting_the_last_member_down_closes_the_group_incident_as_deleted() {
    let Some(f) = Fixture::new("gp_mdel").await else {
        return;
    };
    let ops_channel = f.channel("ops").await;
    let group = f.group("DNS", true, ops_channel).await;
    let ns = f.monitor("ns", "DNS", None).await;
    let a = f.monitor("a", "DNS", None).await;
    let ns_down = f.down(ns).await;
    let a_down = f.down(a).await;
    f.signal(ns_down, NotificationReason::Opened).await;
    f.signal(a_down, NotificationReason::Opened).await;
    let incident = f.open_group_incident(group).await.expect("group incident");

    f.targets.delete(f.org, a, None).await.unwrap();
    assert_eq!(
        f.column::<Option<chrono::DateTime<Utc>>>(a_down, "closing_notice_at")
            .await,
        None
    );
    assert!(f.open_group_incident(group).await.is_some());

    f.targets.delete(f.org, ns, None).await.unwrap();
    assert_eq!(f.open_group_incident(group).await, None);
    assert!(f.column::<bool>(incident, "closed_by_monitor_delete").await);
    // Told at once with the deleted monitors' own incidents.
    assert!(
        f.ops
            .closed_with_monitors(f.org, &[ns])
            .await
            .unwrap()
            .contains(&incident)
    );
    let read = f.ops.get(f.org, incident).await.unwrap().unwrap();
    assert_eq!(read.closing_reason(), NotificationReason::MonitorDeleted);
    f.signal(incident, NotificationReason::MonitorDeleted).await;
    assert_eq!(
        f.pages(incident).await.last().map(|p| p.0.clone()),
        Some("monitor_deleted".into())
    );
    f.done().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_member_detached_by_a_group_delete_pages_its_own_channels() {
    let Some(f) = Fixture::new("gp_detach").await else {
        return;
    };
    let ops_channel = f.channel("ops").await;
    let own = f.channel("own").await;
    let group = f.group("DNS", true, ops_channel).await;
    let ns = f.monitor("ns", "DNS", Some(own)).await;
    let ns_down = f.down(ns).await;
    f.signal(ns_down, NotificationReason::Opened).await;
    let incident = f.open_group_incident(group).await.expect("group incident");

    f.groups.delete(f.org, group, None).await.unwrap().unwrap();
    f.signal(incident, NotificationReason::GroupDeleted).await;
    assert_eq!(
        f.pages(incident).await.last().map(|p| p.0.clone()),
        Some("group_deleted".into())
    );
    f.lose_signal(ns_down).await;
    f.engine.sweep_once().await;
    assert_eq!(f.pages(ns_down).await, vec![("reopened".into(), own, None)]);
    // Acted on as its own incident again.
    f.ops
        .acknowledge(f.org, ns_down, Actor::System, None, None)
        .await
        .unwrap();
    f.done().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_group_policy_pages_its_levels_instead_of_the_channels() {
    let Some(f) = Fixture::new("gp_policy").await else {
        return;
    };
    let ops_channel = f.channel("ops").await;
    let pager = f.channel("pager").await;
    let group = f.group("DNS", true, ops_channel).await;
    let policies = PgEscalationPolicyStore::new(f.pool.clone());
    let policy = uptimepage::storage::EscalationPolicyStore::create(
        &policies,
        f.org,
        uptimepage::domain::NewEscalationPolicy {
            name: "p".into(),
            description: None,
            repeat_count: 0,
            steps: vec![uptimepage::domain::NewEscalationStep {
                level: 1,
                delay_secs: 300,
                targets: vec![uptimepage::domain::NewEscalationTarget {
                    target_type: uptimepage::domain::EscalationTargetType::Channel,
                    user_id: None,
                    schedule_id: None,
                    channel_id: Some(pager),
                }],
            }],
        },
        i64::MAX,
    )
    .await
    .unwrap();
    assert!(
        uptimepage::storage::EscalationPolicyStore::set_group_policy(
            &policies,
            f.org,
            group,
            Some(policy.id),
            None
        )
        .await
        .unwrap()
    );
    let ns = f.monitor("ns", "DNS", None).await;
    let ns_down = f.down(ns).await;
    f.signal(ns_down, NotificationReason::Opened).await;
    let incident = f.open_group_incident(group).await.expect("group incident");
    assert_eq!(
        f.pages(incident).await,
        vec![("opened".into(), pager, Some(1))]
    );
    assert_eq!(
        f.column::<Option<Uuid>>(incident, "escalation_policy_id")
            .await,
        Some(policy.id)
    );
    f.done().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_member_held_by_maintenance_joins_the_group_when_the_window_ends() {
    let Some(f) = Fixture::new("gp_maint").await else {
        return;
    };
    let ops_channel = f.channel("ops").await;
    let group = f.group("DNS", true, ops_channel).await;
    let ns = f.monitor("ns", "DNS", None).await;
    let window: Uuid = sqlx::query_scalar(
        "INSERT INTO maintenance_windows (org_id, title, starts_at, ends_at, suppress_alerts) \
         VALUES ($1, 'w', now() - interval '5 minutes', now() + interval '1 hour', true) \
         RETURNING id",
    )
    .bind(f.org.0)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO maintenance_window_components (org_id, maintenance_id, target_id) \
         VALUES ($1, $2, $3)",
    )
    .bind(f.org.0)
    .bind(window)
    .bind(ns)
    .execute(&f.pool)
    .await
    .unwrap();

    let ns_down = f.down(ns).await;
    f.signal(ns_down, NotificationReason::Opened).await;
    assert_eq!(f.open_group_incident(group).await, None);

    sqlx::query(
        "UPDATE maintenance_windows SET ends_at = now() - interval '1 second' WHERE id = $1",
    )
    .bind(window)
    .execute(&f.pool)
    .await
    .unwrap();
    f.engine.sweep_once().await;
    let incident = f.open_group_incident(group).await.expect("group incident");
    assert_eq!(f.pages(incident).await.len(), 1);
    let due = f.ops.due_for_maintenance_release(100).await.unwrap();
    assert!(due.iter().all(|d| d.id != ns_down));
    f.done().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_group_reminds_on_its_own_interval_naming_who_is_down() {
    let Some(f) = Fixture::new("gp_remind").await else {
        return;
    };
    let ops_channel = f.channel("ops").await;
    let group = f.group("DNS", true, ops_channel).await;
    f.groups
        .update(
            f.org,
            group,
            MonitorGroupUpdate {
                renotify_interval_secs: Some(60),
                ..Default::default()
            },
            WriteSource::Ui,
            None,
        )
        .await
        .unwrap();
    let ns = f.monitor("ns", "DNS", None).await;
    let ns_down = f.down(ns).await;
    f.signal(ns_down, NotificationReason::Opened).await;
    let incident = f.open_group_incident(group).await.expect("group incident");
    sqlx::query(
        "UPDATE incident_notifications SET created_at = now() - interval '2 minutes' \
         WHERE incident_id = $1",
    )
    .bind(incident)
    .execute(&f.pool)
    .await
    .unwrap();
    f.engine.sweep_once().await;
    let pages = f.pages(incident).await;
    assert_eq!(pages.len(), 2);
    assert_eq!(pages[1], ("reminder".into(), ops_channel, None));
    let reminder: Uuid = sqlx::query_scalar(
        "SELECT id FROM incident_notifications WHERE incident_id = $1 AND reason = 'reminder'",
    )
    .bind(incident)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        f.ops.notice_members(f.org, reminder).await.unwrap(),
        vec!["ns"]
    );
    f.done().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_members_close_waits_for_whoever_holds_its_group() {
    let Some(f) = Fixture::new("gp_wait").await else {
        return;
    };
    let ops_channel = f.channel("ops").await;
    let group = f.group("DNS", true, ops_channel).await;
    let ns = f.monitor("ns", "DNS", None).await;
    let ns_down = f.down(ns).await;
    f.ops.attach_member(f.org, ns_down).await.unwrap();
    let incident = f.open_group_incident(group).await.expect("group incident");

    let mut held = f.pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM monitor_groups WHERE id = $1 FOR NO KEY UPDATE")
        .bind(group)
        .execute(&mut *held)
        .await
        .unwrap();
    let writer = PgIncidentStore::new(f.pool.clone());
    let org = f.org;
    let close = tokio::spawn(async move { writer.close(org, ns_down, Utc::now()).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!close.is_finished());
    held.commit().await.unwrap();
    let closed = close.await.unwrap().unwrap();
    assert_eq!(closed.and_then(|c| c.group_incident), Some(incident));
    f.done().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn resolving_a_group_incident_while_members_attach_never_deadlocks() {
    let Some(f) = Fixture::new("gp_race").await else {
        return;
    };
    let ops_channel = f.channel("ops").await;
    let group = f.group("DNS", true, ops_channel).await;
    for round in 0..8 {
        let first = f.monitor(&format!("first{round}"), "DNS", None).await;
        let second = f.monitor(&format!("second{round}"), "DNS", None).await;
        let first_down = f.down(first).await;
        f.ops.attach_member(f.org, first_down).await.unwrap();
        let incident = f.open_group_incident(group).await.expect("group incident");
        let second_down = f.down(second).await;
        let (resolved, attached) = tokio::join!(
            f.ops.resolve(f.org, incident, Actor::System, None),
            f.ops.attach_member(f.org, second_down),
        );
        resolved.unwrap();
        attached.unwrap();
        // Whichever landed first, nothing is left attached to a closed group incident.
        let stranded: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM incidents m JOIN incidents g ON g.id = m.group_incident_id \
             WHERE m.ended_at IS NULL AND g.ended_at IS NOT NULL",
        )
        .fetch_one(&f.pool)
        .await
        .unwrap();
        assert_eq!(stranded, 0);
        if let Some(open) = f.open_group_incident(group).await {
            f.ops
                .resolve(f.org, open, Actor::System, None)
                .await
                .unwrap();
        }
    }
    f.done().await;
}

impl Fixture {
    /// As if the group's last word about members went out a minute ago.
    async fn wait_out_the_minute(&self, incident: Uuid) {
        sqlx::query(
            "UPDATE incidents SET last_member_notice_at = last_member_notice_at - interval '61 seconds' \
             WHERE id = $1",
        )
        .bind(incident)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    async fn digests(&self, incident: Uuid) -> Vec<(Uuid, Option<i32>, Option<i32>)> {
        sqlx::query_as(
            "SELECT id, member_from_seq, member_to_seq FROM incident_notifications \
             WHERE incident_id = $1 AND reason = 'member_down' ORDER BY created_at, id",
        )
        .bind(incident)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn members_who_fail_after_the_first_page_share_one_digest_a_minute_later() {
    let Some(f) = Fixture::new("gp_digest").await else {
        return;
    };
    let ops_channel = f.channel("ops").await;
    let group = f.group("DNS", true, ops_channel).await;
    let ns = f.monitor("ns", "DNS", None).await;
    let a = f.monitor("a", "DNS", None).await;
    let mx = f.monitor("mx", "DNS", None).await;
    let ns_down = f.down(ns).await;
    f.signal(ns_down, NotificationReason::Opened).await;
    let incident = f.open_group_incident(group).await.expect("group incident");
    for target in [a, mx] {
        let down = f.down(target).await;
        f.signal(down, NotificationReason::Opened).await;
    }
    f.engine.sweep_once().await;
    assert!(
        f.digests(incident).await.is_empty(),
        "not within the minute"
    );

    f.wait_out_the_minute(incident).await;
    f.engine.sweep_once().await;
    let digests = f.digests(incident).await;
    assert_eq!(digests.len(), 1);
    let (row, from, to) = digests[0];
    assert_eq!((from, to), (Some(2), Some(3)));
    assert_eq!(
        f.ops.notice_members(f.org, row).await.unwrap(),
        vec!["a", "mx"]
    );
    assert_eq!(f.column::<i32>(incident, "member_notice_seq").await, 3);

    f.wait_out_the_minute(incident).await;
    f.engine.sweep_once().await;
    assert_eq!(f.digests(incident).await.len(), 1, "nothing new to say");
    f.done().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_digest_passes_over_members_back_up_and_waits_when_alert_again_is_off() {
    let Some(f) = Fixture::new("gp_quiet").await else {
        return;
    };
    let ops_channel = f.channel("ops").await;
    let group = f.group("DNS", true, ops_channel).await;
    let ns = f.monitor("ns", "DNS", None).await;
    let a = f.monitor("a", "DNS", None).await;
    let mx = f.monitor("mx", "DNS", None).await;
    let ns_down = f.down(ns).await;
    f.signal(ns_down, NotificationReason::Opened).await;
    let incident = f.open_group_incident(group).await.expect("group incident");
    let a_down = f.down(a).await;
    f.signal(a_down, NotificationReason::Opened).await;
    f.writer.close(f.org, a_down, Utc::now()).await.unwrap();
    f.wait_out_the_minute(incident).await;
    let told: chrono::DateTime<Utc> = f.column(incident, "last_member_notice_at").await;
    f.engine.sweep_once().await;
    assert!(f.digests(incident).await.is_empty());
    assert_eq!(f.column::<i32>(incident, "member_notice_seq").await, 2);
    assert_eq!(
        f.column::<chrono::DateTime<Utc>>(incident, "last_member_notice_at")
            .await,
        told
    );

    f.groups
        .update(
            f.org,
            group,
            MonitorGroupUpdate {
                alert_again: Some(false),
                ..Default::default()
            },
            WriteSource::Ui,
            None,
        )
        .await
        .unwrap();
    let mx_down = f.down(mx).await;
    f.signal(mx_down, NotificationReason::Opened).await;
    f.engine.sweep_once().await;
    assert!(f.digests(incident).await.is_empty());
    assert_eq!(f.column::<i32>(incident, "member_notice_seq").await, 2);
    f.done().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn members_attached_before_the_first_page_are_named_in_it() {
    let Some(f) = Fixture::new("gp_first").await else {
        return;
    };
    let ops_channel = f.channel("ops").await;
    let group = f.group("DNS", true, ops_channel).await;
    let ns = f.monitor("ns", "DNS", None).await;
    let a = f.monitor("a", "DNS", None).await;
    let ns_down = f.down(ns).await;
    let a_down = f.down(a).await;
    f.ops.attach_member(f.org, ns_down).await.unwrap();
    f.ops.attach_member(f.org, a_down).await.unwrap();
    let incident = f.open_group_incident(group).await.expect("group incident");
    f.signal(incident, NotificationReason::Opened).await;
    assert_eq!(
        f.pages(incident).await,
        vec![("opened".into(), ops_channel, Some(2))]
    );
    let row = f.first_page(incident).await;
    assert_eq!(
        f.ops.notice_members(f.org, row).await.unwrap(),
        vec!["ns", "a"]
    );
    f.wait_out_the_minute(incident).await;
    f.engine.sweep_once().await;
    assert!(f.digests(incident).await.is_empty());
    f.done().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn two_engines_finding_the_same_digest_due_send_it_once() {
    let Some(f) = Fixture::new("gp_twice").await else {
        return;
    };
    let ops_channel = f.channel("ops").await;
    let group = f.group("DNS", true, ops_channel).await;
    let ns = f.monitor("ns", "DNS", None).await;
    let a = f.monitor("a", "DNS", None).await;
    let ns_down = f.down(ns).await;
    f.signal(ns_down, NotificationReason::Opened).await;
    let incident = f.open_group_incident(group).await.expect("group incident");
    let a_down = f.down(a).await;
    f.ops.attach_member(f.org, a_down).await.unwrap();
    f.wait_out_the_minute(incident).await;
    let other = engine(&f.pool);
    tokio::join!(f.engine.sweep_once(), other.sweep_once());
    assert_eq!(f.digests(incident).await.len(), 1);
    f.done().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn after_an_empty_first_rung_the_first_page_that_reaches_anyone_names_the_members() {
    use uptimepage::domain::{
        EscalationTargetType, NewEscalationPolicy, NewEscalationStep, NewEscalationTarget,
    };
    use uptimepage::storage::EscalationPolicyStore;
    let Some(f) = Fixture::new("gp_rung").await else {
        return;
    };
    let ops_channel = f.channel("ops").await;
    let off = f.channel("off").await;
    PgNotificationChannelStore::new(f.pool.clone(), None)
        .update(
            f.org,
            off,
            uptimepage::domain::NotificationChannelUpdate {
                enabled: Some(false),
                ..Default::default()
            },
            WriteSource::Ui,
            None,
        )
        .await
        .unwrap();
    let pager = f.channel("pager").await;
    let group = f.group("DNS", true, ops_channel).await;
    let step = |level, channel| NewEscalationStep {
        level,
        delay_secs: 60,
        targets: vec![NewEscalationTarget {
            target_type: EscalationTargetType::Channel,
            user_id: None,
            schedule_id: None,
            channel_id: Some(channel),
        }],
    };
    let policies = PgEscalationPolicyStore::new(f.pool.clone());
    let policy = policies
        .create(
            f.org,
            NewEscalationPolicy {
                name: "p".into(),
                description: None,
                repeat_count: 0,
                steps: vec![step(1, off), step(2, pager)],
            },
            i64::MAX,
        )
        .await
        .unwrap();
    policies
        .set_group_policy(f.org, group, Some(policy.id), None)
        .await
        .unwrap();
    let ns = f.monitor("ns", "DNS", None).await;
    let ns_down = f.down(ns).await;
    f.signal(ns_down, NotificationReason::Opened).await;
    let incident = f.open_group_incident(group).await.expect("group incident");
    assert!(f.pages(incident).await.is_empty());
    assert_eq!(
        f.column::<Option<chrono::DateTime<Utc>>>(incident, "last_member_notice_at")
            .await,
        None
    );

    sqlx::query(
        "UPDATE incidents SET next_escalation_at = now() - interval '1 second' WHERE id = $1",
    )
    .bind(incident)
    .execute(&f.pool)
    .await
    .unwrap();
    f.engine.sweep_once().await;
    assert_eq!(
        f.pages(incident).await,
        vec![("escalated".into(), pager, Some(1))]
    );
    assert!(
        f.column::<Option<chrono::DateTime<Utc>>>(incident, "last_member_notice_at")
            .await
            .is_some()
    );
    f.done().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_digest_waits_while_every_member_down_is_paused() {
    let Some(f) = Fixture::new("gp_dpark").await else {
        return;
    };
    let ops_channel = f.channel("ops").await;
    let group = f.group("DNS", true, ops_channel).await;
    let ns = f.monitor("ns", "DNS", None).await;
    let a = f.monitor("a", "DNS", None).await;
    let ns_down = f.down(ns).await;
    f.signal(ns_down, NotificationReason::Opened).await;
    let incident = f.open_group_incident(group).await.expect("group incident");
    let a_down = f.down(a).await;
    f.signal(a_down, NotificationReason::Opened).await;
    f.targets
        .set_enabled(f.org, &[ns, a], false, None)
        .await
        .unwrap();
    f.wait_out_the_minute(incident).await;
    f.engine.sweep_once().await;
    assert!(f.digests(incident).await.is_empty());

    f.targets
        .set_enabled(f.org, &[a], true, None)
        .await
        .unwrap();
    f.engine.sweep_once().await;
    assert_eq!(f.digests(incident).await.len(), 1);
    f.done().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn deleting_a_groups_policy_falls_back_to_the_org_default() {
    use uptimepage::domain::{
        EscalationTargetType, NewEscalationPolicy, NewEscalationStep, NewEscalationTarget,
    };
    use uptimepage::storage::EscalationPolicyStore;
    let Some(f) = Fixture::new("gp_poldel").await else {
        return;
    };
    let ops = f.channel("ops").await;
    let group = f.group("DNS", true, ops).await;
    let policies = PgEscalationPolicyStore::new(f.pool.clone());
    let policy = |name: &str| NewEscalationPolicy {
        name: name.into(),
        description: None,
        repeat_count: 0,
        steps: vec![NewEscalationStep {
            level: 1,
            delay_secs: 60,
            targets: vec![NewEscalationTarget {
                target_type: EscalationTargetType::Channel,
                user_id: None,
                schedule_id: None,
                channel_id: Some(ops),
            }],
        }],
    };
    let bound = policies
        .create(f.org, policy("bound"), i64::MAX)
        .await
        .unwrap();
    let default = policies
        .create(f.org, policy("default"), i64::MAX)
        .await
        .unwrap();
    policies
        .set_org_default(f.org, Some(default.id))
        .await
        .unwrap();
    policies
        .set_group_policy(f.org, group, Some(bound.id), None)
        .await
        .unwrap();
    assert_eq!(
        policies.resolve_for_group(f.org, group).await.unwrap(),
        Some(bound.id)
    );

    assert!(policies.delete(f.org, bound.id).await.unwrap());
    assert_eq!(
        policies.resolve_for_group(f.org, group).await.unwrap(),
        Some(default.id)
    );

    // A binding that committed behind the delete's clear still falls through.
    sqlx::query("UPDATE monitor_groups SET escalation_policy_id = $1 WHERE id = $2")
        .bind(bound.id)
        .bind(group)
        .execute(&f.pool)
        .await
        .unwrap();
    assert_eq!(
        policies.resolve_for_group(f.org, group).await.unwrap(),
        Some(default.id)
    );
    f.done().await;
}

/// A group delete locks the group and then its members. An incident opening
/// for a member must take the group first too, or the two deadlock.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn an_opening_waits_on_a_group_delete_without_deadlocking() {
    let Some(f) = Fixture::new("gp_dlock").await else {
        return;
    };
    let ops = f.channel("ops").await;
    let group = f.group("DNS", true, ops).await;
    let ns = f.monitor("ns", "DNS", None).await;

    let mut delete = f.pool.begin().await.unwrap();
    let delete_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *delete)
        .await
        .unwrap();
    sqlx::query("SELECT 1 FROM monitor_groups WHERE id = $1 FOR UPDATE")
        .bind(group)
        .execute(&mut *delete)
        .await
        .unwrap();
    let writer = PgIncidentStore::new(f.pool.clone());
    let org = f.org;
    let opening = tokio::spawn(async move {
        writer
            .insert_open(
                org,
                NewOpenIncident {
                    target_id: ns,
                    started_at: Utc::now() - chrono::Duration::minutes(2),
                    status_at_start: CheckStatus::Down,
                    check_count: 2,
                    error_sample: None,
                    region: None,
                    regions_down: Vec::new(),
                    regions_up: Vec::new(),
                },
            )
            .await
    });
    let waiting = "SELECT count(*) FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid))";
    let mut blocked = false;
    for _ in 0..250 {
        if sqlx::query_scalar::<_, i64>(waiting)
            .bind(delete_pid)
            .fetch_one(&f.pool)
            .await
            .unwrap()
            > 0
        {
            blocked = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(blocked, "the opening never queued behind the delete");
    sqlx::query("SET LOCAL lock_timeout = '3s'")
        .execute(&mut *delete)
        .await
        .unwrap();
    sqlx::query("SELECT 1 FROM targets WHERE id = $1 FOR UPDATE")
        .bind(ns)
        .execute(&mut *delete)
        .await
        .expect("the delete takes the member while the opening waits on the group");
    delete.rollback().await.unwrap();
    assert!(opening.await.unwrap().unwrap().is_some());
    f.done().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_group_incident_a_degraded_member_opened_turns_down_with_its_members() {
    let Some(f) = Fixture::new("gp_raise").await else {
        return;
    };
    let ops = f.channel("ops").await;
    let group = f.group("DNS", true, ops).await;
    let a = f.monitor("a", "DNS", None).await;

    let a_inc = f.open(a, CheckStatus::Degraded).await;
    f.signal(a_inc, NotificationReason::Opened).await;
    let incident = f.open_group_incident(group).await.expect("group incident");
    assert_eq!(
        f.column::<String>(incident, "status_at_start").await,
        "degraded"
    );

    f.writer.escalate(f.org, a_inc, None).await.unwrap();
    assert_eq!(
        f.column::<String>(incident, "status_at_start").await,
        "down"
    );
    f.done().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_member_joining_down_raises_a_degraded_group_incident() {
    let Some(f) = Fixture::new("gp_join").await else {
        return;
    };
    let ops = f.channel("ops").await;
    let group = f.group("DNS", true, ops).await;
    let a = f.monitor("a", "DNS", None).await;
    let b = f.monitor("b", "DNS", None).await;

    let a_inc = f.open(a, CheckStatus::Degraded).await;
    f.signal(a_inc, NotificationReason::Opened).await;
    let incident = f.open_group_incident(group).await.expect("group incident");
    let b_inc = f.down(b).await;
    f.signal(b_inc, NotificationReason::Opened).await;
    assert_eq!(
        f.column::<String>(incident, "status_at_start").await,
        "down"
    );
    f.done().await;
}
