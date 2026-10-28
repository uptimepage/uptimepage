//! Live-PG coverage for monitor groups: the name policy shared with the
//! database, the frozen label column, group writes from monitors and of their
//! own, the incident a group that alerts as one shares, and its delete.

use crate::common;

use std::time::Duration;

use sqlx::PgPool;
use url::Url;
use uuid::Uuid;

use uptimepage::domain::{
    ChannelConfig, CheckSpec, ExpectedStatus, MonitorGroupUpdate, NewMonitorGroup,
    NewNotificationChannel, NewTarget, OrgId, SlackConfig, TargetAlerts, TargetUpdate, WriteSource,
};
use uptimepage::error::{AppError, codes};
use uptimepage::public_status::{Closed, IncidentStore, PgIncidentStore};
use uptimepage::storage::group_incidents::{Attach, attach_member};
use uptimepage::storage::monitor_groups::{MonitorGroupStore, PgMonitorGroupStore};
use uptimepage::storage::{
    IncidentOpsStore, NotificationChannelStore, PgIncidentOpsStore, PgNotificationChannelStore,
    PostgresTargetStore, TargetFilter, TargetStore, create_org_with_owner,
};

use crate::common::MIGRATOR;

struct Fixture {
    pool: PgPool,
    db: String,
    org: OrgId,
    targets: PostgresTargetStore,
    groups: PgMonitorGroupStore,
}

impl Fixture {
    async fn new(prefix: &str) -> Option<Self> {
        let (url, db) = common::fresh_test_db(prefix).await?;
        let pool = common::open_test_pool(&url).await;
        MIGRATOR.run(&pool).await.unwrap();
        Some(Self {
            org: org(&pool, "grp").await,
            targets: PostgresTargetStore::from_pool(pool.clone(), None),
            groups: PgMonitorGroupStore::new(pool.clone()),
            pool,
            db,
        })
    }

    async fn done(self) {
        self.pool.close().await;
        common::drop_test_db(&self.db).await;
    }

    async fn monitor(&self, name: &str, group: Option<&str>) -> uptimepage::domain::Target {
        let new = NewTarget {
            name: name.into(),
            check: CheckSpec::Http(common::default_http_check(
                Url::parse("https://example.com/").unwrap(),
                ExpectedStatus::Exact(200),
            )),
            interval: Duration::from_secs(30),
            enabled: true,
            tags: vec![],
            alerts: Default::default(),
            region_policy: Default::default(),
            alert_confirmations: 2,
            notify_recovery: true,
            renotify_interval_secs: 3600,
            recovery_period_secs: None,
            group_name: group.map(str::to_owned),
            group_id: None,
            owner_user_id: None,
            regions: None,
        };
        self.targets
            .create(self.org, new, WriteSource::Ui, None, i64::MAX, i64::MAX)
            .await
            .unwrap()
    }

    async fn channel(&self, name: &str) -> Uuid {
        let new = NewNotificationChannel {
            name: name.into(),
            config: ChannelConfig::Slack(SlackConfig {
                webhook_url: format!("https://hooks.slack.com/services/T/B/{name}"),
                mention: None,
            }),
            enabled: true,
            auto_bind_tags: Vec::new(),
            acknowledge_button: true,
            resolve_button: false,
        };
        PgNotificationChannelStore::new(self.pool.clone(), None)
            .create(self.org, new, WriteSource::Ui, i64::MAX, None)
            .await
            .unwrap()
            .id
    }

    /// A member incident as the writer opens one routed to `group`.
    async fn routed_incident(&self, target: Uuid, group: Uuid, ago_mins: i32) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO incidents \
                 (org_id, target_id, started_at, status_at_start, routed_group_id, page_owed_at) \
             VALUES ($1, $2, now() - make_interval(mins => $4), 'down', $3, now()) RETURNING id",
        )
        .bind(self.org.0)
        .bind(target)
        .bind(group)
        .bind(ago_mins)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    async fn alerting_group(&self, name: &str) -> Uuid {
        let mut new = NewMonitorGroup::named(name);
        new.alert_as_one = true;
        self.groups
            .create(self.org, new, WriteSource::Ui, None, i64::MAX)
            .await
            .unwrap()
            .id
    }

    async fn audit_count(&self, action: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM org_audit_log WHERE org_id = $1 AND action = $2")
            .bind(self.org.0)
            .bind(action)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn event_kinds(&self, incident: Uuid) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT kind FROM incident_events WHERE org_id = $1 AND incident_id = $2 \
             ORDER BY occurred_at, id",
        )
        .bind(self.org.0)
        .bind(incident)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }
}

