use chrono::Utc;
use uuid::Uuid;

use crate::domain::{
    ActorType, IncidentAcknowledgement, IncidentEventKind, IncidentOrigin, IncidentSeverity,
    IncidentState, IncidentUrgency, IncidentVisibility, NewIncidentNotification,
    NotificationReason, NotificationStatus, OpsIncident, OrgId, UserId,
};

use super::pg::resolved_public_message;
use super::{
    Actor, InMemoryIncidentOpsStore, IncidentOpsFilter, IncidentOpsStore, LifecycleOutcome,
};

#[test]
fn resolved_public_message_uses_note_or_default() {
    let written = resolved_public_message(Some("rolled back deploy"));
    assert_eq!(written.message, "rolled back deploy");
    assert!(!written.generated, "the operator's own words");
    for blank in [Some("   "), None] {
        let default = resolved_public_message(blank);
        assert_eq!(default.message, "This incident has been resolved.");
        assert!(default.generated, "the platform wrote it");
    }
}

fn org() -> OrgId {
    OrgId(Uuid::nil())
}

fn user() -> UserId {
    UserId(Uuid::now_v7())
}

fn seed_triggered(store: &InMemoryIncidentOpsStore) -> Uuid {
    let id = Uuid::now_v7();
    let now = Utc::now();
    store.seed(OpsIncident {
        id,
        target_id: Some(Uuid::now_v7()),
        target_ref: None,
        target_name: None,
        target_kind: None,
        closed_by_monitor_delete: false,
        title: None,
        state: IncidentState::Triggered,
        severity: IncidentSeverity::Major,
        urgency: IncidentUrgency::High,
        origin: IncidentOrigin::Monitor,
        visibility: IncidentVisibility::Internal,
        paging_enabled: true,
        counts_as_downtime: true,
        started_at: now,
        ended_at: None,
        acknowledged_at: None,
        acknowledged_by: None,
        assigned_to: None,
        resolved_by: None,
        escalation_policy_id: None,
        escalation_level: 0,
        escalation_round: 0,
        next_escalation_at: Some(now),
        check_count: 2,
        error_sample: Some("boom".into()),
        regions_down: Vec::new(),
        regions_up: Vec::new(),
        created_at: now,
        updated_at: now,
        recovering_since: None,
    });
    id
}

fn unwrap_updated(o: LifecycleOutcome) -> OpsIncident {
    match o {
        LifecycleOutcome::Updated(i) => *i,
        other => panic!("expected Updated, got {other:?}"),
    }
}

#[tokio::test]
async fn a_close_by_monitor_delete_stays_out_of_resolution_metrics() {
    let store = InMemoryIncidentOpsStore::new();
    let id = seed_triggered(&store);
    store.edit(id, |i| {
        i.state = IncidentState::Resolved;
        i.ended_at = Some(i.started_at + chrono::Duration::days(20));
        i.resolved_by = Some(user());
        i.closed_by_monitor_delete = true;
    });
    let m = store.metrics(org(), 30).await.unwrap();
    assert_eq!(m.total, 1);
    assert_eq!((m.human_resolved, m.auto_resolved), (0, 0));
    assert_eq!(m.closed_with_monitor, 1);
    assert_eq!(m.mttr_secs, None, "a 20-day cleanup is not a 20-day repair");
}

#[tokio::test]
async fn only_a_declared_incident_reopens_once_its_monitor_is_deleted() {
    let store = InMemoryIncidentOpsStore::new();
    let monitors = seed_triggered(&store);
    let declared = seed_triggered(&store);
    for id in [monitors, declared] {
        store.resolve(org(), id, Actor::System, None).await.unwrap();
        store.edit(id, |i| {
            i.target_ref = i.target_id.take();
            i.target_name = Some("api".into());
        });
    }
    store.edit(declared, |i| i.origin = IncidentOrigin::Manual);

    assert!(
        store
            .reopen(org(), monitors, Actor::System, None)
            .await
            .is_err()
    );
    let reopened = store
        .reopen(org(), declared, Actor::System, None)
        .await
        .unwrap();
    assert!(matches!(reopened, LifecycleOutcome::Updated(_)));
}

