//! Live-Postgres contract for `notification_channels`: cross-tenant isolation
//! on every store method, the per-org quota cap, secrets sealed at rest by the
//! credentials KEK, and the org scoping of the channel-id lookup that the
//! target→channel binding IDOR guard (`verify_alert_channels`) is built on.
//! The in-memory suite (`notification_channels_test`) covers the
//! HTTP/redaction contract; this one exercises the real SQL.
//!
//! Live-PG ignored: needs `DATABASE_URL`. Migrations are auto-applied by
//! `pg_pool_from_env` on first connect — point it at a throwaway DB to also
//! validate the migrations themselves.

mod common;

use std::time::Duration;

use chrono::Utc;
use uptimepage::api::error::codes;
use uptimepage::domain::{
    AlertBinding, ChannelConfig, CheckSpec, EmailConfig, ExpectedStatus, NewIncidentNotification,
    NewNotificationChannel, NewTarget, NotificationChannelUpdate, NotificationReason,
    NotificationStatus, SlackConfig, TargetAlerts, WriteSource,
};
use uptimepage::error::AppError;
use uptimepage::storage::{
    Actor, IncidentOpsStore, NotificationChannelStore, PgIncidentOpsStore,
    PgNotificationChannelStore, PostgresTargetStore, TargetStore, create_org_with_owner,
};

use common::{default_http_check, make_user, pg_pool_from_env, test_cipher, unique_slug};

fn slack(name: &str, secret: &str) -> NewNotificationChannel {
    NewNotificationChannel {
        name: name.into(),
        config: ChannelConfig::Slack(SlackConfig {
            webhook_url: format!("https://hooks.slack.com/services/{secret}"),
            mention: None,
        }),
        enabled: true,
        auto_bind_tags: Vec::new(),
    }
}

