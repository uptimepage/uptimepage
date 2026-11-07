//! Alert source store: routes, names, secrets, the cap, token rotation, and
//! what its incidents keep when it is renamed or deleted.

use crate::common;
use common::{make_user, unique_slug};
use sqlx::PgPool;
use uptimepage::domain::{
    AlertBinding, AlertGrouping, AlertSourceKind, AlertSourceUpdate, NewAlertSource, OrgId,
    TargetAlerts, UserId, WriteSource,
};
use uptimepage::error::{AppError, codes};
use uptimepage::storage::alert_sources::{
    AlertSourceStore, InMemoryAlertSourceStore, PgAlertSourceStore,
};
use uptimepage::storage::{
    Actor, EscalationPolicyStore, IncidentOpsStore, PgEscalationPolicyStore, PgIncidentOpsStore,
    create_org_with_owner,
};
use uuid::Uuid;

const CAP: i64 = 25;

async fn seed(pool: &PgPool, prefix: &str) -> (OrgId, UserId) {
    let user = make_user(pool, prefix).await;
    let org = create_org_with_owner(pool, user, &unique_slug(prefix), "n")
        .await
        .expect("create org")
        .expect("org created");
    (org.id, user)
}

async fn seed_channel(pool: &PgPool, org: OrgId, name: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO notification_channels (org_id, name, kind, config, enabled) \
         VALUES ($1, $2, 'webhook', '{\"webhook\":{\"url\":\"http://x/\",\"headers\":{}}}'::jsonb, true) \
         RETURNING id",
    )
    .bind(org.0)
    .bind(name)
    .fetch_one(pool)
    .await
    .expect("insert channel")
}

async fn seed_policy(pool: &PgPool, org: OrgId) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO escalation_policies (org_id, name) VALUES ($1, 'ladder') RETURNING id",
    )
    .bind(org.0)
    .fetch_one(pool)
    .await
    .expect("insert policy")
}

async fn seed_alert_incident(pool: &PgPool, org: OrgId, source: Uuid, key: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO incidents \
             (org_id, started_at, status_at_start, origin, title, alert_source_id, dedup_key) \
         VALUES ($1, now(), 'down', 'alert', 'KubePodCrashLooping', $2, $3) RETURNING id",
    )
    .bind(org.0)
    .bind(source)
    .bind(key)
    .fetch_one(pool)
    .await
    .expect("insert alert incident")
}

fn routed(name: &str, kind: AlertSourceKind, channel: Uuid) -> NewAlertSource {
    NewAlertSource {
        name: name.into(),
        kind,
        grouping: AlertGrouping::Group,
        escalation_policy_id: None,
        alerts: TargetAlerts(vec![AlertBinding {
            channel_id: channel,
        }]),
        notify_recovery: true,
        renotify_interval_secs: 3600,
        signing_secret: None,
    }
}

fn code_of(err: &AppError) -> &'static str {
    match err {
        AppError::BadRequest { code, .. }
        | AppError::Conflict { code, .. }
        | AppError::Unprocessable { code, .. }
        | AppError::QuotaExceeded { code, .. } => code,
        other => panic!("unexpected error: {other:?}"),
    }
}

