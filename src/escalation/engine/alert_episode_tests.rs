//! Incidents another tool sent in page through their alert source: its policy
//! or channels, its pause, its damper key and its kept name once deleted.

use super::*;

use crate::domain::{
    AlertBinding, AlertGrouping, AlertSourceKind, AlertSourceUpdate, IncidentAlertLink,
    IncidentAlertSourceRef, IncidentOrigin, NewAlertSource, NewEscalationPolicy,
    NotificationStatus, TargetAlerts, WriteSource,
};
use crate::storage::alert_sources::{AlertSourceStore, InMemoryAlertSourceStore};
use crate::storage::{
    Actor, DueIncident, InMemoryEscalationPolicyStore, InMemoryIncidentOpsStore,
    InMemoryNotificationChannelStore, InMemoryTargetStore,
};

use super::deliver::Rebuilt;
use super::tests::{channel_step, failing_channel, org, seed_incident};

struct Harness {
    ops: Arc<InMemoryIncidentOpsStore>,
    sources: Arc<InMemoryAlertSourceStore>,
    policies: Arc<InMemoryEscalationPolicyStore>,
    channels: Arc<InMemoryNotificationChannelStore>,
    channel: Uuid,
}

impl Harness {
    async fn new() -> Self {
        let channels = Arc::new(InMemoryNotificationChannelStore::new());
        let channel = failing_channel(&channels).await;
        Self {
            ops: Arc::new(InMemoryIncidentOpsStore::new()),
            sources: Arc::new(InMemoryAlertSourceStore::new()),
            policies: Arc::new(InMemoryEscalationPolicyStore::new()),
            channels,
            channel,
        }
    }

