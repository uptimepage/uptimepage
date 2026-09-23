//! Postgres-backed validation of the operational incident lifecycle store
//! (`PgIncidentOpsStore`). The in-memory store unit tests cover the state
//! machine; these exercise the real SQL: the transition UPDATE ... RETURNING,
//! the `OpsIncident` row mapping, the per-incident advisory lock, the timeline
//! event insert (+ its org-match trigger), and cross-tenant scoping.
//!
//! `#[ignore]`d by default; runs under `--run-ignored all` once `DATABASE_URL`
//! is set. The harness auto-applies all migrations on first connect.

mod common;

use common::{make_user, unique_slug};
use sqlx::PgPool;
use uptimepage::domain::{
    IncidentEventKind, IncidentState, NewIncidentNotification, NewManualIncident,
    NotificationOutcome, NotificationReason, NotificationStatus, OrgId,
};
use uptimepage::storage::{
    Actor, InMemoryIncidentOpsStore, IncidentOpsStore, LifecycleOutcome, PgIncidentOpsStore,
    QUEUED_TAKEOVER_SECS, create_org_with_owner,
};
use uuid::Uuid;

/// Seed an org (with owner) + a target + one open (`triggered`) incident.
/// Returns (org, owner user, incident id).
async fn seed(pool: &PgPool, prefix: &str) -> (OrgId, uptimepage::domain::UserId, Uuid) {
    let user = make_user(pool, prefix).await;
    let org = create_org_with_owner(pool, user, &unique_slug(prefix), "n")
        .await
        .expect("create org")
        .expect("org created");
    let target_id: Uuid = sqlx::query_scalar(
        "INSERT INTO targets (org_id, name, check_spec, interval_secs) \
         VALUES ($1, 'svc', '{}'::jsonb, 30) RETURNING id",
    )
    .bind(org.id.0)
    .fetch_one(pool)
    .await
    .expect("insert target");
    let incident_id: Uuid = sqlx::query_scalar(
        "INSERT INTO incidents (org_id, target_id, started_at, status_at_start) \
         VALUES ($1, $2, now() - interval '5 minutes', 'down') RETURNING id",
    )
    .bind(org.id.0)
    .bind(target_id)
    .fetch_one(pool)
    .await
    .expect("insert incident");
    (org.id, user, incident_id)
}

fn updated(o: LifecycleOutcome) -> uptimepage::domain::OpsIncident {
    match o {
        LifecycleOutcome::Updated(i) => *i,
        other => panic!("expected Updated, got {other:?}"),
    }
}

/// A declared incident may name no monitor at all, and every read and write
/// path over it decodes the same row. `target_id` is NULL there, so a column
/// typed non-null turns the whole edit surface into a 500.
#[tokio::test]
#[ignore]
async fn a_target_less_incident_survives_every_narration_path_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user, _) = seed(&pool, "incnotgt").await;
    let ops = PgIncidentOpsStore::new(pool.clone());
    let narration = uptimepage::storage::PgIncidentNarrationStore::new(pool.clone());

    let declared = ops
        .declare(
            org,
            NewManualIncident {
                title: Some("partner outage".into()),
                ..Default::default()
            },
            Actor::User(user),
        )
        .await
        .expect("declare");
    assert_eq!(declared.target_id, None, "declared without a monitor");

    let read = uptimepage::storage::IncidentNarrationStore::get(&narration, org, declared.id)
        .await
        .expect("read narration")
        .expect("incident exists");
    assert_eq!(read.target_id, None);

    let patched = uptimepage::storage::IncidentNarrationStore::patch_narration(
        &narration,
        org,
        declared.id,
        uptimepage::domain::IncidentNarrationUpdate {
            public_title: Some(Some("Partner API down".into())),
            ..Default::default()
        },
    )
    .await
    .expect("amend")
    .expect("incident exists");
    assert_eq!(patched.target_id, None);
    assert_eq!(patched.public_title.as_deref(), Some("Partner API down"));

    // A brief names a monitor, so this incident has no place in that list and
    // must be skipped rather than break the query for the whole org.
    let briefs = uptimepage::storage::IncidentNarrationStore::list_briefs(
        &narration,
        org,
        uptimepage::storage::IncidentBriefFilter::default(),
    )
    .await
    .expect("list briefs");
    assert!(!briefs.iter().any(|b| b.id == declared.id), "{briefs:?}");
}

/// Declaring is done with the least information, so every field it captures
/// has to be changeable after.
#[tokio::test]
#[ignore]
async fn an_incident_can_be_amended_after_it_is_declared_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user, _) = seed(&pool, "incedit").await;
    let ops = PgIncidentOpsStore::new(pool.clone());
    let narration = uptimepage::storage::PgIncidentNarrationStore::new(pool.clone());

    let declared = ops
        .declare(
            org,
            NewManualIncident {
                title: Some("partner outage".into()),
                ..Default::default()
            },
            Actor::User(user),
        )
        .await
        .expect("declare");
    assert_eq!(declared.urgency, uptimepage::domain::IncidentUrgency::High);

    uptimepage::storage::IncidentNarrationStore::patch_narration(
        &narration,
        org,
        declared.id,
        uptimepage::domain::IncidentNarrationUpdate {
            title: Some(Some("partner API degraded".into())),
            severity: Some(uptimepage::domain::IncidentSeverity::Critical),
            urgency: Some(uptimepage::domain::IncidentUrgency::Low),
            public_title: Some(Some("Elevated errors".into())),
            public_description: Some(Some("Some checkouts fail.".into())),
            counts_as_downtime: Some(true),
        },
    )
    .await
    .expect("amend")
    .expect("incident exists");

    let after = ops.get(org, declared.id).await.unwrap().unwrap();
    assert_eq!(after.title.as_deref(), Some("partner API degraded"));
    assert_eq!(
        after.severity,
        uptimepage::domain::IncidentSeverity::Critical
    );
    assert_eq!(after.urgency, uptimepage::domain::IncidentUrgency::Low);

    // Clearing the internal title falls the label back to the monitor name.
    uptimepage::storage::IncidentNarrationStore::patch_narration(
        &narration,
        org,
        declared.id,
        uptimepage::domain::IncidentNarrationUpdate {
            title: Some(None),
            ..Default::default()
        },
    )
    .await
    .expect("clear title")
    .expect("incident exists");
    let cleared = ops.get(org, declared.id).await.unwrap().unwrap();
    assert_eq!(cleared.title, None);
    assert_eq!(
        cleared.severity,
        uptimepage::domain::IncidentSeverity::Critical,
        "an omitted field is left alone"
    );
}

/// The reconcile sweep pages any triggered incident that reached no channel,
/// which would undo the declare path's silence a minute later.
#[tokio::test]
#[ignore]
async fn a_quietly_declared_incident_is_never_reconciled_into_a_page_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user, _) = seed(&pool, "incquiet").await;
    let store = PgIncidentOpsStore::new(pool.clone());

    let quiet = store
        .declare(org, NewManualIncident::default(), Actor::User(user))
        .await
        .expect("declare quiet");
    let loud = store
        .declare(
            org,
            NewManualIncident {
                notify: true,
                ..Default::default()
            },
            Actor::User(user),
        )
        .await
        .expect("declare with alerts");
    assert!(!quiet.paging_enabled);
    assert!(loud.paging_enabled);

    sqlx::query("UPDATE incidents SET started_at = now() - interval '1 hour' WHERE org_id = $1")
        .bind(org.0)
        .execute(&pool)
        .await
        .unwrap();
    let window = (
        chrono::Utc::now() - chrono::Duration::hours(6),
        chrono::Utc::now() - chrono::Duration::seconds(60),
    );
    let due = store.due_for_reconcile(window, 100).await.unwrap();
    assert!(
        !due.iter().any(|d| d.id == quiet.id),
        "a declare that asked for no alerts must stay unpaged"
    );
    assert!(
        due.iter().any(|d| d.id == loud.id),
        "a declare that asked for alerts keeps the dropped-signal safety net"
    );
}

