use chrono::Utc;
use uuid::Uuid;

use crate::domain::{
    IncidentEventKind, IncidentOrigin, IncidentSeverity, IncidentState, IncidentUrgency,
    IncidentVisibility, OpsIncident, OrgId, UserId,
};

use super::pg::resolved_public_message;
use super::{
    Actor, InMemoryIncidentOpsStore, IncidentOpsFilter, IncidentOpsStore, LifecycleOutcome,
};

#[test]
fn resolved_public_message_uses_note_or_default() {
    assert_eq!(
        resolved_public_message(Some("rolled back deploy")),
        "rolled back deploy"
    );
    assert_eq!(
        resolved_public_message(Some("   ")),
        "This incident has been resolved."
    );
    assert_eq!(
        resolved_public_message(None),
        "This incident has been resolved."
    );
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
async fn acknowledge_sets_owner_and_stops_escalation() {
    let store = InMemoryIncidentOpsStore::new();
    let id = seed_triggered(&store);
    let u = user();
    let inc = unwrap_updated(
        store
            .acknowledge(org(), id, Actor::User(u), Some("on it".into()), None)
            .await
            .unwrap(),
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
            .unwrap(),
    );
    let first_at = acked.acknowledged_at;
    // A second responder re-acks; ownership + time must not be overwritten.
    let again = unwrap_updated(
        store
            .acknowledge(org(), id, Actor::User(user()), None, None)
            .await
            .unwrap(),
    );
    assert_eq!(again.acknowledged_by, Some(first));
    assert_eq!(again.acknowledged_at, first_at);
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
        .unwrap();
    assert!(matches!(out, LifecycleOutcome::IllegalTransition(_)));
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

#[test]
fn ack_link_is_bound_to_incident_episode_channel_and_expiry() {
    use super::{incident_ack_token, incident_ack_url, verify_incident_ack};

    let org = OrgId(Uuid::from_u128(1));
    let incident = Uuid::from_u128(2);
    let channel = Uuid::from_u128(3);
    let episode = 0i64;
    let exp = 1_800_000_000i64;
    let mac = incident_ack_token("s3cret", org, incident, channel, episode, exp);
    let ok = |secret, org, incident, channel, episode, exp, mac| {
        verify_incident_ack(secret, org, incident, channel, episode, exp, mac)
    };

    assert!(ok("s3cret", org, incident, channel, episode, exp, &mac));
    assert!(!ok("other", org, incident, channel, episode, exp, &mac));
    assert!(!ok(
        "s3cret",
        OrgId(Uuid::from_u128(9)),
        incident,
        channel,
        episode,
        exp,
        &mac
    ));
    assert!(!ok(
        "s3cret",
        org,
        Uuid::from_u128(9),
        channel,
        episode,
        exp,
        &mac
    ));
    assert!(!ok(
        "s3cret",
        org,
        incident,
        Uuid::from_u128(9),
        episode,
        exp,
        &mac
    ));
    // The episode: a link minted before a reopen must not verify after one.
    assert!(!ok(
        "s3cret",
        org,
        incident,
        channel,
        episode + 1,
        exp,
        &mac
    ));
    assert!(!ok(
        "s3cret",
        org,
        incident,
        channel,
        episode,
        exp + 1,
        &mac
    ));
    assert!(!ok("s3cret", org, incident, channel, episode, exp, ""));

    // Separated in the signed input, so moving a digit from one to the other
    // cannot forge a match.
    assert_ne!(
        incident_ack_token("s3cret", org, incident, channel, 1, 23),
        incident_ack_token("s3cret", org, incident, channel, 12, 3)
    );

    let now = Utc::now();
    let url = incident_ack_url(
        "https://app.example.com/",
        "s3cret",
        org,
        incident,
        channel,
        7,
        now,
    )
    .expect("link with a base url and a secret");
    assert!(url.starts_with("https://app.example.com/incident/ack?"));
    assert!(url.contains("&g=7&"));
    assert!(url.contains(&format!("e={}", now.timestamp() + super::ACK_LINK_TTL_SECS)));

    // No base URL and no secret each mean no link at all.
    assert!(incident_ack_url("", "s3cret", org, incident, channel, 0, now).is_none());
    assert!(
        incident_ack_url(
            "https://app.example.com",
            "",
            org,
            incident,
            channel,
            0,
            now
        )
        .is_none()
    );
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