async fn org(pool: &PgPool, prefix: &str) -> OrgId {
    let user = common::make_user(pool, prefix).await;
    create_org_with_owner(pool, user, &common::unique_slug(prefix), "Co")
        .await
        .unwrap()
        .unwrap()
        .id
}

#[derive(sqlx::FromRow, Debug)]
struct IncidentRow {
    ended_at: Option<chrono::DateTime<chrono::Utc>>,
    closing_notice_at: Option<chrono::DateTime<chrono::Utc>>,
    page_owed_at: Option<chrono::DateTime<chrono::Utc>>,
    group_id: Option<Uuid>,
    group_ref: Option<Uuid>,
    group_name: Option<String>,
    routed_group_id: Option<Uuid>,
    group_incident_id: Option<Uuid>,
    group_member_seq: Option<i32>,
    member_seq: i32,
    closed_by_group_delete: bool,
    detached_at: Option<chrono::DateTime<chrono::Utc>>,
}

async fn incident(pool: &PgPool, id: Uuid) -> IncidentRow {
    sqlx::query_as(
        "SELECT ended_at, closing_notice_at, page_owed_at, group_id, group_ref, group_name, \
                routed_group_id, group_incident_id, group_member_seq, member_seq, \
                closed_by_group_delete, detached_at \
         FROM incidents WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Every Unicode White_Space character, the set `str::trim` strips.
const WHITE_SPACE: &[char] = &[
    '\u{0009}', '\u{000A}', '\u{000B}', '\u{000C}', '\u{000D}', '\u{0020}', '\u{0085}', '\u{00A0}',
    '\u{1680}', '\u{2000}', '\u{2001}', '\u{2002}', '\u{2003}', '\u{2004}', '\u{2005}', '\u{2006}',
    '\u{2007}', '\u{2008}', '\u{2009}', '\u{200A}', '\u{2028}', '\u{2029}', '\u{202F}', '\u{205F}',
    '\u{3000}',
];

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_database_trims_a_group_name_as_str_trim_does() {
    let Some(f) = Fixture::new("grp_trim").await else {
        return;
    };
    let kept = ['\u{200B}', '\u{FEFF}', '\u{180E}', 'x'];
    for c in WHITE_SPACE.iter().chain(&kept) {
        assert_eq!(c.is_whitespace(), WHITE_SPACE.contains(c), "{:?}", c);
        let raw = format!("{c}{c}DNS{c}");
        let trimmed: String = sqlx::query_scalar("SELECT group_name_trim($1)")
            .bind(&raw)
            .fetch_one(&f.pool)
            .await
            .unwrap();
        assert_eq!(trimmed, raw.trim(), "U+{:04X}", *c as u32);
    }
    f.done().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_label_column_is_frozen_and_reads_come_from_the_group() {
    let Some(f) = Fixture::new("grp_frozen").await else {
        return;
    };
    let insert = |name: Option<&'static str>| {
        sqlx::query(
            "INSERT INTO targets (org_id, name, check_spec, interval_secs, group_name) \
             VALUES ($1, 'old build', '{}'::jsonb, 30, $2)",
        )
        .bind(f.org.0)
        .bind(name)
        .execute(&f.pool)
    };
    assert!(insert(Some("DNS")).await.is_err());
    insert(None).await.unwrap();

    let ns = f.monitor("ns", Some("DNS")).await;
    let renamed = sqlx::query("UPDATE targets SET group_name = 'Other' WHERE id = $1")
        .bind(ns.id)
        .execute(&f.pool)
        .await;
    assert!(renamed.is_err());
    sqlx::query("UPDATE targets SET name = 'ns2', group_name = group_name WHERE id = $1")
        .bind(ns.id)
        .execute(&f.pool)
        .await
        .unwrap();

    let group = ns.group_id.expect("joined a group");
    f.groups
        .update(
            f.org,
            group,
            MonitorGroupUpdate {
                name: Some("example.com DNS".into()),
                ..Default::default()
            },
            WriteSource::Ui,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    let read = f.targets.get(f.org, ns.id).await.unwrap().unwrap();
    assert_eq!(read.group_name.as_deref(), Some("example.com DNS"));
    let column: Option<String> = sqlx::query_scalar("SELECT group_name FROM targets WHERE id = $1")
        .bind(ns.id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(column, None);
    f.done().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_monitor_write_finds_or_creates_its_group_by_trimmed_name() {
    let Some(f) = Fixture::new("grp_resolve").await else {
        return;
    };
    let ns = f.monitor("ns", Some("DNS ")).await;
    let a = f.monitor("a", Some("\u{00A0}DNS")).await;
    let blank = f.monitor("blank", Some(" \u{3000}")).await;
    assert_eq!(ns.group_name.as_deref(), Some("DNS"));
    assert!(ns.group_id.is_some());
    assert_eq!(ns.group_id, a.group_id);
    assert_eq!((blank.group_id, blank.group_name), (None, None));
    assert_eq!(f.audit_count("group.created").await, 1);

    let moved = f
        .targets
        .update(
            f.org,
            a.id,
            TargetUpdate {
                group_name: Some(Some("Mail".into())),
                ..Default::default()
            },
            Some(WriteSource::Api),
            None,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(moved.group_name.as_deref(), Some("Mail"));
    assert_eq!(f.audit_count("target.group_changed").await, 1);

    let hit = f
        .targets
        .set_group(f.org, &[ns.id, a.id], None, None)
        .await
        .unwrap();
    assert_eq!(hit.len(), 2);
    assert_eq!(f.audit_count("target.group_changed").await, 2);
    assert_eq!(
        f.targets.distinct_groups(f.org).await.unwrap(),
        vec!["DNS".to_string(), "Mail".to_string()]
    );
    let nobody = f
        .targets
        .set_group(f.org, &[Uuid::now_v7()], Some("Typo"), None)
        .await
        .unwrap();
    assert!(nobody.is_empty());
    assert_eq!(f.audit_count("group.created").await, 2);

    f.targets
        .set_group(f.org, &[ns.id], Some("  Mail "), None)
        .await
        .unwrap();
    let in_mail = f
        .targets
        .list(
            f.org,
            TargetFilter {
                group: Some("Mail".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        in_mail.iter().map(|t| t.id).collect::<Vec<_>>(),
        vec![ns.id]
    );

    let too_long = f
        .targets
        .set_group(f.org, &[ns.id], Some(&"x".repeat(51)), None)
        .await;
    assert!(
        matches!(too_long, Err(AppError::BadRequest { code, .. }) if code == codes::GROUP_TOO_LONG)
    );
    f.done().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_group_keeps_its_own_settings_and_channels() {
    let Some(f) = Fixture::new("grp_crud").await else {
        return;
    };
    let ops = f.channel("ops").await;
    let pager = f.channel("pager").await;
    let mut new = NewMonitorGroup::named("  example.com DNS ");
    new.alert_as_one = true;
    new.alerts = TargetAlerts(vec![uptimepage::domain::AlertBinding { channel_id: ops }]);
    let group = f
        .groups
        .create(f.org, new, WriteSource::Terraform, None, i64::MAX)
        .await
        .unwrap();
    assert_eq!(group.name, "example.com DNS");
    assert_eq!(group.write_source, WriteSource::Terraform);

    let taken = f
        .groups
        .create(
            f.org,
            NewMonitorGroup::named("example.com DNS"),
            WriteSource::Ui,
            None,
            i64::MAX,
        )
        .await;
    assert!(
        matches!(taken, Err(AppError::Conflict { code, .. }) if code == codes::GROUP_NAME_TAKEN)
    );
    let blank = f
        .groups
        .create(
            f.org,
            NewMonitorGroup::named("\u{2003}"),
            WriteSource::Ui,
            None,
            i64::MAX,
        )
        .await;
    assert!(
        matches!(blank, Err(AppError::BadRequest { code, .. }) if code == codes::INVALID_GROUP_NAME)
    );

    let updated = f
        .groups
        .update(
            f.org,
            group.id,
            MonitorGroupUpdate {
                alert_again: Some(false),
                renotify_interval_secs: Some(1800),
                alerts: Some(TargetAlerts(vec![
                    uptimepage::domain::AlertBinding { channel_id: pager },
                    uptimepage::domain::AlertBinding { channel_id: ops },
                ])),
                ..Default::default()
            },
            WriteSource::Ui,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(!updated.alert_again);
    assert_eq!(updated.renotify_interval_secs, 1800);
    let bound = |g: &uptimepage::domain::MonitorGroup| {
        g.alerts.iter().map(|b| b.channel_id).collect::<Vec<_>>()
    };
    assert_eq!(bound(&updated), vec![pager, ops]);
    let meta: serde_json::Value = sqlx::query_scalar(
        "SELECT metadata FROM org_audit_log WHERE org_id = $1 AND action = 'group.updated'",
    )
    .bind(f.org.0)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        meta["changed"],
        serde_json::json!(["alerts", "renotify_interval_secs", "alert_again"])
    );

    PgNotificationChannelStore::new(f.pool.clone(), None)
        .delete(f.org, pager, None)
        .await
        .unwrap();
    let read = f.groups.get(f.org, group.id).await.unwrap().unwrap();
    assert_eq!(bound(&read), vec![ops]);

    let ns = f.monitor("ns", Some("example.com DNS")).await;
    assert_eq!(ns.group_id, Some(group.id));
    let deleted = f
        .groups
        .delete(f.org, group.id, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((deleted.members, deleted.closed_incident), (1, None));
    let ns = f.targets.get(f.org, ns.id).await.unwrap().unwrap();
    assert_eq!((ns.group_id, ns.group_name), (None, None));
    assert!(f.groups.list(f.org).await.unwrap().is_empty());
    f.done().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn members_share_one_group_incident_numbered_in_order() {
    let Some(f) = Fixture::new("grp_attach").await else {
        return;
    };
    let group = f.alerting_group("DNS").await;
    let ns = f.monitor("ns", Some("DNS")).await;
    let a = f.monitor("a", Some("DNS")).await;
    let alone = f.monitor("alone", None).await;
    let ns_down = f.routed_incident(ns.id, group, 10).await;
    let a_down = f.routed_incident(a.id, group, 5).await;
    let alone_down: Uuid = sqlx::query_scalar(
        "INSERT INTO incidents (org_id, target_id, started_at, status_at_start) \
         VALUES ($1, $2, now(), 'down') RETURNING id",
    )
    .bind(f.org.0)
    .bind(alone.id)
    .fetch_one(&f.pool)
    .await
    .unwrap();

    let first = attach_member(&f.pool, f.org, ns_down).await.unwrap();
    let Attach::Joined {
        group_incident,
        seq: 1,
        opened: true,
    } = first
    else {
        panic!("first member opens the group incident: {first:?}");
    };
    assert_eq!(
        attach_member(&f.pool, f.org, a_down).await.unwrap(),
        Attach::Joined {
            group_incident,
            seq: 2,
            opened: false
        }
    );
    assert_eq!(
        attach_member(&f.pool, f.org, a_down).await.unwrap(),
        Attach::Settled
    );
    assert_eq!(
        attach_member(&f.pool, f.org, alone_down).await.unwrap(),
        Attach::Standalone
    );

    let g = incident(&f.pool, group_incident).await;
    assert_eq!(g.group_id, Some(group));
    assert_eq!(g.group_name.as_deref(), Some("DNS"));
    assert_eq!(g.member_seq, 2);
    assert!(g.page_owed_at.is_some());
    let started: (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>) = sqlx::query_as(
        "SELECT g.started_at, m.started_at FROM incidents g, incidents m \
         WHERE g.id = $1 AND m.id = $2",
    )
    .bind(group_incident)
    .bind(ns_down)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(started.0, started.1);
    for (member, seq) in [(ns_down, 1), (a_down, 2)] {
        let m = incident(&f.pool, member).await;
        assert_eq!(m.group_incident_id, Some(group_incident));
        assert_eq!(m.group_member_seq, Some(seq));
        assert_eq!(m.page_owed_at, None);
    }
    assert_eq!(
        f.event_kinds(group_incident).await,
        vec!["member_down", "member_down"]
    );
    let ops = PgIncidentOpsStore::new(f.pool.clone());
    let read = ops.get(f.org, group_incident).await.unwrap().unwrap();
    assert!(read.may_page());
    assert_eq!(read.origin, uptimepage::domain::IncidentOrigin::Group);
    f.done().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_group_incident_closes_with_its_last_member_down() {
    let Some(f) = Fixture::new("grp_close").await else {
        return;
    };
    let group = f.alerting_group("DNS").await;
    let ns = f.monitor("ns", Some("DNS")).await;
    let a = f.monitor("a", Some("DNS")).await;
    let ns_down = f.routed_incident(ns.id, group, 10).await;
    let a_down = f.routed_incident(a.id, group, 5).await;
    attach_member(&f.pool, f.org, ns_down).await.unwrap();
    let Attach::Joined { group_incident, .. } =
        attach_member(&f.pool, f.org, a_down).await.unwrap()
    else {
        panic!("second member joins");
    };
    let writer = PgIncidentStore::new(f.pool.clone());
    let now = chrono::Utc::now();
    let later = now - chrono::Duration::seconds(30);

    assert_eq!(
        writer.close(f.org, a_down, now).await.unwrap(),
        Some(Closed::default())
    );
    assert!(writer.close(f.org, a_down, now).await.unwrap().is_none());
    let member = incident(&f.pool, a_down).await;
    assert!(member.ended_at.is_some());
    assert_eq!(member.closing_notice_at, None);
    assert_eq!(incident(&f.pool, group_incident).await.ended_at, None);

    // Backdated earlier than the first close: the group ends with the latest.
    assert_eq!(
        writer.close(f.org, ns_down, later).await.unwrap(),
        Some(Closed {
            group_incident: Some(group_incident)
        })
    );
    let g = incident(&f.pool, group_incident).await;
    assert_eq!(
        g.ended_at.map(|t| t.timestamp_micros()),
        Some(now.timestamp_micros())
    );
    assert!(g.closing_notice_at.is_some());
    assert_eq!(
        f.event_kinds(group_incident).await,
        vec!["member_down", "member_down", "member_up", "member_up"]
    );

    let routed_unattached = f.routed_incident(ns.id, group, 1).await;
    assert!(
        writer
            .close(f.org, routed_unattached, now)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        incident(&f.pool, routed_unattached)
            .await
            .closing_notice_at
            .is_some()
    );
    f.done().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn deleting_a_group_hands_members_still_down_back_to_their_own_channels() {
    let Some(f) = Fixture::new("grp_delete").await else {
        return;
    };
    let group = f.alerting_group("DNS").await;
    let ns = f.monitor("ns", Some("DNS")).await;
    let a = f.monitor("a", Some("DNS")).await;
    let mx = f.monitor("mx", Some("DNS")).await;
    let ns_down = f.routed_incident(ns.id, group, 90).await;
    let a_down = f.routed_incident(a.id, group, 80).await;
    attach_member(&f.pool, f.org, ns_down).await.unwrap();
    let Attach::Joined { group_incident, .. } =
        attach_member(&f.pool, f.org, a_down).await.unwrap()
    else {
        panic!("second member joins");
    };
    PgIncidentStore::new(f.pool.clone())
        .close(f.org, a_down, chrono::Utc::now())
        .await
        .unwrap();
    let held = f.routed_incident(mx.id, group, 70).await;

    let deleted = f.groups.delete(f.org, group, None).await.unwrap().unwrap();
    assert_eq!(deleted.members, 3);
    assert_eq!(deleted.closed_incident, Some(group_incident));
    assert_eq!(deleted.detached, vec![ns_down]);

    let g = incident(&f.pool, group_incident).await;
    assert!(g.ended_at.is_some() && g.closing_notice_at.is_some());
    assert!(g.closed_by_group_delete);
    assert_eq!((g.group_id, g.group_ref), (None, Some(group)));
    assert_eq!(g.group_name.as_deref(), Some("DNS"));
    let note: Option<String> = sqlx::query_scalar(
        "SELECT message FROM incident_events WHERE incident_id = $1 AND kind = 'group_deleted'",
    )
    .bind(group_incident)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        note.as_deref(),
        Some("Group deleted while ns is still down; it now alerts on its own.")
    );
    let read = PgIncidentOpsStore::new(f.pool.clone())
        .get(f.org, group_incident)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        read.closing_reason(),
        uptimepage::domain::NotificationReason::GroupDeleted
    );

    let m = incident(&f.pool, ns_down).await;
    assert_eq!((m.group_incident_id, m.routed_group_id), (None, None));
    assert!(m.detached_at.is_some() && m.page_owed_at.is_some());
    assert_eq!(m.ended_at, None);
    let ops = PgIncidentOpsStore::new(f.pool.clone());
    assert_eq!(ops.generation(f.org, ns_down).await.unwrap(), Some(1));
    // Closed before the delete: stays as it was.
    assert_eq!(incident(&f.pool, a_down).await.detached_at, None);
    // Routed but never attached: its route goes, its own first page is still owed.
    let h = incident(&f.pool, held).await;
    assert_eq!(h.routed_group_id, None);
    assert!(h.page_owed_at.is_some());

    let again = f.alerting_group("DNS").await;
    assert_ne!(again, group);
    assert_eq!(f.audit_count("group.deleted").await, 1);
    f.done().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn group_links_stay_inside_the_org_and_point_at_group_incidents() {
    let Some(f) = Fixture::new("grp_links").await else {
        return;
    };
    let other = org(&f.pool, "grpb").await;
    let theirs = PgMonitorGroupStore::new(f.pool.clone())
        .create(
            other,
            NewMonitorGroup::named("DNS"),
            WriteSource::Ui,
            None,
            i64::MAX,
        )
        .await
        .unwrap()
        .id;
    let ns = f.monitor("ns", None).await;
    let cross_target = sqlx::query("UPDATE targets SET group_id = $2 WHERE id = $1")
        .bind(ns.id)
        .bind(theirs)
        .execute(&f.pool)
        .await;
    assert!(cross_target.is_err());
    let cross_route = sqlx::query(
        "INSERT INTO incidents (org_id, target_id, started_at, status_at_start, routed_group_id) \
         VALUES ($1, $2, now(), 'down', $3)",
    )
    .bind(f.org.0)
    .bind(ns.id)
    .bind(theirs)
    .execute(&f.pool)
    .await;
    assert!(cross_route.is_err());

    let plain: Uuid = sqlx::query_scalar(
        "INSERT INTO incidents (org_id, target_id, started_at, status_at_start) \
         VALUES ($1, $2, now(), 'down') RETURNING id",
    )
    .bind(f.org.0)
    .bind(ns.id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    let point = async |at: Uuid| {
        sqlx::query("UPDATE incidents SET group_incident_id = $2 WHERE id = $1")
            .bind(plain)
            .bind(at)
            .execute(&f.pool)
            .await
    };
    assert!(point(plain).await.is_err(), "not its own group");
    let manual: Uuid = sqlx::query_scalar(
        "INSERT INTO incidents (org_id, started_at, status_at_start, origin) \
         VALUES ($1, now(), 'down', 'manual') RETURNING id",
    )
    .bind(f.org.0)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert!(point(manual).await.is_err(), "not a group incident");
    let group_without_group = sqlx::query(
        "INSERT INTO incidents (org_id, started_at, status_at_start, origin) \
         VALUES ($1, now(), 'down', 'group')",
    )
    .bind(f.org.0)
    .execute(&f.pool)
    .await;
    assert!(group_without_group.is_err());
    f.done().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_group_silence_holds_its_members_until_every_one_is_back() {
    use uptimepage::storage::{PgSilenceStore, Resolved, SilenceStore};
    let Some(f) = Fixture::new("grp_silence").await else {
        return;
    };
    let ops = f.channel("ops").await;
    let mut new = NewMonitorGroup::named("DNS");
    new.alert_as_one = true;
    new.alerts = TargetAlerts(vec![uptimepage::domain::AlertBinding { channel_id: ops }]);
    f.groups
        .create(f.org, new, WriteSource::Ui, None, i64::MAX)
        .await
        .unwrap();
    let ns = f.monitor("ns", Some("DNS")).await.id;
    let a = f.monitor("a", Some("DNS")).await.id;
    let mx = f.monitor("mx", Some("DNS")).await.id;
    let alone = f.monitor("alone", None).await.id;
    let store = PgSilenceStore::new(f.pool.clone());
    let now = chrono::Utc::now();
    for m in [ns, a, mx] {
        store.enter(f.org, m, now).await.unwrap();
    }

    let groups = store
        .alerting_groups(&[(f.org, ns), (f.org, alone)])
        .await
        .unwrap();
    assert_eq!(groups.len(), 1);
    let group = groups[&(f.org, ns)].clone();
    assert_eq!(
        group
            .alerts
            .iter()
            .map(|b| b.channel_id)
            .collect::<Vec<_>>(),
        vec![ops]
    );

    let episode = store
        .join_group_silence(f.org, &group, &[ops], &[(ns, now), (a, now)])
        .await
        .unwrap();
    let due = store.group_silences_due().await.unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(
        (due[0].id, due[0].notified, due[0].finished),
        (episode, false, false)
    );
    assert_eq!(due[0].members, vec!["ns", "a"]);
    assert_eq!(due[0].channel_ids, vec![ops]);
    store
        .mark_group_silence_notified(f.org, episode, now)
        .await
        .unwrap();
    assert!(store.group_silences_due().await.unwrap().is_empty());

    let again = store
        .join_group_silence(f.org, &group, &[], &[(mx, now), (ns, now)])
        .await
        .unwrap();
    assert_eq!(again, episode);
    assert_eq!(
        store.resolve(f.org, ns, now).await.unwrap(),
        Resolved::InGroup
    );
    assert_eq!(
        store.resolve(f.org, ns, now).await.unwrap(),
        Resolved::Already
    );
    assert_eq!(
        store.resolve(f.org, alone, now).await.unwrap(),
        Resolved::Already
    );
    // Silent again while the episode is open: a fresh row in the same one.
    store.enter(f.org, ns, now).await.unwrap();
    store
        .join_group_silence(f.org, &group, &[], &[(ns, now)])
        .await
        .unwrap();
    for m in [ns, a] {
        assert_eq!(
            store.resolve(f.org, m, now).await.unwrap(),
            Resolved::InGroup
        );
    }
    assert!(store.group_silences_due().await.unwrap().is_empty());
    f.targets.delete(f.org, mx, None).await.unwrap();
    let due = store.group_silences_due().await.unwrap();
    assert_eq!(due.len(), 1);
    assert!(due[0].finished && due[0].resumed);
    assert_eq!(due[0].members, vec!["ns", "a", "mx"]);

    store
        .end_group_silence(f.org, episode, now, true)
        .await
        .unwrap();
    assert!(store.group_silences_due().await.unwrap().is_empty());
    let next = store
        .join_group_silence(f.org, &group, &[ops], &[(ns, now)])
        .await
        .unwrap();
    assert_ne!(next, episode);

    // Every member deleted: the episode is over, but nothing came back.
    let mut mail = NewMonitorGroup::named("Mail");
    mail.alert_as_one = true;
    f.groups
        .create(f.org, mail, WriteSource::Ui, None, i64::MAX)
        .await
        .unwrap();
    let z = f.monitor("z", Some("Mail")).await.id;
    store.enter(f.org, z, now).await.unwrap();
    let mail = store.alerting_groups(&[(f.org, z)]).await.unwrap()[&(f.org, z)].clone();
    let gone = store
        .join_group_silence(f.org, &mail, &[], &[(z, now)])
        .await
        .unwrap();
    f.targets.delete(f.org, z, None).await.unwrap();
    let due = store.group_silences_due().await.unwrap();
    let ended = due.iter().find(|d| d.id == gone).expect("due");
    assert!(ended.finished && !ended.resumed);
    f.done().await;
}