/// Incidents cascade away with their monitor, so the audit log is the only
/// place a churned customer's incident work survives.
#[tokio::test]
#[ignore]
async fn operator_incident_actions_reach_the_org_audit_log_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user, seeded) = seed(&pool, "incaudit").await;
    let store = PgIncidentOpsStore::new(pool.clone());

    let declared = store
        .declare(
            org,
            NewManualIncident {
                title: Some("partner outage".into()),
                ..Default::default()
            },
            Actor::User(user),
        )
        .await
        .expect("declare");
    store
        .resolve(org, declared.id, Actor::User(user), None)
        .await
        .expect("resolve");
    store.auto_resolve(org, seeded).await.expect("auto resolve");

    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM org_audit_log WHERE org_id = $1 AND action LIKE 'incident.%'          ORDER BY occurred_at",
    )
    .bind(org.0)
    .fetch_all(&pool)
    .await
    .expect("read audit log");
    assert_eq!(actions, vec!["incident.declared", "incident.resolved"]);

    let declared_meta: serde_json::Value = sqlx::query_scalar(
        "SELECT metadata FROM org_audit_log WHERE org_id = $1 AND action = 'incident.declared'",
    )
    .bind(org.0)
    .fetch_one(&pool)
    .await
    .expect("read declare metadata");
    assert_eq!(declared_meta["incident_id"], declared.id.to_string());
    assert_eq!(declared_meta["severity"], "major");
}

#[tokio::test]
#[ignore]
async fn acknowledge_then_manual_resolve_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user, id) = seed(&pool, "incack").await;
    let store = PgIncidentOpsStore::new(pool.clone());

    let acked = updated(
        store
            .acknowledge(org, id, Actor::User(user), Some("on it".into()), None)
            .await
            .unwrap(),
    );
    assert_eq!(acked.state, IncidentState::Acknowledged);
    assert_eq!(acked.acknowledged_by, Some(user));
    assert!(acked.acknowledged_at.is_some());
    assert!(acked.next_escalation_at.is_none());

    let resolved = updated(
        store
            .resolve(org, id, Actor::User(user), None)
            .await
            .unwrap(),
    );
    assert_eq!(resolved.state, IncidentState::Resolved);
    assert_eq!(resolved.resolved_by, Some(user));
    assert!(resolved.ended_at.is_some());

    // Timeline recorded both lifecycle events, oldest first.
    let tl = store.timeline(org, id).await.unwrap();
    let kinds: Vec<_> = tl.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        vec![
            uptimepage::domain::IncidentEventKind::Acknowledged,
            uptimepage::domain::IncidentEventKind::Resolved
        ]
    );
}

#[tokio::test]
#[ignore]
async fn auto_resolve_leaves_resolved_by_null_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, _user, id) = seed(&pool, "incauto").await;
    let store = PgIncidentOpsStore::new(pool.clone());
    let resolved = updated(store.auto_resolve(org, id).await.unwrap());
    assert_eq!(resolved.state, IncidentState::Resolved);
    assert_eq!(resolved.resolved_by, None);
}

#[tokio::test]
#[ignore]
async fn reopen_after_resolve_clears_state_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user, id) = seed(&pool, "increopen").await;
    let store = PgIncidentOpsStore::new(pool.clone());
    store
        .acknowledge(org, id, Actor::User(user), None, None)
        .await
        .unwrap();
    store
        .resolve(org, id, Actor::User(user), None)
        .await
        .unwrap();
    let reopened = updated(
        store
            .reopen(org, id, Actor::User(user), None)
            .await
            .unwrap(),
    );
    assert_eq!(reopened.state, IncidentState::Triggered);
    assert!(reopened.ended_at.is_none());
    assert!(reopened.acknowledged_by.is_none());
    assert!(reopened.resolved_by.is_none());
}

#[tokio::test]
#[ignore]
async fn illegal_transition_is_reported_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user, id) = seed(&pool, "incillegal").await;
    let store = PgIncidentOpsStore::new(pool.clone());
    store
        .resolve(org, id, Actor::User(user), None)
        .await
        .unwrap();
    // Acknowledging a resolved incident is illegal (reopen first).
    let out = store
        .acknowledge(org, id, Actor::User(user), None, None)
        .await
        .unwrap();
    assert!(matches!(out, LifecycleOutcome::IllegalTransition(_)));
}

#[tokio::test]
#[ignore]
async fn writer_opens_internal_incident_for_private_monitor_pg() {
    use uptimepage::domain::CheckStatus;
    use uptimepage::public_status::incident_writer::{
        IncidentStore, NewOpenIncident, PgIncidentStore,
    };

    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "incvis").await;
    let org = create_org_with_owner(&pool, user, &unique_slug("incvis"), "n")
        .await
        .unwrap()
        .unwrap();
    // A monitor on no status page at all.
    let target_id: Uuid = sqlx::query_scalar(
        "INSERT INTO targets (org_id, name, check_spec, interval_secs) \
         VALUES ($1, 'private-svc', '{}'::jsonb, 30) RETURNING id",
    )
    .bind(org.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();

    let store = PgIncidentStore::new(pool.clone());
    let id = store
        .insert_open(
            org.id,
            NewOpenIncident {
                target_id,
                started_at: chrono::Utc::now(),
                status_at_start: CheckStatus::Down,
                check_count: 2,
                error_sample: None,
                region: None,
                regions_down: Vec::new(),
                regions_up: Vec::new(),
            },
        )
        .await
        .unwrap();

    let visibility: String = sqlx::query_scalar("SELECT visibility FROM incidents WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        visibility, "internal",
        "a private monitor's incident must never be publicly visible"
    );
}