#[tokio::test]
async fn an_incident_that_ended_with_its_monitor_takes_no_new_page() {
    let store = InMemoryIncidentOpsStore::new();
    let id = seed_triggered(&store);
    store.resolve(org(), id, Actor::System, None).await.unwrap();
    store.edit(id, |i| {
        i.target_ref = i.target_id.take();
        i.target_name = Some("api".into());
    });
    let page = Uuid::now_v7();
    assert!(
        store
            .publish(org(), id, None, None, Some(vec![page]), Actor::System)
            .await
            .is_err()
    );
    assert!(
        store
            .publish(org(), id, None, None, None, Actor::System)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn metrics_keep_two_deleted_monitors_that_shared_a_name_apart() {
    let store = InMemoryIncidentOpsStore::new();
    let (first, second) = (Uuid::now_v7(), Uuid::now_v7());
    for monitor in [first, first, second] {
        let id = seed_triggered(&store);
        store.edit(id, |i| {
            i.target_id = None;
            i.target_ref = Some(monitor);
            i.target_name = Some("api".into());
        });
    }
    let m = store.metrics(org(), 30).await.unwrap();
    let apis: Vec<u64> = m
        .top_monitors
        .iter()
        .filter(|t| t.name == "api")
        .map(|t| t.count)
        .collect();
    assert_eq!(apis, vec![2, 1]);
}

#[tokio::test]
async fn acknowledge_sets_owner_and_stops_escalation() {
    let store = InMemoryIncidentOpsStore::new();
    let id = seed_triggered(&store);
    let u = user();
    let inc = unwrap_updated(
        store
            .acknowledge(org(), id, Actor::User(u), Some("on it".into()), None)
            .await
            .unwrap()
            .outcome,
    );
    assert_eq!(inc.state, IncidentState::Acknowledged);
    assert_eq!(inc.acknowledged_by, Some(u));
    assert!(inc.next_escalation_at.is_none());
    let tl = store.timeline(org(), id).await.unwrap();
    assert_eq!(tl.len(), 1);
    assert_eq!(tl[0].kind, IncidentEventKind::Acknowledged);
}

#[tokio::test]
async fn re_acknowledge_keeps_first_acker() {
    let store = InMemoryIncidentOpsStore::new();
    let id = seed_triggered(&store);
    let first = user();
    let acked = unwrap_updated(
        store
            .acknowledge(org(), id, Actor::User(first), None, None)
            .await
            .unwrap()
            .outcome,
    );
    let first_at = acked.acknowledged_at;
    // A second responder re-acks; ownership + time must not be overwritten.
    let again = unwrap_updated(
        store
            .acknowledge(org(), id, Actor::User(user()), None, None)
            .await
            .unwrap()
            .outcome,
    );
    assert_eq!(again.acknowledged_by, Some(first));
    assert_eq!(again.acknowledged_at, first_at);
}

#[tokio::test]
async fn repeat_acknowledgement_logs_once_per_responder_per_episode() {
    let store = InMemoryIncidentOpsStore::new();
    let id = seed_triggered(&store);
    let (alice, bob) = (user(), user());
    let acks = || async {
        store
            .timeline(org(), id)
            .await
            .unwrap()
            .iter()
            .filter(|e| e.kind == IncidentEventKind::Acknowledged)
            .count()
    };
    for actor in [
        Actor::Link,
        Actor::User(alice),
        Actor::User(alice),
        Actor::Mcp(alice),
        Actor::Link,
        Actor::User(bob),
    ] {
        let out = store
            .acknowledge(org(), id, actor, None, None)
            .await
            .unwrap()
            .outcome;
        assert!(matches!(out, LifecycleOutcome::Updated(_)));
    }
    assert_eq!(
        acks().await,
        3,
        "the notification, alice and bob, once each"
    );

    // A repeat that brings a note keeps the note, as a note.
    store
        .acknowledge(
            org(),
            id,
            Actor::User(bob),
            Some("db failover".into()),
            None,
        )
        .await
        .unwrap();
    assert_eq!(acks().await, 3);
    let last = store.timeline(org(), id).await.unwrap().pop().unwrap();
    assert_eq!(last.kind, IncidentEventKind::Note);
    assert_eq!(last.message.as_deref(), Some("db failover"));

    // A notification's note is its fixed label, not something anyone wrote.
    let before = store.timeline(org(), id).await.unwrap().len();
    store
        .acknowledge(
            org(),
            id,
            Actor::Link,
            Some("Acknowledged in Pushover".into()),
            None,
        )
        .await
        .unwrap();
    assert_eq!(store.timeline(org(), id).await.unwrap().len(), before);

    let who = |acks: Vec<IncidentAcknowledgement>| {
        acks.into_iter()
            .map(|a| (a.actor_type, a.actor_id))
            .collect::<Vec<_>>()
    };
    let list = store
        .acknowledgements(org(), &[id])
        .await
        .unwrap()
        .remove(&id)
        .unwrap();
    assert_eq!(
        who(list),
        [
            (ActorType::Link, None),
            (ActorType::User, Some(alice)),
            (ActorType::User, Some(bob)),
        ]
    );

    store
        .resolve(org(), id, Actor::User(alice), None)
        .await
        .unwrap();
    store
        .reopen(org(), id, Actor::User(alice), None)
        .await
        .unwrap();
    store
        .acknowledge(org(), id, Actor::User(alice), None, None)
        .await
        .unwrap();
    assert_eq!(acks().await, 4, "a reopened incident is a new episode");
    let list = store
        .acknowledgements(org(), &[id])
        .await
        .unwrap()
        .remove(&id)
        .unwrap();
    assert_eq!(who(list), [(ActorType::User, Some(alice))]);
}

#[tokio::test]
async fn cannot_acknowledge_resolved() {
    let store = InMemoryIncidentOpsStore::new();
    let id = seed_triggered(&store);
    store
        .resolve(org(), id, Actor::User(user()), None)
        .await
        .unwrap();
    let out = store
        .acknowledge(org(), id, Actor::User(user()), None, None)
        .await
        .unwrap()
        .outcome;
    assert!(matches!(out, LifecycleOutcome::IllegalTransition(_)));
}

/// A page from before a resolve/reopen must not close the outage that followed
/// it, and a second press on a closed incident records nothing.
#[tokio::test]
async fn a_resolve_from_an_alert_is_pinned_to_its_episode_and_needs_an_open_incident() {
    let store = InMemoryIncidentOpsStore::new();
    let id = seed_triggered(&store);
    let (alice, bob) = (user(), user());
    let events = || async { store.timeline(org(), id).await.unwrap().len() };

    let out = store
        .resolve_episode(org(), id, Actor::User(alice), 3)
        .await
        .unwrap();
    assert!(matches!(out, LifecycleOutcome::Stale), "{out:?}");
    assert_eq!(events().await, 0, "a refusal leaves no trace");

    let inc = unwrap_updated(
        store
            .resolve_episode(org(), id, Actor::User(alice), 0)
            .await
            .unwrap(),
    );
    assert_eq!(inc.state, IncidentState::Resolved);
    assert_eq!(inc.resolved_by, Some(alice));
    let closed = events().await;

    let out = store
        .resolve_episode(org(), id, Actor::User(bob), 0)
        .await
        .unwrap();
    assert!(
        matches!(out, LifecycleOutcome::IllegalTransition(_)),
        "{out:?}"
    );
    assert_eq!(events().await, closed, "a second press adds nothing");
    let kept = store.get(org(), id).await.unwrap().unwrap();
    assert_eq!(kept.resolved_by, Some(alice), "the first resolver keeps it");

    store
        .reopen(org(), id, Actor::User(alice), None)
        .await
        .unwrap();
    let out = store
        .resolve_episode(org(), id, Actor::User(bob), 0)
        .await
        .unwrap();
    assert!(matches!(out, LifecycleOutcome::Stale), "{out:?}");
    let inc = unwrap_updated(
        store
            .resolve_episode(org(), id, Actor::User(bob), 1)
            .await
            .unwrap(),
    );
    assert_eq!(inc.resolved_by, Some(bob));
}

#[tokio::test]
async fn manual_resolve_records_user_auto_resolve_does_not() {
    let store = InMemoryIncidentOpsStore::new();
    let id = seed_triggered(&store);
    let u = user();
    let inc = unwrap_updated(
        store
            .resolve(org(), id, Actor::User(u), None)
            .await
            .unwrap(),
    );
    assert_eq!(inc.state, IncidentState::Resolved);
    assert_eq!(inc.resolved_by, Some(u));
    assert!(inc.ended_at.is_some());

    let id2 = seed_triggered(&store);
    let inc2 = unwrap_updated(store.auto_resolve(org(), id2).await.unwrap());
    assert_eq!(inc2.resolved_by, None);
}

#[tokio::test]
async fn reopen_resets_resolution_and_ack() {
    let store = InMemoryIncidentOpsStore::new();
    let id = seed_triggered(&store);
    store
        .acknowledge(org(), id, Actor::User(user()), None, None)
        .await
        .unwrap();
    store
        .resolve(org(), id, Actor::User(user()), None)
        .await
        .unwrap();
    let inc = unwrap_updated(
        store
            .reopen(org(), id, Actor::User(user()), None)
            .await
            .unwrap(),
    );
    assert_eq!(inc.state, IncidentState::Triggered);
    assert!(inc.ended_at.is_none());
    assert!(inc.acknowledged_by.is_none());
    assert!(inc.resolved_by.is_none());
}

#[tokio::test]
async fn assign_and_unassign_log_events() {
    let store = InMemoryIncidentOpsStore::new();
    let id = seed_triggered(&store);
    let u = user();
    let inc = store
        .assign(org(), id, Some(u), Actor::User(u))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inc.assigned_to, Some(u));
    store.assign(org(), id, None, Actor::User(u)).await.unwrap();
    let tl = store.timeline(org(), id).await.unwrap();
    assert_eq!(tl.len(), 2);
    assert_eq!(tl[0].kind, IncidentEventKind::Assigned);
    assert_eq!(tl[1].kind, IncidentEventKind::Unassigned);
}

#[tokio::test]
async fn publish_then_unpublish_flips_visibility_and_logs() {
    let store = InMemoryIncidentOpsStore::new();
    let id = seed_triggered(&store);
    let u = user();
    let pubd = store
        .publish(
            org(),
            id,
            Some("EU outage".into()),
            None,
            None,
            Actor::User(u),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pubd.visibility, IncidentVisibility::Public);
    let unpubd = store
        .unpublish(org(), id, Actor::User(u))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unpubd.visibility, IncidentVisibility::Internal);
    let kinds: Vec<_> = store
        .timeline(org(), id)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert!(kinds.contains(&IncidentEventKind::Published));
    assert!(kinds.contains(&IncidentEventKind::Unpublished));
}