/// A source reaches someone or is refused, and one that later loses its last
/// route says so rather than paging nobody in silence.
#[tokio::test]
#[ignore]
async fn a_source_must_route_somewhere_and_says_when_it_no_longer_does_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user) = seed(&pool, "asroute").await;
    let store = PgAlertSourceStore::new(pool.clone(), None);
    let mut bare = routed("prom-prod", AlertSourceKind::Alertmanager, Uuid::nil());
    bare.alerts = TargetAlerts::default();
    let refused = store
        .create(org, bare.clone(), WriteSource::Api, Some(user), CAP)
        .await
        .unwrap_err();
    assert_eq!(code_of(&refused), codes::ALERT_SOURCE_NO_ROUTE);
    assert!(store.list(org).await.unwrap().is_empty(), "nothing kept");

    let policy = seed_policy(&pool, org).await;
    sqlx::query("UPDATE organizations SET default_escalation_policy_id = $2 WHERE id = $1")
        .bind(org.0)
        .bind(policy)
        .execute(&pool)
        .await
        .unwrap();
    let by_default = store
        .create(org, bare, WriteSource::Api, Some(user), CAP)
        .await
        .expect("the org default is a route");
    assert!(!by_default.routes_to_nobody);

    let channel = seed_channel(&pool, org, "ops").await;
    let by_channel = store
        .create(
            org,
            routed("grafana", AlertSourceKind::Grafana, channel),
            WriteSource::Ui,
            Some(user),
            CAP,
        )
        .await
        .unwrap();
    PgEscalationPolicyStore::new(pool.clone())
        .delete(org, policy)
        .await
        .unwrap();
    let after = store.get(org, by_default.id).await.unwrap().unwrap();
    assert!(after.routes_to_nobody, "its only route was the default");
    assert!(
        !store
            .get(org, by_channel.id)
            .await
            .unwrap()
            .unwrap()
            .routes_to_nobody
    );
    sqlx::query("UPDATE notification_channels SET enabled = false WHERE id = $1 AND org_id = $2")
        .bind(channel)
        .bind(org.0)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        store
            .get(org, by_channel.id)
            .await
            .unwrap()
            .unwrap()
            .routes_to_nobody,
        "a disabled channel reaches nobody"
    );

    let paused = store
        .update(
            org,
            by_channel.id,
            AlertSourceUpdate {
                enabled: Some(false),
                name: Some("grafana-old".into()),
                ..Default::default()
            },
            WriteSource::Ui,
            Some(user),
        )
        .await
        .expect("a source with no route left still pauses and renames")
        .unwrap();
    assert!(!paused.enabled);

    let cleared = store
        .update(
            org,
            by_channel.id,
            AlertSourceUpdate {
                alerts: Some(TargetAlerts::default()),
                ..Default::default()
            },
            WriteSource::Ui,
            Some(user),
        )
        .await
        .unwrap_err();
    assert_eq!(code_of(&cleared), codes::ALERT_SOURCE_NO_ROUTE);
}

/// A policy delete leaves a source on the org default, as it does a monitor.
#[tokio::test]
#[ignore]
async fn a_deleted_policy_comes_off_its_sources_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user) = seed(&pool, "aspolicy").await;
    let store = PgAlertSourceStore::new(pool.clone(), None);
    let policy = seed_policy(&pool, org).await;
    let channel = seed_channel(&pool, org, "ops").await;
    let mut new = routed("datadog", AlertSourceKind::Datadog, channel);
    new.escalation_policy_id = Some(policy);
    let created = store
        .create(org, new, WriteSource::Api, Some(user), CAP)
        .await
        .unwrap();
    assert_eq!(created.escalation_policy_id, Some(policy));
    PgEscalationPolicyStore::new(pool.clone())
        .delete(org, policy)
        .await
        .unwrap();
    let after = store.get(org, created.id).await.unwrap().unwrap();
    assert_eq!(after.escalation_policy_id, None);
}

/// Another org's channel or policy is refused as if it did not exist.
#[tokio::test]
#[ignore]
async fn a_source_binds_only_its_own_orgs_channels_and_policies_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user) = seed(&pool, "asmine").await;
    let (other, _) = seed(&pool, "astheirs").await;
    let store = PgAlertSourceStore::new(pool.clone(), None);
    let theirs = seed_channel(&pool, other, "theirs").await;
    let refused = store
        .create(
            org,
            routed("a", AlertSourceKind::Generic, theirs),
            WriteSource::Api,
            Some(user),
            CAP,
        )
        .await
        .unwrap_err();
    assert_eq!(code_of(&refused), codes::INVALID_ALERT_CONFIG);

    let mine = seed_channel(&pool, org, "mine").await;
    let mut new = routed("b", AlertSourceKind::Generic, mine);
    new.escalation_policy_id = Some(seed_policy(&pool, other).await);
    let refused = store
        .create(org, new, WriteSource::Api, Some(user), CAP)
        .await
        .unwrap_err();
    assert_eq!(code_of(&refused), codes::ESCALATION_POLICY_NOT_FOUND);
}