#[tokio::test]
#[ignore]
async fn cross_org_cannot_touch_incident_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (_org_a, _user_a, id) = seed(&pool, "incown").await;
    // A second, unrelated org must not see or mutate org A's incident.
    let other_user = make_user(&pool, "incother").await;
    let other_org = create_org_with_owner(&pool, other_user, &unique_slug("incother"), "n")
        .await
        .unwrap()
        .unwrap();
    let store = PgIncidentOpsStore::new(pool.clone());

    assert!(store.get(other_org.id, id).await.unwrap().is_none());
    let out = store
        .acknowledge(other_org.id, id, Actor::User(other_user), None, None)
        .await
        .unwrap();
    assert!(matches!(out, LifecycleOutcome::NotFound));
    // The legitimate owner's add_note works; the other org's is a no-op.
    assert!(
        store
            .add_note(other_org.id, id, Actor::User(other_user), "x".into())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
#[ignore]
async fn notification_log_record_retry_and_scope_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, _user, id) = seed(&pool, "incnotif").await;
    let store = PgIncidentOpsStore::new(pool.clone());

    // A failed paging row (channel_id NULL avoids needing a real channel; the
    // org-match trigger still validates incident_id ↔ org_id).
    store
        .record_notification(NewIncidentNotification {
            org,
            incident_id: id,
            escalation_level: Some(0),
            target_user_id: None,
            channel_id: None,
            transport: "slack".into(),
            reason: NotificationReason::Opened,
            status: NotificationStatus::Failed,
            attempt: 1,
            error: Some("connect refused".into()),
            sent_at: None,
        })
        .await
        .expect("record");

    let rows = store.notifications_for(org, id).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].reason, NotificationReason::Opened);
    assert_eq!(rows[0].status, NotificationStatus::Failed);
    let notif_id = rows[0].id;

    // The retry scan picks it up (attempt 1 < cap, no backoff scheduled) with
    // the owning org.
    let now = chrono::Utc::now();
    let pending = store.pending_notifications(now, 50, 5).await.unwrap();
    let mine = pending.iter().find(|p| p.id == notif_id).expect("pending");
    assert_eq!(mine.org, org);
    assert_eq!(mine.reason, NotificationReason::Opened);

    // Schedule a backoff into the future: the scan must skip it until due.
    store
        .mark_notification(
            org,
            notif_id,
            NotificationOutcome {
                status: NotificationStatus::Failed,
                attempt: 2,
                error: Some("still down".into()),
                sent_at: None,
                next_attempt_at: Some(now + chrono::Duration::hours(1)),
                provider_receipt: None,
            },
        )
        .await
        .unwrap();
    assert!(
        store
            .pending_notifications(now, 50, 5)
            .await
            .unwrap()
            .iter()
            .all(|p| p.id != notif_id),
        "a backed-off row is not retried before next_attempt_at"
    );
    assert!(
        store
            .pending_notifications(now + chrono::Duration::hours(2), 50, 5)
            .await
            .unwrap()
            .iter()
            .any(|p| p.id == notif_id),
        "once the backoff elapses the row is due again"
    );

    // Mark it sent; it must drop out of the retry scan.
    store
        .mark_notification(
            org,
            notif_id,
            NotificationOutcome {
                status: NotificationStatus::Sent,
                attempt: 3,
                error: None,
                sent_at: Some(chrono::Utc::now()),
                next_attempt_at: None,
                provider_receipt: None,
            },
        )
        .await
        .unwrap();
    let rows = store.notifications_for(org, id).await.unwrap();
    assert_eq!(rows[0].status, NotificationStatus::Sent);
    assert_eq!(rows[0].attempt, 3);
    assert!(rows[0].sent_at.is_some());
    assert!(
        store
            .pending_notifications(chrono::Utc::now(), 50, 5)
            .await
            .unwrap()
            .iter()
            .all(|p| p.id != notif_id),
        "a sent row must not be retried"
    );

    // A queued row is a first attempt still running: the scan leaves it alone
    // until the takeover window passes.
    let queued_id = store
        .record_notification(NewIncidentNotification {
            org,
            incident_id: id,
            escalation_level: Some(0),
            target_user_id: None,
            channel_id: None,
            transport: "email".into(),
            reason: NotificationReason::Opened,
            status: NotificationStatus::Queued,
            attempt: 1,
            error: None,
            sent_at: None,
        })
        .await
        .expect("record queued");
    let now = chrono::Utc::now();
    assert!(
        store
            .pending_notifications(now, 50, 5)
            .await
            .unwrap()
            .iter()
            .all(|p| p.id != queued_id)
    );
    assert!(
        store
            .pending_notifications(
                now + chrono::Duration::seconds(QUEUED_TAKEOVER_SECS + 1),
                50,
                5
            )
            .await
            .unwrap()
            .iter()
            .any(|p| p.id == queued_id)
    );
    store
        .mark_notification(
            org,
            queued_id,
            NotificationOutcome {
                status: NotificationStatus::Sent,
                attempt: 1,
                error: None,
                sent_at: Some(chrono::Utc::now()),
                next_attempt_at: None,
                provider_receipt: None,
            },
        )
        .await
        .unwrap();

    // append_event lands on the timeline; cross-org reads see nothing.
    store
        .append_event(
            org,
            id,
            IncidentEventKind::Notified,
            Actor::System,
            Some("paged".into()),
        )
        .await
        .unwrap();
    let tl = store.timeline(org, id).await.unwrap();
    assert!(tl.iter().any(|e| e.kind == IncidentEventKind::Notified));

    let other_user = make_user(&pool, "incnotifx").await;
    let other = create_org_with_owner(&pool, other_user, &unique_slug("incnotifx"), "n")
        .await
        .unwrap()
        .unwrap();
    assert!(
        store
            .notifications_for(other.id, id)
            .await
            .unwrap()
            .is_empty(),
        "another org must not see this incident's paging log"
    );
}

#[tokio::test]
#[ignore]
async fn publish_sets_visibility_and_narration_then_unpublish_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user, id) = seed(&pool, "incpub").await;
    let store = PgIncidentOpsStore::new(pool.clone());

    // Publish seeds the public title; visibility flips to public.
    let pubd = store
        .publish(
            org,
            id,
            Some("EU API outage".into()),
            None,
            None,
            Actor::User(user),
        )
        .await
        .unwrap()
        .expect("incident exists");
    assert_eq!(
        pubd.visibility,
        uptimepage::domain::IncidentVisibility::Public
    );

    let (vis, title): (String, Option<String>) =
        sqlx::query_as("SELECT visibility, public_title FROM incidents WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(vis, "public");
    assert_eq!(title.as_deref(), Some("EU API outage"));

    // A second publish without a title must not clobber the stored copy.
    store
        .publish(org, id, None, None, None, Actor::User(user))
        .await
        .unwrap()
        .unwrap();
    let kept: Option<String> =
        sqlx::query_scalar("SELECT public_title FROM incidents WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(kept.as_deref(), Some("EU API outage"));

    let unpubd = store
        .unpublish(org, id, Actor::User(user))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        unpubd.visibility,
        uptimepage::domain::IncidentVisibility::Internal
    );

    let tl = store.timeline(org, id).await.unwrap();
    assert!(tl.iter().any(|e| e.kind == IncidentEventKind::Published));
    assert!(tl.iter().any(|e| e.kind == IncidentEventKind::Unpublished));

    // Cross-tenant publish is a no-op (returns None, leaves the row internal).
    let other_user = make_user(&pool, "incpubx").await;
    let other = create_org_with_owner(&pool, other_user, &unique_slug("incpubx"), "n")
        .await
        .unwrap()
        .unwrap();
    assert!(
        store
            .publish(other.id, id, None, None, None, Actor::User(other_user))
            .await
            .unwrap()
            .is_none(),
        "another org cannot publish this incident"
    );
}

#[tokio::test]
#[ignore]
async fn metrics_rolls_up_mtta_mttr_and_counts_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user, a) = seed(&pool, "incmetrics").await;
    let store = PgIncidentOpsStore::new(pool.clone());

    // A: human-acknowledged then human-resolved (contributes MTTA + MTTR).
    store
        .acknowledge(org, a, Actor::User(user), None, None)
        .await
        .unwrap();
    store
        .resolve(org, a, Actor::User(user), None)
        .await
        .unwrap();

    // B: manual incident auto-resolved by the system (no resolver).
    let b = store
        .declare(org, NewManualIncident::default(), Actor::User(user))
        .await
        .unwrap();
    store.auto_resolve(org, b.id).await.unwrap();

    // C: manual incident left triggered.
    store
        .declare(org, NewManualIncident::default(), Actor::User(user))
        .await
        .unwrap();

    let m = store.metrics(org, 30).await.unwrap();
    assert_eq!(m.window_days, 30);
    assert_eq!(m.total, 3);
    assert!(m.mtta_secs.is_some(), "A was acknowledged");
    assert!(m.mttr_secs.is_some(), "A and B were resolved");
    assert_eq!(m.auto_resolved, 1, "B auto-resolved");
    assert_eq!(m.human_resolved, 1, "A human-resolved");
    let resolved = m
        .by_state
        .iter()
        .find(|b| b.key == "resolved")
        .map(|b| b.count);
    assert_eq!(resolved, Some(2));
    // A carries a target; the noisy-monitor list surfaces it.
    assert!(m.top_monitors.iter().any(|t| t.count >= 1));

    // A short window that predates every incident yields zeroes, not an error.
    // (All incidents are seconds old, so window_days=1 still includes them; the
    // store clamps and never divides by zero on an empty set.)
    let other = store.metrics(org, 1).await.unwrap();
    assert_eq!(other.total, 3);

    // Cross-tenant isolation: a fresh org sees none of these.
    let other_user = make_user(&pool, "incmetricsx").await;
    let other_org = create_org_with_owner(&pool, other_user, &unique_slug("incmetricsx"), "n")
        .await
        .unwrap()
        .unwrap();
    let isolated = store.metrics(other_org.id, 30).await.unwrap();
    assert_eq!(isolated.total, 0);
    assert!(isolated.mtta_secs.is_none());
    assert!(isolated.mttr_secs.is_none());
}