/// Two owner-orgs (one user each). Returns `(org_a, org_b, user_a, user_b)`.
async fn two_orgs(
    pool: &sqlx::PgPool,
    tag: &str,
) -> (
    uptimepage::domain::OrgId,
    uptimepage::domain::OrgId,
    uptimepage::domain::UserId,
    uptimepage::domain::UserId,
) {
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

async fn cleanup(
    pool: &sqlx::PgPool,
    orgs: &[uptimepage::domain::OrgId],
    users: &[uptimepage::domain::UserId],
) {
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
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn channels_isolated_across_orgs_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b, user_a, user_b) = two_orgs(&pool, "nc-iso").await;
    let store = PgNotificationChannelStore::new(pool.clone(), None);

    let ch = store
        .create(
            org_a,
            slack("A Ops", "T/B/aaa"),
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .expect("A creates its channel");

    // B cannot see A's channel on any read path …
    assert!(store.get(org_b, ch.id).await.unwrap().is_none());
    assert!(store.list(org_b).await.unwrap().is_empty());
    assert!(
        store
            .existing_channel_ids(org_b, &[ch.id])
            .await
            .unwrap()
            .is_empty()
    );
    // … nor mutate it: update is a no-op miss, delete reports nothing removed.
    assert!(
        store
            .update(
                org_b,
                ch.id,
                NotificationChannelUpdate::default(),
                WriteSource::Ui,
                None,
            )
            .await
            .unwrap()
            .is_none()
    );
    assert!(!store.delete(org_b, ch.id, None).await.unwrap());

    // A still owns it after B's failed attempts.
    let a_view = store.get(org_a, ch.id).await.unwrap().expect("A sees it");
    assert_eq!(a_view.name, "A Ops");
    assert_eq!(store.list(org_a).await.unwrap().len(), 1);
    assert_eq!(
        store.existing_channel_ids(org_a, &[ch.id]).await.unwrap(),
        vec![ch.id]
    );
    assert!(store.delete(org_a, ch.id, None).await.unwrap());

    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}

/// The rule lookup reads the org's rule-carrying channels off the org index
/// and folds case in Rust; only live SQL proves the org scope and that the
/// text[] column round-trips.
#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn tag_rule_lookup_is_org_scoped_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b, user_a, user_b) = two_orgs(&pool, "nc-rule").await;
    let store = PgNotificationChannelStore::new(pool.clone(), None);

    let mut rule = slack("A db team", "T/B/rule");
    rule.auto_bind_tags = vec!["db".into(), "cache".into()];
    let by_rule = store
        .create(org_a, rule, WriteSource::Ui, 10, Some(user_a))
        .await
        .unwrap();
    // Same rule, other tenant: an overlap query without the org filter would
    // page a stranger's channel.
    let mut theirs = slack("B db team", "T/B/theirs");
    theirs.auto_bind_tags = vec!["db".into()];
    store
        .create(org_b, theirs, WriteSource::Ui, 10, Some(user_b))
        .await
        .unwrap();
    // No rule at all: never matched, whatever the monitor carries.
    store
        .create(
            org_a,
            slack("A plain", "T/B/plain"),
            WriteSource::Ui,
            10,
            Some(user_a),
        )
        .await
        .unwrap();

    // One tag in common is enough, and only the caller's org answers.
    assert_eq!(
        store
            .auto_bound_ids(org_a, &["prod".to_string(), "cache".to_string()])
            .await
            .unwrap(),
        vec![by_rule.id]
    );
    assert!(
        store
            .auto_bound_ids(org_a, &["web".to_string()])
            .await
            .unwrap()
            .is_empty()
    );
    // An untagged monitor matches nothing rather than everything.
    assert!(store.auto_bound_ids(org_a, &[]).await.unwrap().is_empty());

    // Case must not decide who is paged: the rule and the monitor tag are
    // typed on different screens, and a near-miss is silent until an outage.
    assert_eq!(
        store
            .auto_bound_ids(org_a, &["CACHE".to_string()])
            .await
            .unwrap(),
        vec![by_rule.id]
    );
    let mut shouty = slack("A shouty rule", "T/B/shouty");
    shouty.auto_bind_tags = vec!["Prod".into()];
    let by_shouty = store
        .create(org_a, shouty, WriteSource::Ui, 10, Some(user_a))
        .await
        .unwrap();
    assert_eq!(
        store
            .auto_bound_ids(org_a, &["prod".to_string()])
            .await
            .unwrap(),
        vec![by_shouty.id]
    );

    // The rule survives a round-trip and a PATCH replaces it whole.
    assert_eq!(
        store
            .get(org_a, by_rule.id)
            .await
            .unwrap()
            .unwrap()
            .auto_bind_tags,
        vec!["db".to_string(), "cache".to_string()]
    );
    store
        .update(
            org_a,
            by_rule.id,
            NotificationChannelUpdate {
                auto_bind_tags: Some(vec!["web".into()]),
                ..Default::default()
            },
            WriteSource::Ui,
            Some(user_a),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        store
            .auto_bound_ids(org_a, &["web".to_string()])
            .await
            .unwrap(),
        vec![by_rule.id]
    );
    assert!(
        store
            .auto_bound_ids(org_a, &["db".to_string()])
            .await
            .unwrap()
            .is_empty()
    );

    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn quota_cap_is_per_org_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b, user_a, user_b) = two_orgs(&pool, "nc-cap").await;
    let store = PgNotificationChannelStore::new(pool.clone(), None);
    let cap = 2;
    // Single-threaded: asserts the rejection contract + per-org scoping + live
    // counting. The advisory-lock concurrency that makes the count+INSERT
    // race-safe is covered by the storage-locks unit suite, not here.

    let a1 = store
        .create(org_a, slack("A1", "T/B/a1"), WriteSource::Ui, cap, None)
        .await
        .expect("A #1");
    store
        .create(org_a, slack("A2", "T/B/a2"), WriteSource::Ui, cap, None)
        .await
        .expect("A #2");
    // A is at its cap: the next create is rejected.
    match store
        .create(org_a, slack("A3", "T/B/a3"), WriteSource::Ui, cap, None)
        .await
    {
        Err(AppError::Unprocessable { code, .. }) => {
            assert_eq!(code, codes::CHANNEL_QUOTA_EXCEEDED)
        }
        other => panic!("expected {}, got {other:?}", codes::CHANNEL_QUOTA_EXCEEDED),
    }

    // The cap is per-org: B is unaffected by A's count.
    store
        .create(org_b, slack("B1", "T/B/b1"), WriteSource::Ui, cap, None)
        .await
        .expect("B #1 (A's count must not affect B)");
    store
        .create(org_b, slack("B2", "T/B/b2"), WriteSource::Ui, cap, None)
        .await
        .expect("B #2");

    // Cap tracks the live count: freeing a slot lets A create again.
    assert!(store.delete(org_a, a1.id, None).await.unwrap());
    store
        .create(org_a, slack("A4", "T/B/a4"), WriteSource::Ui, cap, None)
        .await
        .expect("A can create again after deleting one");

    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn channel_config_sealed_at_rest_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, _org_b, user_a, user_b) = two_orgs(&pool, "nc-seal").await;
    let store = PgNotificationChannelStore::new(pool.clone(), Some(test_cipher()));

    let secret = "T/B/zzSEKRETzz";
    let ch = store
        .create(
            org_a,
            slack("Sealed", secret),
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .expect("create sealed channel");

    // Raw column is the {"$enc":"v1:…"} envelope — never plaintext.
    let (raw,): (String,) =
        sqlx::query_as("SELECT config::text FROM notification_channels WHERE id = $1")
            .bind(ch.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(raw.contains("$enc"), "config must be sealed: {raw}");
    assert!(
        !raw.contains("zzSEKRETzz"),
        "plaintext secret leaked to disk: {raw}"
    );

    // Opened back through the store the caller sees plaintext again.
    let opened = store.get(org_a, ch.id).await.unwrap().unwrap();
    match opened.config {
        ChannelConfig::Slack(SlackConfig { webhook_url, .. }) => {
            assert!(webhook_url.ends_with(secret))
        }
        other => panic!("unexpected variant: {other:?}"),
    }

    cleanup(&pool, &[org_a], &[user_a, user_b]).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn target_alert_binding_channel_lookup_is_org_scoped_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b, user_a, user_b) = two_orgs(&pool, "nc-bind").await;
    let channels = PgNotificationChannelStore::new(pool.clone(), None);
    let targets = PostgresTargetStore::from_pool(pool.clone(), None);

    let ch = channels
        .create(
            org_a,
            slack("Bound", "T/B/bind"),
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .expect("A's channel");

    let url = url::Url::parse("https://example.com/").unwrap();
    let new_target = NewTarget {
        name: "alerting".into(),
        check: CheckSpec::Http(default_http_check(url, ExpectedStatus::Exact(200))),
        interval: Duration::from_secs(30),
        enabled: true,
        tags: vec![],
        alerts: TargetAlerts(vec![AlertBinding { channel_id: ch.id }]),
        region_policy: Default::default(),
        alert_confirmations: 2,
        notify_recovery: true,
        renotify_interval_secs: 3600,
        group_name: None,
        owner_user_id: None,
        regions: None,
    };
    let created = targets
        .create(org_a, new_target, WriteSource::Ui, i64::MAX, i64::MAX)
        .await
        .expect("A creates a target bound to its own channel");

    // Binding round-trips.
    let fetched = targets.get(org_a, created.id).await.unwrap().unwrap();
    let bound: Vec<_> = fetched.alerts.iter().map(|b| b.channel_id).collect();
    assert_eq!(bound, vec![ch.id]);

    // The IDOR guard: the channel id is "existing" only for its owning org,
    // so a foreign target can never resolve another tenant's channel.
    assert_eq!(
        channels
            .existing_channel_ids(org_a, &[ch.id])
            .await
            .unwrap(),
        vec![ch.id]
    );
    assert!(
        channels
            .existing_channel_ids(org_b, &[ch.id])
            .await
            .unwrap()
            .is_empty()
    );

    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn unbind_channel_scrubs_only_that_binding_in_org_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b, user_a, user_b) = two_orgs(&pool, "nc-unbind").await;
    let channels = PgNotificationChannelStore::new(pool.clone(), None);
    let targets = PostgresTargetStore::from_pool(pool.clone(), None);

    let ch_a = channels
        .create(org_a, slack("A", "T/B/ua"), WriteSource::Ui, i64::MAX, None)
        .await
        .unwrap();
    let ch_b = channels
        .create(org_a, slack("B", "T/B/ub"), WriteSource::Ui, i64::MAX, None)
        .await
        .unwrap();

    let mk_target = |name: &str, alerts: Vec<AlertBinding>| {
        let url = url::Url::parse("https://example.com/").unwrap();
        NewTarget {
            name: name.into(),
            check: CheckSpec::Http(default_http_check(url, ExpectedStatus::Exact(200))),
            interval: Duration::from_secs(30),
            enabled: true,
            tags: vec![],
            alerts: TargetAlerts(alerts),
            region_policy: Default::default(),
            alert_confirmations: 2,
            notify_recovery: true,
            renotify_interval_secs: 3600,
            group_name: None,
            owner_user_id: None,
            regions: None,
        }
    };
    let both = targets
        .create(
            org_a,
            mk_target(
                "both",
                vec![
                    AlertBinding {
                        channel_id: ch_a.id,
                    },
                    AlertBinding {
                        channel_id: ch_b.id,
                    },
                ],
            ),
            WriteSource::Ui,
            i64::MAX,
            i64::MAX,
        )
        .await
        .unwrap();
    let only_a = targets
        .create(
            org_a,
            mk_target(
                "only-a",
                vec![AlertBinding {
                    channel_id: ch_a.id,
                }],
            ),
            WriteSource::Ui,
            i64::MAX,
            i64::MAX,
        )
        .await
        .unwrap();
    // Store-level write: another org carrying the same channel id must be
    // untouched by org_a's scrub (handler validation would never allow this
    // binding, which is exactly why the SQL needs its own org guard).
    let foreign = targets
        .create(
            org_b,
            mk_target(
                "foreign",
                vec![AlertBinding {
                    channel_id: ch_a.id,
                }],
            ),
            WriteSource::Ui,
            i64::MAX,
            i64::MAX,
        )
        .await
        .unwrap();

    let touched = targets.unbind_channel(org_a, ch_a.id).await.unwrap();
    assert_eq!(touched, 2);

    let both = targets.get(org_a, both.id).await.unwrap().unwrap();
    let bound: Vec<_> = both.alerts.iter().map(|b| b.channel_id).collect();
    assert_eq!(bound, vec![ch_b.id], "sibling binding must survive");

    let only_a = targets.get(org_a, only_a.id).await.unwrap().unwrap();
    assert!(only_a.alerts.is_empty(), "binding must be scrubbed");

    let foreign = targets.get(org_b, foreign.id).await.unwrap().unwrap();
    assert_eq!(
        foreign.alerts.iter().count(),
        1,
        "foreign org's bindings must be untouched"
    );

    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn due_for_renotify_selects_overdue_open_unacked_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "renotify").await;
    let org = create_org_with_owner(&pool, user, &unique_slug("renotify"), "R")
        .await
        .unwrap()
        .expect("org")
        .id;
    let targets = PostgresTargetStore::from_pool(pool.clone(), None);
    let ops = PgIncidentOpsStore::new(pool.clone());
    let channels = PgNotificationChannelStore::new(pool.clone(), None);
    let channel = channels
        .create(
            org,
            slack("Renotify", "T/B/renotify"),
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .expect("create renotify channel");

    let mk_target = |name: &str, renotify: u32| {
        let url = url::Url::parse("https://example.com/").unwrap();
        NewTarget {
            name: name.into(),
            check: CheckSpec::Http(default_http_check(url, ExpectedStatus::Exact(200))),
            interval: Duration::from_secs(30),
            enabled: true,
            tags: vec![],
            alerts: TargetAlerts::default(),
            region_policy: Default::default(),
            alert_confirmations: 2,
            notify_recovery: true,
            renotify_interval_secs: renotify,
            group_name: None,
            owner_user_id: None,
            regions: None,
        }
    };

    // Open a triggered incident for `target_id` and record one successful page
    // `page_age` ago. Returns the incident id.
    async fn open_paged_incident(
        pool: &sqlx::PgPool,
        ops: &PgIncidentOpsStore,
        org: uptimepage::domain::OrgId,
        target_id: uuid::Uuid,
        channel_id: uuid::Uuid,
        page_age: chrono::Duration,
    ) -> uuid::Uuid {
        let (inc,): (uuid::Uuid,) = sqlx::query_as(
            "INSERT INTO incidents \
                (org_id, target_id, started_at, status_at_start, state, severity, urgency, origin, visibility) \
             VALUES ($1, $2, now() - interval '3 hours', 'down', 'triggered', 'major', 'high', 'monitor', 'internal') \
             RETURNING id",
        )
        .bind(org.0)
        .bind(target_id)
        .fetch_one(pool)
        .await
        .unwrap();
        let notif = ops
            .record_notification(NewIncidentNotification {
                org,
                incident_id: inc,
                escalation_level: Some(0),
                target_user_id: None,
                channel_id: Some(channel_id),
                transport: "slack".into(),
                reason: NotificationReason::Opened,
                status: NotificationStatus::Sent,
                attempt: 1,
                error: None,
                sent_at: Some(Utc::now() - page_age),
            })
            .await
            .unwrap();
        // record_notification stamps created_at = now(); backdate it so the
        // reminder cadence (which keys off the last attempt's created_at) sees
        // this page as `page_age` old.
        sqlx::query("UPDATE incident_notifications SET created_at = $2 WHERE id = $1")
            .bind(notif)
            .bind(Utc::now() - page_age)
            .execute(pool)
            .await
            .unwrap();
        inc
    }

    let overdue_t = targets
        .create(
            org,
            mk_target("overdue", 3600),
            WriteSource::Ui,
            i64::MAX,
            i64::MAX,
        )
        .await
        .unwrap();
    let recent_t = targets
        .create(
            org,
            mk_target("recent", 3600),
            WriteSource::Ui,
            i64::MAX,
            i64::MAX,
        )
        .await
        .unwrap();
    let off_t = targets
        .create(
            org,
            mk_target("off", 0),
            WriteSource::Ui,
            i64::MAX,
            i64::MAX,
        )
        .await
        .unwrap();

    let overdue = open_paged_incident(
        &pool,
        &ops,
        org,
        overdue_t.id,
        channel.id,
        chrono::Duration::hours(2),
    )
    .await;
    let recent = open_paged_incident(
        &pool,
        &ops,
        org,
        recent_t.id,
        channel.id,
        chrono::Duration::minutes(1),
    )
    .await;
    let off = open_paged_incident(
        &pool,
        &ops,
        org,
        off_t.id,
        channel.id,
        chrono::Duration::hours(2),
    )
    .await;

    let due: Vec<uuid::Uuid> = ops
        .due_for_renotify(Utc::now(), 100)
        .await
        .unwrap()
        .into_iter()
        .map(|d| d.id)
        .collect();
    assert!(due.contains(&overdue), "an overdue open incident is due");
    assert!(
        !due.contains(&recent),
        "a recently-paged incident is below its interval"
    );
    assert!(
        !due.contains(&off),
        "renotify_interval_secs = 0 disables reminders"
    );

    // Acknowledging the overdue one silences it.
    ops.acknowledge(org, overdue, Actor::System, None, None)
        .await
        .unwrap();
    let due: Vec<uuid::Uuid> = ops
        .due_for_renotify(Utc::now(), 100)
        .await
        .unwrap()
        .into_iter()
        .map(|d| d.id)
        .collect();
    assert!(
        !due.contains(&overdue),
        "an acknowledged incident is not reminded"
    );

    cleanup(&pool, &[org], &[user]).await;
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn telegram_lifecycle_disable_by_external_ref() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b, user_a, user_b) = two_orgs(&pool, "tg-life").await;
    let store = PgNotificationChannelStore::new(pool.clone(), None);

    let chat = format!("-100{}", uuid::Uuid::now_v7().simple());
    let linked = |name: &str, chat: &str| uptimepage::domain::NewNotificationChannel {
        name: name.into(),
        config: uptimepage::domain::ChannelConfig::TelegramApp(
            uptimepage::domain::TelegramAppConfig {
                chat_id: chat.into(),
                chat_title: Some("Ops".into()),
            },
        ),
        enabled: true,
        auto_bind_tags: Vec::new(),
    };
    // Two orgs share the kicked chat; org A has a second, unrelated link.
    let a = store
        .create(org_a, linked("prod", &chat), WriteSource::Ui, 10, None)
        .await
        .unwrap();
    store
        .create(org_b, linked("ops", &chat), WriteSource::Ui, 10, None)
        .await
        .unwrap();
    let other_chat = format!("-200{}", uuid::Uuid::now_v7().simple());
    store
        .create(
            org_a,
            linked("other", &other_chat),
            WriteSource::Ui,
            10,
            None,
        )
        .await
        .unwrap();

    let kind = uptimepage::domain::ChannelKind::TelegramApp;
    assert_eq!(store.count_by_external_ref(kind, &chat).await.unwrap(), 2);
    assert_eq!(
        store
            .disable_by_external_ref(kind, &chat, "unlinked from the Telegram side")
            .await
            .unwrap(),
        2
    );
    // Idempotent on already-disabled rows.
    assert_eq!(
        store
            .disable_by_external_ref(kind, &chat, "unlinked from the Telegram side")
            .await
            .unwrap(),
        0
    );
    let got = store.get(org_a, a.id).await.unwrap().unwrap();
    assert!(!got.enabled);
    assert_eq!(
        got.disabled_reason.as_deref(),
        Some("unlinked from the Telegram side")
    );
    // The unrelated chat is untouched and still counted by its own ref.
    assert_eq!(
        store
            .count_by_external_ref(kind, &other_chat)
            .await
            .unwrap(),
        1
    );

    // A name-only PATCH while disabled keeps the note…
    let renamed = store
        .update(
            org_a,
            a.id,
            NotificationChannelUpdate {
                name: Some("prod-tg".into()),
                ..Default::default()
            },
            WriteSource::Ui,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(!renamed.enabled);
    assert!(renamed.disabled_reason.is_some());

    // …and re-enabling clears it.
    let re = store
        .update(
            org_a,
            a.id,
            NotificationChannelUpdate {
                enabled: Some(true),
                ..Default::default()
            },
            WriteSource::Ui,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(re.enabled && re.disabled_reason.is_none());

    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn email_lifecycle_ref_is_derived_and_follows_the_address() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b, user_a, user_b) = two_orgs(&pool, "em-life").await;
    // Sealed config + plaintext lifecycle ref must coexist: the bounce path
    // matches the address without opening the sealed blob.
    let store = PgNotificationChannelStore::new(pool.clone(), Some(test_cipher()));
    let kind = uptimepage::domain::ChannelKind::Email;

    let addr_a = format!("{}@example.com", unique_slug("em-life"));
    let addr_b = format!("{}@example.com", unique_slug("em-life"));
    let email = |name: &str, to: &str| NewNotificationChannel {
        name: name.into(),
        config: ChannelConfig::Email(uptimepage::domain::EmailConfig { to: to.into() }),
        enabled: true,
        auto_bind_tags: Vec::new(),
    };
    let ch = store
        .create(org_a, email("mail", &addr_a), WriteSource::Ui, 10, None)
        .await
        .unwrap();

    // The derived ref finds the channel; the bounce disable lands and the
    // dead address must re-prove itself before it can page again.
    let upd = store.get(org_a, ch.id).await.unwrap().unwrap().updated_at;
    assert!(store.set_verified(org_a, ch.id, upd).await.unwrap());
    assert_eq!(store.count_by_external_ref(kind, &addr_a).await.unwrap(), 1);
    assert_eq!(
        store
            .disable_by_external_ref(kind, &addr_a, "the email address hard-bounced")
            .await
            .unwrap(),
        1
    );
    let got = store.get(org_a, ch.id).await.unwrap().unwrap();
    assert!(!got.enabled);
    assert_eq!(
        got.disabled_reason.as_deref(),
        Some("the email address hard-bounced")
    );
    assert!(got.verified_at.is_none(), "bounce re-arms verification");

    // Replacing the address re-points the ref: the old address no longer
    // matches, the new one does.
    store
        .update(
            org_a,
            ch.id,
            NotificationChannelUpdate {
                config: Some(ChannelConfig::Email(uptimepage::domain::EmailConfig {
                    to: addr_b.clone(),
                })),
                enabled: Some(true),
                ..Default::default()
            },
            WriteSource::Ui,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(store.count_by_external_ref(kind, &addr_a).await.unwrap(), 0);
    assert_eq!(
        store
            .disable_by_external_ref(kind, &addr_a, "stale ref must not match")
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .disable_by_external_ref(kind, &addr_b, "the email address hard-bounced")
            .await
            .unwrap(),
        1
    );
    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn seeded_owner_email_is_verified_idempotent_and_capped() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b, user_a, user_b) = two_orgs(&pool, "seed-own").await;
    let store = PgNotificationChannelStore::new(pool.clone(), Some(test_cipher()));
    let addr = format!("{}@Example.com", unique_slug("Seed-Own"));
    let lowered = addr.to_ascii_lowercase();

    let ch = store
        .seed_owner_email(org_a, &addr, user_a, 10)
        .await
        .unwrap()
        .expect("first seed lands");
    // Unverified, an email channel is recorded as a failed notification and
    // delivers nothing, which would defeat the seeding.
    assert!(ch.verified_at.is_some());
    assert!(ch.enabled);
    assert_eq!(ch.kind, uptimepage::domain::ChannelKind::Email);
    assert_eq!(ch.name, lowered);
    assert_eq!(
        store
            .count_by_external_ref(uptimepage::domain::ChannelKind::Email, &lowered)
            .await
            .unwrap(),
        1
    );

    // Re-running the signup path must not stack duplicates or error.
    assert!(
        store
            .seed_owner_email(org_a, &addr, user_a, 10)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(store.list(org_a).await.unwrap().len(), 1);

    // At the cap it declines rather than failing: a signup must still succeed.
    assert!(
        store
            .seed_owner_email(org_b, &format!("other-{lowered}"), user_b, 0)
            .await
            .unwrap()
            .is_none()
    );
    assert!(store.list(org_b).await.unwrap().is_empty());

    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn delivery_health_is_org_scoped_and_alerts_once_per_run_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b, user_a, user_b) = two_orgs(&pool, "nc-health").await;
    let store = PgNotificationChannelStore::new(pool.clone(), None);

    let ch = store
        .create(
            org_a,
            slack("A Ops", "T/B/health"),
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .expect("A creates its channel");
    let stamped_at = ch.updated_at;

    // Another org can neither move A's run nor claim A's alert.
    assert_eq!(
        store
            .record_delivery_outcome(org_b, ch.id, false)
            .await
            .unwrap()
            .consecutive_failures,
        0
    );
    assert!(
        store
            .claim_failure_alert(org_b, ch.id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .get(org_a, ch.id)
            .await
            .unwrap()
            .unwrap()
            .consecutive_failures,
        0
    );

    let first = store
        .record_delivery_outcome(org_a, ch.id, false)
        .await
        .unwrap();
    assert_eq!(first.consecutive_failures, 1);
    let started = first.failing_since.expect("run has a start");

    let second = store
        .record_delivery_outcome(org_a, ch.id, false)
        .await
        .unwrap();
    assert_eq!(second.consecutive_failures, 2);
    assert_eq!(
        second.failing_since,
        Some(started),
        "a longer run keeps its original start"
    );

    // Counting is not an edit: `set_verified` reads this stamp to spot a
    // config swap racing a verify click.
    let live = store.get(org_a, ch.id).await.unwrap().unwrap();
    assert!(live.enabled);
    assert_eq!(live.updated_at, stamped_at);
    assert!(live.is_failing(2));

    let claimed = store
        .claim_failure_alert(org_a, ch.id)
        .await
        .unwrap()
        .expect("a fresh run is claimable");
    assert!(
        store
            .claim_failure_alert(org_a, ch.id)
            .await
            .unwrap()
            .is_none()
    );

    // An unsent claim comes back. A release naming some other run's stamp does
    // not free it, so a late release cannot hand back a claim it no longer owns.
    store
        .release_failure_alert(org_a, ch.id, claimed - chrono::Duration::seconds(1))
        .await
        .unwrap();
    assert!(
        store
            .claim_failure_alert(org_a, ch.id)
            .await
            .unwrap()
            .is_none()
    );
    store
        .release_failure_alert(org_a, ch.id, claimed)
        .await
        .unwrap();
    let reclaimed = store
        .claim_failure_alert(org_a, ch.id)
        .await
        .unwrap()
        .expect("a released claim is owed again");
    assert!(reclaimed >= claimed);

    let recovered = store
        .record_delivery_outcome(org_a, ch.id, true)
        .await
        .unwrap();
    assert_eq!(recovered.consecutive_failures, 0);
    assert_eq!(recovered.failing_since, None);
    assert!(
        store
            .claim_failure_alert(org_a, ch.id)
            .await
            .unwrap()
            .is_none()
    );

    // Recovering and dying again inside the cooldown is a flapping endpoint,
    // not news: one report per channel per cooldown, whatever the runs do.
    store
        .record_delivery_outcome(org_a, ch.id, false)
        .await
        .unwrap();
    assert!(
        store
            .claim_failure_alert(org_a, ch.id)
            .await
            .unwrap()
            .is_none(),
        "a flapping endpoint does not mail on every cycle"
    );

    // The stamp says when the channel last worked, so a later failure leaves it
    // where the recovery above put it.
    let landed = store.get(org_a, ch.id).await.unwrap().unwrap();
    let stamped = landed
        .last_delivered_at
        .expect("the delivery that landed above stamps");
    store
        .record_delivery_outcome(org_a, ch.id, false)
        .await
        .unwrap();
    let failed = store.get(org_a, ch.id).await.unwrap().unwrap();
    assert_eq!(failed.last_delivered_at, Some(stamped));

    // Every ordinary save carries `enabled: true`.
    let save = |enabled| NotificationChannelUpdate {
        enabled: Some(enabled),
        ..Default::default()
    };
    store
        .update(
            org_a,
            ch.id,
            NotificationChannelUpdate {
                name: Some("ops renamed".into()),
                ..save(true)
            },
            WriteSource::Ui,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    let saved = store.get(org_a, ch.id).await.unwrap().unwrap();
    assert!(saved.consecutive_failures > 0);
    assert!(saved.failing_since.is_some());

    // Off and back on is the operator saying it is dealt with.
    store
        .update(org_a, ch.id, save(false), WriteSource::Ui, None)
        .await
        .unwrap()
        .unwrap();
    store
        .update(org_a, ch.id, save(true), WriteSource::Ui, None)
        .await
        .unwrap()
        .unwrap();
    let reset = store.get(org_a, ch.id).await.unwrap().unwrap();
    assert_eq!(reset.consecutive_failures, 0);
    assert_eq!(reset.failing_since, None);

    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}

async fn failing_now(pool: &sqlx::PgPool, limit: u32, transport: &str) -> i64 {
    uptimepage::observability::channel_health::failing_by_transport(pool, limit)
        .await
        .expect("gauge query runs")
        .into_iter()
        .find(|(kind, _)| kind == transport)
        .map_or(0, |(_, n)| n)
}

/// The gauge SQL and `NotificationChannel::is_failing` have to agree. Asserts
/// deltas, not absolutes: the query is operator-wide and the DB is shared.
#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn the_failing_channel_gauge_agrees_with_the_domain_predicate_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    const LIMIT: u32 = 3;
    let (org_a, org_b, user_a, user_b) = two_orgs(&pool, "nc-gauge").await;
    let store = PgNotificationChannelStore::new(pool.clone(), None);

    let slack_before = failing_now(&pool, LIMIT, "slack").await;
    let email_before = failing_now(&pool, LIMIT, "email").await;

    let dead = store
        .create(
            org_a,
            slack("Gauge Dead", "T/G/dead"),
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .expect("dead channel created");
    let flaky = store
        .create(
            org_a,
            slack("Gauge Flaky", "T/G/flaky"),
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .expect("flaky channel created");
    // Seeding would land it pre-verified; the unverified one is the case here.
    let unverified = store
        .create(
            org_a,
            NewNotificationChannel {
                name: "Gauge Unverified".into(),
                config: ChannelConfig::Email(EmailConfig {
                    to: format!("gauge-{}@example.com", org_a.0),
                }),
                enabled: true,
                auto_bind_tags: Vec::new(),
            },
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .expect("unverified address created");
    assert_eq!(unverified.verified_at, None);

    for _ in 0..LIMIT {
        store
            .record_delivery_outcome(org_a, dead.id, false)
            .await
            .unwrap();
        store
            .record_delivery_outcome(org_a, unverified.id, false)
            .await
            .unwrap();
    }
    store
        .record_delivery_outcome(org_a, flaky.id, false)
        .await
        .unwrap();

    assert_eq!(
        failing_now(&pool, LIMIT, "slack").await - slack_before,
        1,
        "only the channel past the threshold counts"
    );
    assert_eq!(
        failing_now(&pool, LIMIT, "email").await - email_before,
        0,
        "an unverified address fails by design and is not a dead endpoint"
    );

    let dead_row = store.get(org_a, dead.id).await.unwrap().unwrap();
    let flaky_row = store.get(org_a, flaky.id).await.unwrap().unwrap();
    let unverified_row = store.get(org_a, unverified.id).await.unwrap().unwrap();
    assert!(dead_row.is_failing(LIMIT));
    assert!(!flaky_row.is_failing(LIMIT));
    assert!(!unverified_row.is_failing(LIMIT));

    assert!(
        uptimepage::observability::channel_health::failing_by_transport(&pool, 0)
            .await
            .unwrap()
            .is_empty()
    );

    // Deleting the org pauses monitoring, freezing the run where it stands —
    // counting it would alert on something no operator can clear.
    sqlx::query("UPDATE organizations SET deleted_at = now() WHERE id = $1")
        .bind(org_a.0)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        failing_now(&pool, LIMIT, "slack").await - slack_before,
        0,
        "a deleted org's channels drop out of the count"
    );
    sqlx::query("UPDATE organizations SET deleted_at = NULL WHERE id = $1")
        .bind(org_a.0)
        .execute(&pool)
        .await
        .unwrap();

    store
        .record_delivery_outcome(org_a, dead.id, true)
        .await
        .unwrap();
    assert_eq!(
        failing_now(&pool, LIMIT, "slack").await - slack_before,
        0,
        "a recovered channel stops being counted"
    );

    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}