#[tokio::test]
async fn publish_missing_incident_is_none() {
    let store = InMemoryIncidentOpsStore::new();
    let res = store
        .publish(org(), Uuid::now_v7(), None, None, None, Actor::System)
        .await
        .unwrap();
    assert!(res.is_none());
}

#[tokio::test]
async fn add_note_on_missing_incident_is_none() {
    let store = InMemoryIncidentOpsStore::new();
    let res = store
        .add_note(org(), Uuid::now_v7(), Actor::System, "x".into())
        .await
        .unwrap();
    assert!(res.is_none());
}

#[tokio::test]
async fn list_filters_by_state() {
    let store = InMemoryIncidentOpsStore::new();
    let a = seed_triggered(&store);
    let _b = seed_triggered(&store);
    store.resolve(org(), a, Actor::System, None).await.unwrap();
    let triggered = store
        .list(
            org(),
            IncidentOpsFilter {
                state: Some(IncidentState::Triggered),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(triggered.len(), 1);
    let resolved = store
        .list(
            org(),
            IncidentOpsFilter {
                state: Some(IncidentState::Resolved),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(resolved.len(), 1);
}

#[tokio::test]
async fn filters_by_severity_and_assignee_with_counts() {
    let store = InMemoryIncidentOpsStore::new();
    let u = user();
    // Two triggered (one critical, mine), one resolved minor.
    let a = seed_triggered(&store);
    let b = seed_triggered(&store);
    let c = seed_triggered(&store);
    store.edit(a, |i| {
        i.severity = IncidentSeverity::Critical;
        i.assigned_to = Some(u);
    });
    store.edit(c, |i| i.severity = IncidentSeverity::Minor);
    let _ = b;
    store.resolve(org(), c, Actor::System, None).await.unwrap();

    // Severity filter.
    let crit = store
        .list(
            org(),
            IncidentOpsFilter {
                severity: Some(IncidentSeverity::Critical),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(crit.len(), 1);
    assert_eq!(crit[0].id, a);

    // Assignee filter ("mine").
    let mine = store
        .list(
            org(),
            IncidentOpsFilter {
                assignee: Some(u),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].id, a);

    // counts_by_state ignores the state filter, honours severity/assignee.
    let counts = store
        .counts_by_state(org(), &IncidentOpsFilter::default())
        .await
        .unwrap();
    assert_eq!(counts.triggered, 2);
    assert_eq!(counts.resolved, 1);
    assert_eq!(counts.total(), 3);

    let mine_counts = store
        .counts_by_state(
            org(),
            &IncidentOpsFilter {
                assignee: Some(u),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(mine_counts.triggered, 1);
    assert_eq!(mine_counts.total(), 1);
}

#[tokio::test]
async fn an_incident_without_a_monitor_publishes_only_to_named_pages() {
    let store = InMemoryIncidentOpsStore::new();
    let u = user();
    let inc = store
        .declare(
            org(),
            crate::domain::NewManualIncident::default(),
            Actor::User(u),
        )
        .await
        .unwrap();
    let refused = store
        .publish(org(), inc.id, None, None, None, Actor::User(u))
        .await
        .unwrap_err();
    assert!(
        matches!(refused, crate::error::AppError::BadRequest { code, .. }
            if code == crate::error::codes::INCIDENT_STATUS_PAGE_REQUIRED),
        "{refused:?}"
    );
    let page = Uuid::now_v7();
    let pubd = store
        .publish(org(), inc.id, None, None, Some(vec![page]), Actor::User(u))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pubd.visibility, IncidentVisibility::Public);
    assert_eq!(store.status_pages(org(), inc.id).await.unwrap(), vec![page]);
}

/// A close owes its notice, one claim at a time, until the holder settles it.
#[tokio::test]
async fn a_close_owes_one_closing_notice_until_its_holder_settles_it() {
    let store = InMemoryIncidentOpsStore::new();
    let id = seed_triggered(&store);
    assert_eq!(
        store.claim_closing_notice(org(), id, 60).await.unwrap(),
        None
    );

    store.resolve(org(), id, Actor::System, None).await.unwrap();
    let claim = store
        .claim_closing_notice(org(), id, 60)
        .await
        .unwrap()
        .expect("a close owes its notice");
    assert_eq!(
        store.claim_closing_notice(org(), id, 60).await.unwrap(),
        None,
        "held by the first claim"
    );
    let due = store.due_closing_notices(0, 86_400, 60, 10).await.unwrap();
    assert!(due.claimed.is_empty(), "a leased notice is not due");

    store.settle_closing_notice(org(), id, claim).await.unwrap();
    assert!(!store.closing_notice_pending(id));
}

#[tokio::test]
async fn a_reopen_withdraws_the_closing_notice() {
    let store = InMemoryIncidentOpsStore::new();
    let id = seed_triggered(&store);
    store.resolve(org(), id, Actor::System, None).await.unwrap();
    store.reopen(org(), id, Actor::System, None).await.unwrap();
    assert!(!store.closing_notice_pending(id));
    assert_eq!(
        store.claim_closing_notice(org(), id, 60).await.unwrap(),
        None
    );
}

/// Settling under a claim that lapsed must not clear the notice whoever took
/// it next is still sending.
#[tokio::test]
async fn a_lapsed_claim_settles_nothing() {
    let store = InMemoryIncidentOpsStore::new();
    let id = seed_triggered(&store);
    store.resolve(org(), id, Actor::System, None).await.unwrap();
    let lapsed = store
        .claim_closing_notice(org(), id, 1)
        .await
        .unwrap()
        .unwrap();
    store.age_closing_notices(chrono::Duration::minutes(1));
    let due = store.due_closing_notices(0, 86_400, 60, 10).await.unwrap();
    assert_eq!(due.claimed.len(), 1, "a lapsed lease falls due again");

    store
        .settle_closing_notice(org(), id, lapsed)
        .await
        .unwrap();
    assert!(store.closing_notice_pending(id));
    store
        .settle_closing_notice(org(), id, due.claimed[0].claim)
        .await
        .unwrap();
    assert!(!store.closing_notice_pending(id));
}

#[tokio::test]
async fn the_sweep_waits_out_the_grace_and_withdraws_what_ended_outside_the_window() {
    let store = InMemoryIncidentOpsStore::new();
    let fresh = seed_triggered(&store);
    store
        .resolve(org(), fresh, Actor::System, None)
        .await
        .unwrap();
    let due = store.due_closing_notices(15, 86_400, 60, 10).await.unwrap();
    assert!(due.claimed.is_empty(), "inside the grace");

    store.age_closing_notices(chrono::Duration::days(2));
    let due = store.due_closing_notices(15, 86_400, 60, 10).await.unwrap();
    assert!(due.claimed.is_empty());
    assert_eq!(due.expired, 1);
    assert!(!store.closing_notice_pending(fresh));
}

fn closing_row(id: Uuid) -> NewIncidentNotification {
    NewIncidentNotification {
        org: org(),
        incident_id: id,
        escalation_level: Some(0),
        target_user_id: None,
        channel_id: Some(Uuid::now_v7()),
        transport: "webhook".into(),
        reason: NotificationReason::Resolved,
        status: NotificationStatus::Queued,
        attempt: 1,
        error: None,
        sent_at: None,
        episode: 0,
    }
}

/// Each row of the notice renews the claim it went out under. A sender whose
/// claim was taken over, or withdrawn by a reopen, records nothing.
#[tokio::test]
async fn a_closing_row_is_recorded_only_under_the_claim_that_holds_the_notice() {
    let store = InMemoryIncidentOpsStore::new();
    let id = seed_triggered(&store);
    store.resolve(org(), id, Actor::System, None).await.unwrap();
    let claim = store
        .claim_closing_notice(org(), id, 60)
        .await
        .unwrap()
        .unwrap();
    let (_, renewed) = store
        .record_closing_notification(closing_row(id), claim, 60)
        .await
        .unwrap()
        .expect("held");

    store.age_closing_notices(chrono::Duration::minutes(2));
    let taken = store.due_closing_notices(0, 86_400, 60, 10).await.unwrap();
    assert_eq!(
        store
            .record_closing_notification(closing_row(id), renewed, 60)
            .await
            .unwrap(),
        None
    );
    let (_, held) = store
        .record_closing_notification(closing_row(id), taken.claimed[0].claim, 60)
        .await
        .unwrap()
        .expect("the new holder's");
    assert_eq!(store.notifications_for(org(), id).await.unwrap().len(), 2);

    store.reopen(org(), id, Actor::System, None).await.unwrap();
    assert_eq!(
        store
            .record_closing_notification(closing_row(id), held, 60)
            .await
            .unwrap(),
        None
    );
    assert_eq!(store.notifications_for(org(), id).await.unwrap().len(), 2);
}