#[tokio::test]
#[ignore]
async fn postmortem_publish_events_persist_with_actor_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user, id) = seed(&pool, "incpmev").await;
    let store = PgIncidentOpsStore::new(pool.clone());

    // These kinds are part of the incident_events CHECK; append_event must
    // accept them and record the acting user.
    store
        .append_event(
            org,
            id,
            IncidentEventKind::PostmortemPublished,
            Actor::User(user),
            None,
        )
        .await
        .unwrap();
    store
        .append_event(
            org,
            id,
            IncidentEventKind::PostmortemUnpublished,
            Actor::User(user),
            None,
        )
        .await
        .unwrap();

    let tl = store.timeline(org, id).await.unwrap();
    assert!(
        tl.iter()
            .any(|e| e.kind == IncidentEventKind::PostmortemPublished && e.actor_id == Some(user)),
        "postmortem publish is attributed on the timeline"
    );
    assert!(
        tl.iter()
            .any(|e| e.kind == IncidentEventKind::PostmortemUnpublished),
        "postmortem unpublish lands on the timeline"
    );
}

#[tokio::test]
#[ignore]
async fn due_for_reconcile_finds_only_unpaged_triggered_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user, never_paged) = seed(&pool, "increc").await;
    let store = PgIncidentOpsStore::new(pool.clone());

    // Old enough to be past any grace window.
    sqlx::query("UPDATE incidents SET started_at = now() - interval '1 hour' WHERE id = $1")
        .bind(never_paged)
        .execute(&pool)
        .await
        .unwrap();

    let window = (
        chrono::Utc::now() - chrono::Duration::hours(6),
        chrono::Utc::now() - chrono::Duration::seconds(60),
    );
    let due = store.due_for_reconcile(window, 100).await.unwrap();
    assert!(
        due.iter().any(|d| d.id == never_paged),
        "an old, never-paged, unarmed triggered incident is reconcilable"
    );

    // Once it has a paging-log row, it drops out of the reconcile set.
    store
        .record_notification(NewIncidentNotification {
            org,
            incident_id: never_paged,
            escalation_level: Some(0),
            target_user_id: None,
            channel_id: None,
            transport: "webhook".into(),
            reason: NotificationReason::Opened,
            status: NotificationStatus::Sent,
            attempt: 1,
            error: None,
            sent_at: Some(chrono::Utc::now()),
        })
        .await
        .unwrap();
    let due = store.due_for_reconcile(window, 100).await.unwrap();
    assert!(
        !due.iter().any(|d| d.id == never_paged),
        "a paged incident is no longer reconcilable"
    );

    // An acknowledged incident is never reconciled either.
    let (_o2, _u2, acked) = (org, user, seed(&pool, "increc2").await.2);
    sqlx::query("UPDATE incidents SET started_at = now() - interval '1 hour', state = 'acknowledged' WHERE id = $1")
        .bind(acked)
        .execute(&pool)
        .await
        .unwrap();
    let due = store.due_for_reconcile(window, 100).await.unwrap();
    assert!(!due.iter().any(|d| d.id == acked));
}

#[tokio::test]
#[ignore]
async fn renotify_backoff_widens_with_each_reminder_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user, id) = seed(&pool, "increno").await;
    let store = PgIncidentOpsStore::new(pool.clone());

    let channel: Uuid = sqlx::query_scalar(
        "INSERT INTO notification_channels (org_id, name, kind, config) \
         VALUES ($1, 'ops', 'webhook', '{}'::jsonb) RETURNING id",
    )
    .bind(org.0)
    .fetch_one(&pool)
    .await
    .expect("insert channel");
    store
        .record_notification(NewIncidentNotification {
            org,
            incident_id: id,
            escalation_level: Some(0),
            target_user_id: None,
            channel_id: Some(channel),
            transport: "webhook".into(),
            reason: NotificationReason::Opened,
            status: NotificationStatus::Sent,
            attempt: 1,
            error: None,
            sent_at: Some(chrono::Utc::now()),
        })
        .await
        .expect("record");

    async fn paged_ago(pool: &PgPool, id: Uuid, secs: i64) {
        sqlx::query(
            "UPDATE incident_notifications \
             SET created_at = now() - make_interval(secs => $2) WHERE incident_id = $1",
        )
        .bind(id)
        .bind(secs as f64)
        .execute(pool)
        .await
        .unwrap();
    }
    // Cross-org scan ordered oldest-page-first, so take the store's whole cap
    // rather than letting leftover incidents crowd this one out.
    async fn is_due(store: &PgIncidentOpsStore, id: Uuid) -> bool {
        store
            .due_for_renotify(chrono::Utc::now(), 1000)
            .await
            .unwrap()
            .iter()
            .any(|d| d.id == id)
    }

    // The monitor keeps the schema default of one hour.
    paged_ago(&pool, id, 3_700).await;
    assert!(
        is_due(&store, id).await,
        "first reminder is due at the monitor's interval"
    );

    // Past reminder 5 the doubling hits the day cap and stays there.
    for reminders in 1..=6u32 {
        store.bump_renotify_count(org, id).await.unwrap();
        let gap = (3600 * 2_i64.pow(reminders)).min(86_400);
        paged_ago(&pool, id, gap - 600).await;
        assert!(
            !is_due(&store, id).await,
            "after {reminders} reminder(s) the gap has widened to {gap}s"
        );
        paged_ago(&pool, id, gap + 600).await;
        assert!(
            is_due(&store, id).await,
            "and it comes due once that gap elapses"
        );
    }

    store.resolve(org, id, Actor::System, None).await.unwrap();
    store
        .reopen(org, id, Actor::User(user), None)
        .await
        .unwrap();
    paged_ago(&pool, id, 3_700).await;
    assert!(
        is_due(&store, id).await,
        "a reopened incident pages at the base interval again"
    );

    // A window that silences paging takes the incident out of the scan entirely,
    // so a long window costs no per-tick work.
    let mw: Uuid = sqlx::query_scalar(
        "INSERT INTO maintenance_windows (org_id, title, starts_at, ends_at, suppress_alerts) \
         VALUES ($1, 'w', now() - interval '5 minutes', now() + interval '1 hour', true) \
         RETURNING id",
    )
    .bind(org.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    let target_id: Uuid = sqlx::query_scalar("SELECT target_id FROM incidents WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO maintenance_window_components (org_id, maintenance_id, target_id) \
         VALUES ($1, $2, $3)",
    )
    .bind(org.0)
    .bind(mw)
    .bind(target_id)
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        !is_due(&store, id).await,
        "a silenced window holds the reminder"
    );
    sqlx::query("UPDATE maintenance_windows SET suppress_alerts = false WHERE id = $1")
        .bind(mw)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        is_due(&store, id).await,
        "an announcement-only window leaves reminders alone"
    );
    sqlx::query("DELETE FROM maintenance_windows WHERE id = $1")
        .bind(mw)
        .execute(&pool)
        .await
        .unwrap();

    // The day cap raises a short interval; it must never lower a long one.
    sqlx::query("UPDATE targets SET renotify_interval_secs = 604800 WHERE id = (SELECT target_id FROM incidents WHERE id = $1)")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    paged_ago(&pool, id, 90_000).await;
    assert!(
        !is_due(&store, id).await,
        "a weekly reminder interval is not pulled forward to daily"
    );
}