#[tokio::test]
#[ignore]
async fn names_secrets_and_the_cap_are_checked_on_write_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user) = seed(&pool, "aswrite").await;
    let store = PgAlertSourceStore::new(pool.clone(), None);
    let channel = seed_channel(&pool, org, "ops").await;
    let create = async |new: NewAlertSource, cap: i64| {
        store
            .create(org, new, WriteSource::Api, Some(user), cap)
            .await
    };

    let blank = create(routed("  ", AlertSourceKind::Generic, channel), CAP)
        .await
        .unwrap_err();
    assert_eq!(code_of(&blank), codes::INVALID_ALERT_SOURCE_NAME);
    let first = create(
        routed(" prom ", AlertSourceKind::Alertmanager, channel),
        CAP,
    )
    .await
    .unwrap();
    assert_eq!(first.name, "prom", "stored trimmed");
    let taken = create(routed("prom", AlertSourceKind::Generic, channel), CAP)
        .await
        .unwrap_err();
    assert_eq!(code_of(&taken), codes::ALERT_SOURCE_NAME_TAKEN);
    let full = create(routed("second", AlertSourceKind::Generic, channel), 1)
        .await
        .unwrap_err();
    assert!(
        matches!(
            full,
            AppError::QuotaExceeded {
                current: 1,
                limit: 1,
                ..
            }
        ),
        "{full:?}"
    );

    let mut unsigned_kind = routed("dd", AlertSourceKind::Datadog, channel);
    unsigned_kind.signing_secret = Some("s".repeat(32));
    let refused = create(unsigned_kind, CAP).await.unwrap_err();
    assert_eq!(code_of(&refused), codes::INVALID_SIGNING_SECRET);
    let mut short = routed("graf", AlertSourceKind::Grafana, channel);
    short.signing_secret = Some("short".into());
    let refused = create(short, CAP).await.unwrap_err();
    assert_eq!(code_of(&refused), codes::INVALID_SIGNING_SECRET);
    let mut signed = routed("graf", AlertSourceKind::Grafana, channel);
    signed.signing_secret = Some("s".repeat(32));
    let signed = create(signed, CAP).await.unwrap();
    assert!(signed.signed);
    let found = store
        .find_by_token(signed.token.as_deref().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        found.signing_secret.as_deref(),
        Some("s".repeat(32).as_str())
    );

    let unsigned = store
        .update(
            org,
            signed.id,
            AlertSourceUpdate {
                signing_secret: Some(None),
                ..Default::default()
            },
            WriteSource::Api,
            Some(user),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(!unsigned.signed, "null removes the secret");

    let audited = async || -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM org_audit_log WHERE org_id = $1 AND action = 'alert_source.updated'",
        )
        .bind(org.0)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let replace = async |secret: &str| {
        store
            .update(
                org,
                signed.id,
                AlertSourceUpdate {
                    signing_secret: Some(Some(secret.into())),
                    ..Default::default()
                },
                WriteSource::Api,
                Some(user),
            )
            .await
            .unwrap()
            .unwrap()
    };
    replace(&"a".repeat(20)).await;
    let before = audited().await;
    replace(&"b".repeat(20)).await;
    assert_eq!(audited().await, before + 1, "a replaced secret is audited");

    let mut twice = routed("dupe", AlertSourceKind::Generic, channel);
    twice.alerts.0.push(AlertBinding {
        channel_id: channel,
    });
    let refused = create(twice, CAP).await.unwrap_err();
    assert_eq!(code_of(&refused), codes::INVALID_ALERT_CONFIG);
}

/// A rotated token stays good for the overlap, then not; revoking ends it at
/// once. Neither leaks to another org.
#[tokio::test]
#[ignore]
async fn a_rotated_token_keeps_working_until_its_overlap_ends_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user) = seed(&pool, "asrotate").await;
    let store = PgAlertSourceStore::new(pool.clone(), None);
    let channel = seed_channel(&pool, org, "ops").await;
    let created = store
        .create(
            org,
            routed("prom", AlertSourceKind::Alertmanager, channel),
            WriteSource::Api,
            Some(user),
            CAP,
        )
        .await
        .unwrap();
    let old = created.token.clone().unwrap();
    let hit = store.find_by_token(&old).await.unwrap().unwrap();
    assert_eq!(
        (hit.org, hit.source.id, hit.previous),
        (org, created.id, false)
    );
    assert!(store.find_by_token("not-a-token").await.unwrap().is_none());

    let rotated = store
        .rotate(org, created.id, false, Some(user))
        .await
        .unwrap()
        .unwrap();
    let new = rotated.token.clone().unwrap();
    assert_ne!(new, old);
    assert!(rotated.open_overlap().is_some());
    assert!(!store.find_by_token(&new).await.unwrap().unwrap().previous);
    assert!(store.find_by_token(&old).await.unwrap().unwrap().previous);
    let stamped = store.get(org, created.id).await.unwrap().unwrap();
    assert!(
        stamped.prev_token_last_used_at.is_some(),
        "the old token's last use shows"
    );

    assert!(
        store
            .revoke_previous(org, created.id, Some(user))
            .await
            .unwrap()
    );
    assert!(store.find_by_token(&old).await.unwrap().is_none());
    assert!(
        !store
            .revoke_previous(org, created.id, Some(user))
            .await
            .unwrap()
    );

    store
        .rotate(org, created.id, true, Some(user))
        .await
        .unwrap()
        .unwrap();
    assert!(
        store.find_by_token(&new).await.unwrap().is_none(),
        "revoke_previous cuts the old token at once"
    );

    store
        .rotate(org, created.id, false, Some(user))
        .await
        .unwrap()
        .unwrap();
    sqlx::query(
        "UPDATE alert_sources SET prev_token_expires_at = now() - interval '1 second' \
         WHERE id = $1 AND org_id = $2",
    )
    .bind(created.id)
    .bind(org.0)
    .execute(&pool)
    .await
    .unwrap();
    uptimepage::storage::heartbeats::purge_expired_prev_tokens(&pool)
        .await
        .unwrap();
    let kept: Option<String> = sqlx::query_scalar(
        "SELECT prev_token_hash FROM alert_sources WHERE id = $1 AND org_id = $2",
    )
    .bind(created.id)
    .bind(org.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(kept, None, "the sweep clears an expired previous token");
}