    fn engine(&self, cfg: EscalationConfig) -> EscalationEngine {
        let (_tx, rx) = mpsc::channel(4);
        EscalationEngine::new(
            rx,
            EngineDeps {
                ops: self.ops.clone(),
                policies: self.policies.clone(),
                on_call: Arc::new(crate::storage::InMemoryOnCallStore::new()),
                contacts: Arc::new(crate::storage::InMemoryContactStore::new()),
                linked_apps: Arc::new(crate::storage::InMemoryLinkedAppStore::new()),
                targets: Arc::new(InMemoryTargetStore::from_vec(Vec::new())),
                groups: Arc::new(crate::storage::InMemoryMonitorGroupStore::new()),
                sources: self.sources.clone(),
                channels: self.channels.clone(),
                maintenance: Arc::new(crate::storage::InMemoryMaintenanceStore::new()),
                orgs: Arc::new(crate::storage::orgs::InMemoryOrgDirectory::new()),
                http: crate::http_outbound::build_outbound_client(
                    crate::security::SsrfGuard::relaxed_for_tests(),
                ),
                cfg,
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

    async fn source(&self, policy: Option<Uuid>) -> crate::domain::AlertSource {
        self.sources
            .create(
                org(),
                NewAlertSource {
                    name: format!("prom-{}", Uuid::now_v7()),
                    kind: AlertSourceKind::Alertmanager,
                    grouping: AlertGrouping::Group,
                    escalation_policy_id: policy,
                    alerts: TargetAlerts(vec![AlertBinding {
                        channel_id: self.channel,
                    }]),
                    notify_recovery: true,
                    renotify_interval_secs: 3600,
                    signing_secret: None,
                },
                WriteSource::Api,
                None,
                25,
            )
            .await
            .unwrap()
    }

    /// An open alert incident from `source` under `key`, owed its first page.
    fn incident(&self, source: &crate::domain::AlertSource, key: &str) -> Uuid {
        let scratch = InMemoryIncidentOpsStore::new();
        let id = seed_incident(&scratch, None);
        let mut incident = scratch.snapshot(id);
        incident.origin = IncidentOrigin::Alert;
        incident.title = Some("KubePodCrashLooping".into());
        incident.alert = IncidentAlertLink {
            alert_source: Some(IncidentAlertSourceRef {
                id: Some(source.id),
                name: source.name.clone(),
            }),
            dedup_key: Some(key.into()),
            closed_by_source_delete: false,
        };
        self.ops.seed(incident);
        id
    }

    async fn paged(&self, id: Uuid) -> Vec<crate::domain::IncidentNotification> {
        self.ops
            .notifications_for(org(), id)
            .await
            .unwrap()
            .into_iter()
            .filter(|n| n.channel_id == Some(self.channel))
            .collect()
    }
}

#[tokio::test]
async fn an_alert_pages_its_sources_channels_and_names_the_source() {
    let h = Harness::new().await;
    let source = h.source(None).await;
    let id = h.incident(&source, "group-1");
    let eng = h.engine(EscalationConfig::default());

    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    let rows = h.paged(id).await;
    assert_eq!(rows.len(), 1, "the bound channel is paged");
    assert_eq!(rows[0].reason, NotificationReason::Opened);

    let Rebuilt::Ready(ready) = eng
        .w
        .rebuild_notice(org(), id, h.channel, NotificationReason::Opened, rows[0].id)
        .await
        .unwrap()
    else {
        panic!("a retry of an alert page rebuilds");
    };
    let (notice, _, _) = *ready;
    assert_eq!(notice.source.as_deref(), Some(source.name.as_str()));
    assert_eq!(notice.monitor_name, None);
    assert_eq!(notice.label(), "KubePodCrashLooping");
}

#[tokio::test]
async fn a_dropped_alert_signal_is_paged_by_the_sweep() {
    let h = Harness::new().await;
    let source = h.source(None).await;
    let id = h.incident(&source, "group-1");
    let eng = h.engine(EscalationConfig::default());
    h.ops.age_owed_pages(chrono::Duration::minutes(1));

    eng.page_owed().await;
    assert_eq!(h.paged(id).await.len(), 1);
    assert!(!h.ops.owes_page(id));
}

#[tokio::test]
async fn a_paused_source_holds_its_first_page_until_it_resumes() {
    let h = Harness::new().await;
    let source = h.source(None).await;
    let pause = |enabled| AlertSourceUpdate {
        enabled: Some(enabled),
        ..Default::default()
    };
    h.sources
        .update(org(), source.id, pause(false), WriteSource::Api, None)
        .await
        .unwrap();
    let id = h.incident(&source, "group-1");
    let eng = h.engine(EscalationConfig::default());

    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    assert!(
        h.paged(id).await.is_empty(),
        "nothing goes out while paused"
    );
    assert!(h.ops.owes_page(id), "still owed");

    h.sources
        .update(org(), source.id, pause(true), WriteSource::Api, None)
        .await
        .unwrap();
    h.ops.age_owed_pages(chrono::Duration::minutes(1));
    eng.page_owed().await;
    assert_eq!(h.paged(id).await.len(), 1, "paged once it resumes");
}

#[tokio::test]
async fn an_alert_walks_its_sources_ladder_and_reminds() {
    let h = Harness::new().await;
    let second = failing_channel(&h.channels).await;
    let policy = h
        .policies
        .create(
            org(),
            NewEscalationPolicy {
                name: "ladder".into(),
                description: None,
                repeat_count: 0,
                steps: vec![channel_step(1, 0, h.channel), channel_step(2, 0, second)],
            },
            10,
        )
        .await
        .unwrap();
    let source = h.source(Some(policy.id)).await;
    let id = h.incident(&source, "group-1");
    let eng = h.engine(EscalationConfig::default());

    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    let inc = h.ops.get(org(), id).await.unwrap().unwrap();
    assert_eq!(inc.escalation_policy_id, Some(policy.id));
    assert_eq!(inc.escalation_level, 1);

    eng.escalate_due().await;
    let inc = h.ops.get(org(), id).await.unwrap().unwrap();
    assert_eq!(inc.escalation_level, 2, "the ladder walks with no monitor");
    let rows = h.ops.notifications_for(org(), id).await.unwrap();
    assert!(
        rows.iter()
            .any(|n| n.channel_id == Some(second) && n.reason == NotificationReason::Escalated)
    );

    eng.escalate_due().await;
    eng.renotify_one(&DueIncident {
        id,
        org: org(),
        target_id: None,
        escalation_policy_id: Some(policy.id),
        escalation_level: 2,
        escalation_round: 0,
    })
    .await
    .unwrap();
    let rows = h.ops.notifications_for(org(), id).await.unwrap();
    assert!(
        rows.iter()
            .any(|n| n.reason == NotificationReason::Reminder),
        "a still-open alert is reminded about"
    );
}

/// The damper counts one alert key, so a flapping alert is held without
/// holding the rest of its source, and a hold that outlasts its window pages.
#[tokio::test]
async fn a_flapping_alert_is_held_per_key_and_released_if_it_stays_open() {
    let h = Harness::new().await;
    let source = h.source(None).await;
    let cfg = EscalationConfig {
        flap_max_opens: 1,
        flap_hold_secs: 600,
        ..Default::default()
    };
    let eng = h.engine(cfg);

    let crossing = h.incident(&source, "flappy");
    eng.page(org(), crossing, NotificationReason::Opened)
        .await
        .unwrap();
    assert_eq!(h.paged(crossing).await.len(), 1, "the crossing still pages");

    let held = h.incident(&source, "flappy");
    eng.page(org(), held, NotificationReason::Opened)
        .await
        .unwrap();
    let rows = h.ops.notifications_for(org(), held).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].channel_id, None, "held, not delivered");

    let other = h.incident(&source, "steady");
    eng.page(org(), other, NotificationReason::Opened)
        .await
        .unwrap();
    assert_eq!(h.paged(other).await.len(), 1, "another key pages as usual");

    h.ops.age_held_rows(chrono::Duration::seconds(900));
    eng.release_held().await;
    assert_eq!(
        h.paged(held).await.len(),
        1,
        "released once the hold expires"
    );
}

#[tokio::test]
async fn a_failed_alert_page_is_retried() {
    let h = Harness::new().await;
    let source = h.source(None).await;
    let id = h.incident(&source, "group-1");
    let eng = h.engine(EscalationConfig::default());

    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    h.ops.clear_retry_backoff();
    eng.retry_pending().await;
    let rows = h.paged(id).await;
    assert_eq!(rows.len(), 1);
    assert!(rows[0].attempt > 1, "the retry rebuilt and resent the page");
}

/// A deleted source pages nothing more, but the people it paged still hear the
/// incident ended, under the name the incident kept.
#[tokio::test]
async fn the_close_is_told_after_the_source_is_deleted() {
    let h = Harness::new().await;
    let source = h.source(None).await;
    let id = h.incident(&source, "group-1");
    let eng = h.engine(EscalationConfig::default());
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();

    h.sources.delete(org(), source.id, None).await.unwrap();
    h.ops.edit(id, |i| {
        if let Some(s) = i.alert.alert_source.as_mut() {
            s.id = None;
        }
    });
    let Rebuilt::SubjectDeleted = eng
        .w
        .rebuild_notice(
            org(),
            id,
            h.channel,
            NotificationReason::Escalated,
            Uuid::nil(),
        )
        .await
        .unwrap()
    else {
        panic!("nothing pages once the source is gone");
    };

    h.ops.resolve(org(), id, Actor::System, None).await.unwrap();
    eng.page(org(), id, NotificationReason::Resolved)
        .await
        .unwrap();
    let closing: Vec<_> = h
        .paged(id)
        .await
        .into_iter()
        .filter(|n| n.reason == NotificationReason::Resolved)
        .collect();
    assert_eq!(closing.len(), 1, "the channel paged hears it ended");
    assert_ne!(closing[0].status, NotificationStatus::Queued);

    let refused = h
        .ops
        .reopen(org(), id, Actor::System, None)
        .await
        .unwrap_err();
    assert!(
        matches!(refused, crate::error::AppError::Conflict { code, .. }
            if code == crate::error::codes::INCIDENT_ALERT_SOURCE_DELETED),
        "{refused:?}"
    );
}

/// An emergency page stops repeating once its source is paused, as one for a
/// paused monitor does, and once its source is gone.
#[tokio::test]
async fn an_emergency_page_stops_when_its_source_is_paused_or_deleted() {
    let h = Harness::new().await;
    let source = h.source(None).await;
    let id = h.incident(&source, "group-1");
    let eng = h.engine(EscalationConfig::default());
    let ack = crate::storage::EmergencyAck {
        id: Uuid::now_v7(),
        org: org(),
        incident_id: id,
        channel_id: h.channel,
        receipt: "r".into(),
        generation: 0,
    };
    assert!(!eng.page_is_spent(&ack).await, "a live alert keeps paging");

    let pause = AlertSourceUpdate {
        enabled: Some(false),
        ..Default::default()
    };
    h.sources
        .update(org(), source.id, pause, WriteSource::Api, None)
        .await
        .unwrap();
    assert!(eng.page_is_spent(&ack).await, "paused");

    h.sources.delete(org(), source.id, None).await.unwrap();
    assert!(eng.page_is_spent(&ack).await, "deleted");
}