#[tokio::test]
#[ignore]
async fn a_maintenance_hold_releases_only_once_the_window_lets_go_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, _user, id) = seed(&pool, "incmaint").await;
    let store = PgIncidentOpsStore::new(pool.clone());
    let target_id: Uuid = sqlx::query_scalar("SELECT target_id FROM incidents WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();

    store
        .record_notification(NewIncidentNotification {
            org,
            incident_id: id,
            escalation_level: None,
            target_user_id: None,
            channel_id: None,
            transport: "maintenance".into(),
            reason: NotificationReason::Opened,
            status: NotificationStatus::Suppressed,
            attempt: 0,
            error: None,
            sent_at: None,
        })
        .await
        .expect("hold marker");

    async fn is_due(store: &PgIncidentOpsStore, id: Uuid) -> bool {
        store
            .due_for_maintenance_release(1000)
            .await
            .unwrap()
            .iter()
            .any(|d| d.id == id)
    }

    assert!(
        is_due(&store, id).await,
        "a hold with no window over it is ready to release"
    );

    // An active silencing window keeps it held.
    let mw: Uuid = sqlx::query_scalar(
        "INSERT INTO maintenance_windows (org_id, title, starts_at, ends_at, suppress_alerts) \
         VALUES ($1, 'w', now() - interval '5 minutes', now() + interval '1 hour', true) \
         RETURNING id",
    )
    .bind(org.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO maintenance_window_components (org_id, maintenance_id, target_id) \
         VALUES ($1, $2, $3)",
    )
    .bind(org.0)
    .bind(mw)
    .bind(target_id)
    .execute(&pool)
    .await
    .unwrap();
    assert!(!is_due(&store, id).await, "the window still covers it");

    // A window that only announces never held it in the first place.
    sqlx::query("UPDATE maintenance_windows SET suppress_alerts = false WHERE id = $1")
        .bind(mw)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        is_due(&store, id).await,
        "an announcement-only window releases"
    );

    sqlx::query("UPDATE maintenance_windows SET suppress_alerts = true, ends_at = now() - interval '1 minute' WHERE id = $1")
        .bind(mw)
        .execute(&pool)
        .await
        .unwrap();
    assert!(is_due(&store, id).await, "a finished window releases");

    // Once a real page lands the marker is spent.
    let channel: Uuid = sqlx::query_scalar(
        "INSERT INTO notification_channels (org_id, name, kind, config) \
         VALUES ($1, 'ops', 'webhook', '{}'::jsonb) RETURNING id",
    )
    .bind(org.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    store
        .record_notification(NewIncidentNotification {
            org,
            incident_id: id,
            escalation_level: Some(0),
            target_user_id: None,
            channel_id: Some(channel),
            transport: "webhook".into(),
            reason: NotificationReason::Opened,
            status: NotificationStatus::Sent,
            attempt: 1,
            error: None,
            sent_at: Some(chrono::Utc::now()),
        })
        .await
        .expect("release page");
    assert!(
        !is_due(&store, id).await,
        "a released hold does not page twice"
    );
}

#[tokio::test]
#[ignore]
async fn assign_rejects_non_member_assignee_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, owner, id) = seed(&pool, "incassign").await;
    let store = PgIncidentOpsStore::new(pool.clone());

    // A user who belongs to a different org must not be assignable.
    let outsider = make_user(&pool, "incassignx").await;
    create_org_with_owner(&pool, outsider, &unique_slug("incassignx"), "n")
        .await
        .unwrap()
        .unwrap();
    let err = store
        .assign(org, id, Some(outsider), Actor::User(owner))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, uptimepage::error::AppError::Unprocessable { code, .. }
            if *code == uptimepage::error::codes::ASSIGNEE_NOT_MEMBER),
        "cross-org assignee must be rejected, got {err:?}"
    );

    // The org's own member assigns fine, and can be cleared.
    let inc = store
        .assign(org, id, Some(owner), Actor::User(owner))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inc.assigned_to, Some(owner));
    let inc = store
        .assign(org, id, None, Actor::User(owner))
        .await
        .unwrap()
        .unwrap();
    assert!(inc.assigned_to.is_none());
}

#[tokio::test]
#[ignore]
async fn declare_conflicts_when_target_already_has_open_incident_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "incdup").await;
    let org = create_org_with_owner(&pool, user, &unique_slug("incdup"), "svc")
        .await
        .unwrap()
        .unwrap();
    let target_id: Uuid = sqlx::query_scalar(
        "INSERT INTO targets (org_id, name, check_spec, interval_secs) \
         VALUES ($1, 'svc', '{}'::jsonb, 30) RETURNING id",
    )
    .bind(org.id.0)
    .fetch_one(&pool)
    .await
    .expect("insert target");
    let store = PgIncidentOpsStore::new(pool.clone());
    let bound = |t| NewManualIncident {
        target_id: Some(t),
        ..Default::default()
    };

    store
        .declare(org.id, bound(target_id), Actor::User(user))
        .await
        .expect("first declare opens");

    let err = store
        .declare(org.id, bound(target_id), Actor::User(user))
        .await
        .expect_err("second declare for the same target must conflict");
    match err {
        uptimepage::error::AppError::Conflict { code, .. } => {
            assert_eq!(code, uptimepage::error::codes::INCIDENT_ALREADY_OPEN)
        }
        other => panic!("expected Conflict, got {other:?}"),
    }

    // Stand-alone (NULL target) declares never collide.
    store
        .declare(org.id, NewManualIncident::default(), Actor::User(user))
        .await
        .expect("null-target declare ok");
    store
        .declare(org.id, NewManualIncident::default(), Actor::User(user))
        .await
        .expect("second null-target declare ok");
}

#[tokio::test]
#[ignore]
async fn reopen_conflicts_when_target_has_a_newer_open_incident_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "increopen").await;
    let org = create_org_with_owner(&pool, user, &unique_slug("increopen"), "svc")
        .await
        .unwrap()
        .unwrap();
    let target_id: Uuid = sqlx::query_scalar(
        "INSERT INTO targets (org_id, name, check_spec, interval_secs) \
         VALUES ($1, 'svc', '{}'::jsonb, 30) RETURNING id",
    )
    .bind(org.id.0)
    .fetch_one(&pool)
    .await
    .expect("insert target");
    let store = PgIncidentOpsStore::new(pool.clone());
    let bound = || NewManualIncident {
        target_id: Some(target_id),
        ..Default::default()
    };

    let old = store
        .declare(org.id, bound(), Actor::User(user))
        .await
        .unwrap();
    store
        .resolve(org.id, old.id, Actor::User(user), None)
        .await
        .unwrap();
    // The target goes down again: a newer incident now holds the open slot.
    store
        .declare(org.id, bound(), Actor::User(user))
        .await
        .unwrap();

    let err = store
        .reopen(org.id, old.id, Actor::User(user), None)
        .await
        .expect_err("reopening the old incident must conflict with the newer open one");
    match err {
        uptimepage::error::AppError::Conflict { code, .. } => {
            assert_eq!(code, uptimepage::error::codes::INCIDENT_ALREADY_OPEN)
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
}

/// The two kinds of incident hold separate slots: an operator can record what
/// they are chasing without muting the monitor underneath it.
#[tokio::test]
#[ignore]
async fn a_declaration_and_a_monitor_incident_can_be_open_at_once_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user, monitor_incident) = seed(&pool, "incboth").await;
    let target_id: Uuid = sqlx::query_scalar("SELECT target_id FROM incidents WHERE id = $1")
        .bind(monitor_incident)
        .fetch_one(&pool)
        .await
        .expect("target of the seeded monitor incident");

    let store = PgIncidentOpsStore::new(pool.clone());
    let declared = store
        .declare(
            org,
            NewManualIncident {
                target_id: Some(target_id),
                ..Default::default()
            },
            Actor::User(user),
        )
        .await
        .expect("declaring alongside an open monitor incident");
    assert!(!declared.counts_as_downtime);

    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM incidents WHERE target_id = $1 AND ended_at IS NULL",
    )
    .bind(target_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(open, 2);
}