/// Grouping waits while an incident is open; a rename follows onto it; a
/// delete resolves it and leaves it the source's name.
#[tokio::test]
#[ignore]
async fn a_sources_incidents_follow_its_rename_and_close_with_its_delete_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user) = seed(&pool, "asdelete").await;
    let store = PgAlertSourceStore::new(pool.clone(), None);
    let channel = seed_channel(&pool, org, "ops").await;
    let created = store
        .create(
            org,
            routed("prom", AlertSourceKind::Alertmanager, channel),
            WriteSource::Api,
            Some(user),
            CAP,
        )
        .await
        .unwrap();
    let incident = seed_alert_incident(&pool, org, created.id, "group-1").await;
    let duplicate = sqlx::query(
        "INSERT INTO incidents \
             (org_id, started_at, status_at_start, origin, title, alert_source_id, dedup_key) \
         VALUES ($1, now(), 'down', 'alert', 'again', $2, 'group-1')",
    )
    .bind(org.0)
    .bind(created.id)
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(
        duplicate
            .to_string()
            .contains("idx_incidents_org_open_alert"),
        "one open incident per key: {duplicate}"
    );

    let locked = store
        .update(
            org,
            created.id,
            AlertSourceUpdate {
                grouping: Some(AlertGrouping::Alert),
                ..Default::default()
            },
            WriteSource::Api,
            Some(user),
        )
        .await
        .unwrap_err();
    assert_eq!(code_of(&locked), codes::ALERT_SOURCE_GROUPING_LOCKED);
    store
        .update(
            org,
            created.id,
            AlertSourceUpdate {
                name: Some("prometheus".into()),
                ..Default::default()
            },
            WriteSource::Api,
            Some(user),
        )
        .await
        .unwrap()
        .unwrap();
    let name: String =
        sqlx::query_scalar("SELECT alert_source_name FROM incidents WHERE id = $1 AND org_id = $2")
            .bind(incident)
            .bind(org.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(name, "prometheus");

    let ops = PgIncidentOpsStore::new(pool.clone());
    let earlier: Uuid = sqlx::query_scalar(
        "INSERT INTO incidents \
             (org_id, started_at, ended_at, state, status_at_start, origin, title, \
              alert_source_id, dedup_key) \
         VALUES ($1, now() - interval '1 hour', now() - interval '50 minutes', 'resolved', \
                 'down', 'alert', 'earlier', $2, 'group-1') RETURNING id",
    )
    .bind(org.0)
    .bind(created.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let open_already = ops
        .reopen(org, earlier, Actor::User(user), None)
        .await
        .unwrap_err();
    assert_eq!(code_of(&open_already), codes::INCIDENT_ALREADY_OPEN);

    let deleted = store
        .delete(org, created.id, Some(user))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(deleted.closed, vec![incident]);
    let (actor, note): (Option<Uuid>, Option<String>) = sqlx::query_as(
        "SELECT actor_id, message FROM incident_events \
         WHERE incident_id = $1 AND org_id = $2 AND kind = 'resolved'",
    )
    .bind(incident)
    .bind(org.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        actor,
        Some(user.0),
        "the person who deleted the source closed it"
    );
    assert_eq!(
        note.as_deref(),
        Some("Closed by deleting the alert source prometheus.")
    );
    let gone = ops
        .reopen(org, incident, Actor::User(user), None)
        .await
        .unwrap_err();
    assert_eq!(code_of(&gone), codes::INCIDENT_ALERT_SOURCE_DELETED);
    let (state, source, kept_ref, kept_name): (String, Option<Uuid>, Option<Uuid>, Option<String>) =
        sqlx::query_as(
            "SELECT state, alert_source_id, alert_source_ref, alert_source_name \
             FROM incidents WHERE id = $1 AND org_id = $2",
        )
        .bind(incident)
        .bind(org.0)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "resolved");
    assert_eq!(source, None);
    assert_eq!(kept_ref, Some(created.id));
    assert_eq!(kept_name.as_deref(), Some("prometheus"));
    assert!(store.get(org, created.id).await.unwrap().is_none());
    assert!(
        store
            .find_by_token(created.token.as_deref().unwrap())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .delete(org, created.id, Some(user))
            .await
            .unwrap()
            .is_none()
    );
}