/// The one field in the amend surface that moves a published number is the one
/// that most needs to say who moved it.
#[tokio::test]
#[ignore]
async fn changing_downtime_accounting_lands_on_the_timeline_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user, _) = seed(&pool, "incdown").await;
    let store = PgIncidentOpsStore::new(pool.clone());
    let narration = uptimepage::storage::PgIncidentNarrationStore::new(pool.clone());
    let declared = store
        .declare(org, NewManualIncident::default(), Actor::User(user))
        .await
        .expect("declare");

    let patched = uptimepage::storage::IncidentNarrationStore::patch_narration(
        &narration,
        org,
        declared.id,
        uptimepage::domain::IncidentNarrationUpdate {
            counts_as_downtime: Some(true),
            ..Default::default()
        },
    )
    .await
    .expect("patch")
    .expect("incident exists");
    assert!(patched.counts_as_downtime);

    store
        .append_event(
            org,
            declared.id,
            IncidentEventKind::DowntimeChanged,
            Actor::User(user),
            Some("now counts toward uptime".into()),
        )
        .await
        .expect("append event");
    let kinds: Vec<IncidentEventKind> = store
        .timeline(org, declared.id)
        .await
        .expect("timeline")
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert!(
        kinds.contains(&IncidentEventKind::DowntimeChanged),
        "{kinds:?}"
    );
}

#[tokio::test]
async fn retry_scan_skips_a_first_attempt_still_in_flight_in_memory() {
    let store = InMemoryIncidentOpsStore::new();
    let org = OrgId(Uuid::from_u128(1));
    let queued_id = store
        .record_notification(NewIncidentNotification {
            org,
            incident_id: Uuid::from_u128(2),
            escalation_level: Some(0),
            target_user_id: None,
            channel_id: Some(Uuid::from_u128(3)),
            transport: "email".into(),
            reason: NotificationReason::Opened,
            status: NotificationStatus::Queued,
            attempt: 1,
            error: None,
            sent_at: None,
        })
        .await
        .unwrap();
    let now = chrono::Utc::now();
    assert!(
        store
            .pending_notifications(now, 50, 5)
            .await
            .unwrap()
            .is_empty()
    );
    let pending = store
        .pending_notifications(
            now + chrono::Duration::seconds(QUEUED_TAKEOVER_SECS + 1),
            50,
            5,
        )
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, queued_id);
}

// Emergency-receipt lifecycle on the in-memory store (no DB): a sent page with
// a receipt is pollable until acknowledged or its receipt is cleared.
#[tokio::test]
async fn emergency_ack_lifecycle_in_memory() {
    let store = InMemoryIncidentOpsStore::new();
    let org = OrgId(Uuid::from_u128(1));
    let incident = Uuid::from_u128(2);
    let channel = Uuid::from_u128(3);

    let record = || async {
        let id = store
            .record_notification(NewIncidentNotification {
                org,
                incident_id: incident,
                escalation_level: Some(0),
                target_user_id: None,
                channel_id: Some(channel),
                transport: "pushover".into(),
                reason: NotificationReason::Opened,
                status: NotificationStatus::Queued,
                attempt: 1,
                error: None,
                sent_at: None,
            })
            .await
            .unwrap();
        store
            .mark_notification(
                org,
                id,
                NotificationOutcome {
                    status: NotificationStatus::Sent,
                    attempt: 1,
                    error: None,
                    sent_at: Some(chrono::Utc::now()),
                    next_attempt_at: None,
                    provider_receipt: Some("rcpt-abc".into()),
                },
            )
            .await
            .unwrap();
        id
    };

    let acked_id = record().await;
    let due = store.due_emergency_acks(10).await.unwrap();
    assert!(
        due.iter()
            .any(|a| a.id == acked_id && a.receipt == "rcpt-abc" && a.channel_id == channel)
    );
    assert!(
        store
            .emergency_acks_for_incident(org, incident)
            .await
            .unwrap()
            .iter()
            .any(|a| a.id == acked_id)
    );

    // Acknowledged rows drop out of the poll set.
    store
        .mark_acked(org, acked_id, chrono::Utc::now())
        .await
        .unwrap();
    assert!(
        store
            .due_emergency_acks(10)
            .await
            .unwrap()
            .iter()
            .all(|a| a.id != acked_id)
    );

    // A cleared receipt (cancelled/expired) also leaves the set.
    let cleared_id = record().await;
    store.clear_receipt(org, cleared_id).await.unwrap();
    assert!(
        store
            .due_emergency_acks(10)
            .await
            .unwrap()
            .iter()
            .all(|a| a.id != cleared_id)
    );
}

#[tokio::test]
#[ignore]
async fn publish_posts_opening_update_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user, id) = seed(&pool, "incopen").await;
    let store = PgIncidentOpsStore::new(pool.clone());

    // First publish of an internal incident posts one opening update.
    store
        .publish(
            org,
            id,
            Some("API errors".into()),
            Some("Looking into it".into()),
            None,
            Actor::User(user),
        )
        .await
        .unwrap()
        .expect("published");
    let (phase, message): (String, String) =
        sqlx::query_as("SELECT phase, message FROM incident_updates WHERE incident_id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(phase, "investigating");
    assert_eq!(message, "Looking into it");

    // Re-publishing does not post a second opening update.
    store
        .publish(org, id, None, None, None, Actor::User(user))
        .await
        .unwrap();
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM incident_updates WHERE incident_id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1, "re-publish posts no new update");

    // An incident already narrated before publish gets no synthesized opener.
    let (org2, user2, id2) = seed(&pool, "incnarr").await;
    sqlx::query(
        "INSERT INTO incident_updates (org_id, incident_id, phase, message, author) \
         VALUES ($1, $2, 'identified', 'root cause found', 'op')",
    )
    .bind(org2.0)
    .bind(id2)
    .execute(&pool)
    .await
    .unwrap();
    store
        .publish(org2, id2, None, None, None, Actor::User(user2))
        .await
        .unwrap();
    let count2: i64 =
        sqlx::query_scalar("SELECT count(*) FROM incident_updates WHERE incident_id = $1")
            .bind(id2)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count2, 1, "pre-narrated incident gets no opener");

    // A retro-published, already-resolved incident gets no "investigating" opener.
    let (org3, user3, id3) = seed(&pool, "incresolved").await;
    sqlx::query("UPDATE incidents SET state = 'resolved', ended_at = now() WHERE id = $1")
        .bind(id3)
        .execute(&pool)
        .await
        .unwrap();
    store
        .publish(org3, id3, None, None, None, Actor::User(user3))
        .await
        .unwrap();
    let count3: i64 =
        sqlx::query_scalar("SELECT count(*) FROM incident_updates WHERE incident_id = $1")
            .bind(id3)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count3, 0, "resolved incident gets no opener");
}