/// The schema refuses an alert incident that does not name its source and key,
/// and one bound to another org's source.
#[tokio::test]
#[ignore]
async fn an_alert_incident_names_its_source_and_key_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user) = seed(&pool, "asschema").await;
    let (other, _) = seed(&pool, "asother").await;
    let store = PgAlertSourceStore::new(pool.clone(), None);
    let channel = seed_channel(&pool, org, "ops").await;
    let source = store
        .create(
            org,
            routed("prom", AlertSourceKind::Alertmanager, channel),
            WriteSource::Api,
            Some(user),
            CAP,
        )
        .await
        .unwrap();
    let no_key = sqlx::query(
        "INSERT INTO incidents (org_id, started_at, status_at_start, origin, title, alert_source_id) \
         VALUES ($1, now(), 'down', 'alert', 't', $2)",
    )
    .bind(org.0)
    .bind(source.id)
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(
        no_key.to_string().contains("incident_alert_named"),
        "{no_key}"
    );
    let foreign = sqlx::query(
        "INSERT INTO incidents \
             (org_id, started_at, status_at_start, origin, title, alert_source_id, dedup_key) \
         VALUES ($1, now(), 'down', 'alert', 't', $2, 'k')",
    )
    .bind(other.0)
    .bind(source.id)
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(foreign.to_string().contains("belongs to org"), "{foreign}");
}

/// The double keeps the same rules for tests that run without Postgres.
#[tokio::test]
async fn the_in_memory_store_keeps_the_same_rules() {
    let store = InMemoryAlertSourceStore::new();
    let org = OrgId(Uuid::now_v7());
    let channel = Uuid::now_v7();
    let mut bare = routed("prom", AlertSourceKind::Alertmanager, channel);
    bare.alerts = TargetAlerts::default();
    let refused = store
        .create(org, bare, WriteSource::Api, None, CAP)
        .await
        .unwrap_err();
    assert_eq!(code_of(&refused), codes::ALERT_SOURCE_NO_ROUTE);

    let created = store
        .create(
            org,
            routed("prom", AlertSourceKind::Alertmanager, channel),
            WriteSource::Api,
            None,
            CAP,
        )
        .await
        .unwrap();
    let taken = store
        .create(
            org,
            routed("prom", AlertSourceKind::Generic, channel),
            WriteSource::Api,
            None,
            CAP,
        )
        .await
        .unwrap_err();
    assert_eq!(code_of(&taken), codes::ALERT_SOURCE_NAME_TAKEN);

    let old = created.token.clone().unwrap();
    let rotated = store
        .rotate(org, created.id, false, None)
        .await
        .unwrap()
        .unwrap();
    assert!(store.find_by_token(&old).await.unwrap().unwrap().previous);
    assert!(
        !store
            .find_by_token(rotated.token.as_deref().unwrap())
            .await
            .unwrap()
            .unwrap()
            .previous
    );
    assert!(store.revoke_previous(org, created.id, None).await.unwrap());
    assert!(store.find_by_token(&old).await.unwrap().is_none());
    assert!(
        store
            .get(OrgId(Uuid::now_v7()), created.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(store.delete(org, created.id, None).await.unwrap().is_some());
}