#[tokio::test]
#[ignore]
async fn resolve_after_unpublish_still_writes_closing_update_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let store = PgIncidentOpsStore::new(pool.clone());

    // Published then unpublished before resolve: subscribers were told it
    // opened, so the closing update must still be written despite the row
    // being internal at resolve time.
    let (org, user, id) = seed(&pool, "increunpub").await;
    store
        .publish(
            org,
            id,
            Some("Outage".into()),
            None,
            None,
            Actor::User(user),
        )
        .await
        .unwrap()
        .unwrap();
    store
        .unpublish(org, id, Actor::User(user))
        .await
        .unwrap()
        .unwrap();
    store
        .resolve(org, id, Actor::User(user), None)
        .await
        .unwrap();
    let resolved: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM incident_updates WHERE incident_id = $1 AND phase = 'resolved'",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(resolved, 1, "ever-published incident gets a closing update");

    // Never published: resolving writes no public update at all.
    let (org2, user2, id2) = seed(&pool, "increnever").await;
    store
        .resolve(org2, id2, Actor::User(user2), None)
        .await
        .unwrap();
    let none: i64 =
        sqlx::query_scalar("SELECT count(*) FROM incident_updates WHERE incident_id = $1")
            .bind(id2)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(none, 0, "internal-only incident stays silent");
}

/// An acknowledgement that arrives through a notification names nobody, and
/// that unknown identity is as much "the first responder" as a named one: a
/// later ack must not backfill itself over it. It is still recorded, though —
/// an engineer taking a page a notification already acknowledged is a real
/// action, and reporting success while writing nothing would hide it.
#[tokio::test]
#[ignore]
async fn an_unattributed_ack_keeps_its_credit_but_still_logs_the_next_one_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user, id) = seed(&pool, "inclinkack").await;
    let store = PgIncidentOpsStore::new(pool.clone());

    let first = updated(
        store
            .acknowledge(
                org,
                id,
                Actor::Link,
                Some("Acknowledged in Pushover".into()),
                None,
            )
            .await
            .unwrap(),
    );
    assert_eq!(first.state, IncidentState::Acknowledged);
    assert_eq!(first.acknowledged_by, None);
    let acked_at = first.acknowledged_at.expect("stamped");

    // A named user acking afterwards takes no credit for it.
    let again = updated(
        store
            .acknowledge(org, id, Actor::User(user), Some("me too".into()), None)
            .await
            .unwrap(),
    );
    assert_eq!(again.acknowledged_by, None, "credit stays with the first");
    assert_eq!(again.acknowledged_at, Some(acked_at));

    // Both actions are on the timeline.
    let events: Vec<(String, Option<Uuid>, Option<String>)> = sqlx::query_as(
        "SELECT actor_type, actor_id, message FROM incident_events \
         WHERE incident_id = $1 AND kind = 'acknowledged' ORDER BY occurred_at",
    )
    .bind(id)
    .fetch_all(&pool)
    .await
    .expect("read events");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].0, "link");
    assert_eq!(events[0].1, None);
    assert_eq!(events[0].2.as_deref(), Some("Acknowledged in Pushover"));
    assert_eq!(events[1].0, "user");
    assert_eq!(events[1].1, Some(user.0));

    // The trail records the anonymous one too, even with no user to point at.
    let audit: Vec<(Option<Uuid>, serde_json::Value)> = sqlx::query_as(
        "SELECT actor_id, metadata FROM org_audit_log \
         WHERE org_id = $1 AND action = 'incident.acknowledged' ORDER BY occurred_at",
    )
    .bind(org.0)
    .fetch_all(&pool)
    .await
    .expect("read audit log");
    assert_eq!(audit.len(), 2);
    assert_eq!(audit[0].0, None);
    assert_eq!(audit[0].1["actor_type"], "link");
    assert_eq!(audit[0].1["incident_id"], id.to_string());
    assert_eq!(audit[1].0, Some(user.0));
}

/// A resolve that lands twice writes the second to the internal timeline, but
/// the customer-facing page does not get told the same thing twice.
#[tokio::test]
#[ignore]
async fn a_repeated_resolve_posts_one_public_update_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user, id) = seed(&pool, "increresolve").await;
    let store = PgIncidentOpsStore::new(pool.clone());

    store
        .publish(
            org,
            id,
            Some("outage".into()),
            None,
            None,
            Actor::User(user),
        )
        .await
        .expect("publish");
    store
        .resolve(org, id, Actor::User(user), Some("fixed".into()))
        .await
        .expect("resolve");
    store
        .resolve(org, id, Actor::User(user), Some("still fixed".into()))
        .await
        .expect("resolve again");

    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM incident_events WHERE incident_id = $1 AND kind = 'resolved'",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .expect("count events");
    assert_eq!(events, 2, "both actions are on the internal timeline");

    let updates: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM incident_updates WHERE incident_id = $1 AND phase = 'resolved'",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .expect("count updates");
    assert_eq!(updates, 1);
}

/// The episode guard, on the real SQL: a page from before a resolve/reopen must
/// not silence the outage that followed it. Counted off the append-only
/// timeline and checked under the same lock as the transition.
#[tokio::test]
#[ignore]
async fn an_ack_pinned_to_a_finished_episode_is_refused_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user, id) = seed(&pool, "incepisode").await;
    let store = PgIncidentOpsStore::new(pool.clone());

    assert_eq!(store.generation(org, id).await.unwrap(), Some(0));
    store
        .resolve(org, id, Actor::User(user), None)
        .await
        .expect("resolve");
    store
        .reopen(org, id, Actor::User(user), None)
        .await
        .expect("reopen");
    assert_eq!(store.generation(org, id).await.unwrap(), Some(1));

    let outcome = store
        .acknowledge(
            org,
            id,
            Actor::Link,
            Some("from the old page".into()),
            Some(0),
        )
        .await
        .expect("acknowledge");
    assert!(
        matches!(outcome, LifecycleOutcome::Stale),
        "expected Stale, got {outcome:?}"
    );
    let after = store.get(org, id).await.unwrap().expect("incident");
    assert_eq!(after.state, IncidentState::Triggered);
    assert_eq!(after.acknowledged_at, None);

    // A refusal leaves no trace on the timeline: nothing happened.
    let acks: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM incident_events WHERE incident_id = $1 AND kind = 'acknowledged'",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .expect("count acks");
    assert_eq!(acks, 0);

    // The page for the outage that is actually running does work.
    let outcome = store
        .acknowledge(
            org,
            id,
            Actor::Link,
            Some("from the new page".into()),
            Some(1),
        )
        .await
        .expect("acknowledge");
    assert_eq!(
        updated(outcome).state,
        IncidentState::Acknowledged,
        "the current episode's page still acknowledges"
    );

    // A console ack pins no episode and is unaffected by any of this.
    assert_eq!(store.generation(org, Uuid::now_v7()).await.unwrap(), None);
}

/// A Pushover page carries the episode it belonged to, so an acknowledgement
/// that arrives in the provider's app long after a reopen cannot land on the
/// outage that followed. The engine must not have to rely on the reopen having
/// cancelled the receipt: the reopen signal can be dropped under load, and the
/// cancel itself can fail.
#[tokio::test]
#[ignore]
async fn an_emergency_receipt_remembers_the_outage_it_paged_for_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user, id) = seed(&pool, "increceiptgen").await;
    let store = PgIncidentOpsStore::new(pool.clone());
    let channel_id: Uuid = sqlx::query_scalar(
        "INSERT INTO notification_channels (org_id, name, kind, config) \
         VALUES ($1, 'pager', 'pushover', '{}'::jsonb) RETURNING id",
    )
    .bind(org.0)
    .fetch_one(&pool)
    .await
    .expect("insert channel");

    let record = |receipt: &str| {
        let receipt = receipt.to_string();
        async {
            store
                .record_notification(NewIncidentNotification {
                    org,
                    incident_id: id,
                    escalation_level: None,
                    target_user_id: None,
                    channel_id: Some(channel_id),
                    transport: "pushover".to_string(),
                    reason: NotificationReason::Opened,
                    status: NotificationStatus::Sent,
                    attempt: 1,
                    error: None,
                    sent_at: Some(chrono::Utc::now()),
                })
                .await
                .expect("record notification");
            let row: Uuid = sqlx::query_scalar(
                "UPDATE incident_notifications SET provider_receipt = $2 \
                 WHERE id = (SELECT id FROM incident_notifications \
                             WHERE incident_id = $1 ORDER BY created_at DESC LIMIT 1) \
                 RETURNING id",
            )
            .bind(id)
            .bind(receipt)
            .fetch_one(&pool)
            .await
            .expect("attach receipt");
            row
        }
    };

    let first = record("rcpt-first").await;
    store
        .resolve(org, id, Actor::User(user), None)
        .await
        .expect("resolve");
    store
        .reopen(org, id, Actor::User(user), None)
        .await
        .expect("reopen");
    let second = record("rcpt-second").await;

    let acks = store
        .emergency_acks_for_incident(org, id)
        .await
        .expect("emergency acks");
    let gen_of = |row: Uuid| {
        acks.iter()
            .find(|a| a.id == row)
            .map(|a| a.generation)
            .expect("receipt listed")
    };
    assert_eq!(gen_of(first), 0, "paged before the reopen");
    assert_eq!(gen_of(second), 1, "paged after it");

    // The old page cannot take the new outage; the new one can.
    assert!(matches!(
        store
            .acknowledge(org, id, Actor::Link, None, Some(gen_of(first)))
            .await
            .expect("acknowledge"),
        LifecycleOutcome::Stale
    ));
    assert_eq!(
        store.get(org, id).await.unwrap().unwrap().state,
        IncidentState::Triggered
    );
    assert_eq!(
        updated(
            store
                .acknowledge(org, id, Actor::Link, None, Some(gen_of(second)))
                .await
                .expect("acknowledge")
        )
        .state,
        IncidentState::Acknowledged
    );
}

async fn seed_status_page(pool: &PgPool, org: OrgId) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO status_pages (org_id, slug, name, enabled) VALUES ($1, $2, 'p', true) \
         RETURNING id",
    )
    .bind(org.0)
    .bind(unique_slug("incpage"))
    .fetch_one(pool)
    .await
    .expect("insert status page")
}

fn refused_with(err: uptimepage::error::AppError, want: &str) {
    match err {
        uptimepage::error::AppError::BadRequest { code, .. } => assert_eq!(code, want),
        other => panic!("expected {want}, got {other:?}"),
    }
}

/// With no monitor there is no component to route by, so the pages it shows
/// on are exactly the ones named, and publishing to none is refused rather
/// than reported as a success nobody can see.
#[tokio::test]
#[ignore]
async fn an_incident_without_a_monitor_is_published_to_the_pages_it_names_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user, _) = seed(&pool, "incpages").await;
    let store = PgIncidentOpsStore::new(pool.clone());
    let first = seed_status_page(&pool, org).await;
    let second = seed_status_page(&pool, org).await;
    let codes = uptimepage::error::codes::INCIDENT_STATUS_PAGE_REQUIRED;

    let bare = store
        .declare(
            org,
            NewManualIncident {
                title: Some("network outage".into()),
                ..Default::default()
            },
            Actor::User(user),
        )
        .await
        .expect("declare");
    refused_with(
        store
            .publish(org, bare.id, None, None, None, Actor::User(user))
            .await
            .expect_err("no page to show it on"),
        codes,
    );
    let still: String = sqlx::query_scalar("SELECT visibility FROM incidents WHERE id = $1")
        .bind(bare.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(still, "internal", "a refused publish leaves it internal");

    store
        .publish(
            org,
            bare.id,
            None,
            None,
            Some(vec![first, second, first]),
            Actor::User(user),
        )
        .await
        .expect("publish to two pages")
        .expect("incident exists");
    let mut want = vec![first, second];
    want.sort();
    assert_eq!(store.status_pages(org, bare.id).await.unwrap(), want);

    store
        .publish(
            org,
            bare.id,
            None,
            None,
            Some(vec![second]),
            Actor::User(user),
        )
        .await
        .expect("narrow to one page");
    assert_eq!(
        store.status_pages(org, bare.id).await.unwrap(),
        vec![second]
    );
    let published: Vec<serde_json::Value> = sqlx::query_scalar(
        "SELECT metadata->'status_page_ids' FROM org_audit_log \
         WHERE org_id = $1 AND action = 'incident.published' ORDER BY occurred_at",
    )
    .bind(org.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        published,
        vec![serde_json::json!(want), serde_json::json!([second])],
        "each publish records the pages it left the incident on"
    );

    let paged = store
        .declare(
            org,
            NewManualIncident {
                title: Some("dns outage".into()),
                status_page_ids: vec![first],
                ..Default::default()
            },
            Actor::User(user),
        )
        .await
        .expect("declare with a page");
    let declared: Vec<Option<serde_json::Value>> = sqlx::query_scalar(
        "SELECT metadata->'status_page_ids' FROM org_audit_log \
         WHERE org_id = $1 AND action = 'incident.declared' \
           AND metadata->>'incident_id' IN ($2::text, $3::text) ORDER BY occurred_at",
    )
    .bind(org.0)
    .bind(bare.id)
    .bind(paged.id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(declared, vec![None, Some(serde_json::json!([first]))]);

    refused_with(
        store
            .publish(org, bare.id, None, None, Some(vec![]), Actor::User(user))
            .await
            .expect_err("clearing every page"),
        codes,
    );
    assert_eq!(
        store.status_pages(org, bare.id).await.unwrap(),
        vec![second],
        "a refused publish keeps the pages it had"
    );
}

/// A page id from another org reads as unknown, and nothing is linked.
#[tokio::test]
#[ignore]
async fn an_incident_cannot_be_posted_to_another_orgs_page_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user, _) = seed(&pool, "incpagex").await;
    let (other_org, _, _) = seed(&pool, "incpagey").await;
    let store = PgIncidentOpsStore::new(pool.clone());
    let mine = seed_status_page(&pool, org).await;
    let theirs = seed_status_page(&pool, other_org).await;

    refused_with(
        store
            .declare(
                org,
                NewManualIncident {
                    title: Some("x".into()),
                    status_page_ids: vec![mine, theirs],
                    ..Default::default()
                },
                Actor::User(user),
            )
            .await
            .expect_err("foreign page"),
        uptimepage::error::codes::INVALID_STATUS_PAGE_ID,
    );
    let linked: i64 =
        sqlx::query_scalar("SELECT count(*) FROM incident_status_pages WHERE status_page_id = $1")
            .bind(theirs)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(linked, 0);
    let declared: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM incidents WHERE org_id = $1 AND origin = 'manual'",
    )
    .bind(org.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(declared, 0, "the declaration rolled back with its pages");
}

/// A monitor's incident shows wherever the monitor does, so naming pages for
/// it is refused rather than silently ignored.
#[tokio::test]
#[ignore]
async fn pages_are_refused_for_an_incident_with_a_monitor_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (org, user, id) = seed(&pool, "incpagemon").await;
    let store = PgIncidentOpsStore::new(pool.clone());
    let page = seed_status_page(&pool, org).await;
    let codes = uptimepage::error::codes::INCIDENT_STATUS_PAGES_WITH_MONITOR;
    let target: Uuid = sqlx::query_scalar("SELECT target_id FROM incidents WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();

    refused_with(
        store
            .publish(org, id, None, None, Some(vec![page]), Actor::User(user))
            .await
            .expect_err("pages on a monitor's incident"),
        codes,
    );
    refused_with(
        store
            .declare(
                org,
                NewManualIncident {
                    target_id: Some(target),
                    status_page_ids: vec![page],
                    ..Default::default()
                },
                Actor::User(user),
            )
            .await
            .expect_err("pages on a monitor's declaration"),
        codes,
    );
}
