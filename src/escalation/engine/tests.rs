use super::*;
use std::collections::HashMap;
use std::time::Duration as StdDuration;

use crate::domain::NotificationStatus;
use crate::domain::{
    AlertBinding, ChannelConfig, CheckSpec, EmailConfig, EscalationTargetType, ExpectedStatus,
    HttpCheck, HttpMethod, IncidentOrigin, IncidentSeverity, IncidentState, IncidentUrgency,
    IncidentVisibility, NewEscalationPolicy, NewEscalationStep, NewEscalationTarget,
    NewNotificationChannel, NotificationChannelUpdate, OpsIncident, Target, TargetAlerts,
    WebhookConfig, WriteSource,
};
use crate::security::redaction::redact_url_paths;
use crate::storage::{
    Actor, DueIncident, InMemoryContactStore, InMemoryEscalationPolicyStore,
    InMemoryIncidentOpsStore, InMemoryNotificationChannelStore, InMemoryOnCallStore,
    InMemoryTargetStore,
};

use super::rules::{FlapState, flap_state, log_error_snippet, retry_after_hint, retry_delay_secs};

pub(super) fn org() -> OrgId {
    OrgId(Uuid::nil())
}

// A channel whose transport always fails fast (closed loopback port), so a
// delivery attempt records `failed` without needing a mock HTTP server.
async fn failing_channel(store: &InMemoryNotificationChannelStore) -> Uuid {
    store
        .create(
            org(),
            NewNotificationChannel {
                name: format!("ops-{}", Uuid::now_v7()),
                config: ChannelConfig::Webhook(WebhookConfig {
                    url: "http://127.0.0.1:1/notify".into(),
                    headers: Default::default(),
                    secret: None,
                }),
                enabled: true,
                auto_bind_tags: Vec::new(),
                acknowledge_button: true,
                resolve_button: false,
            },
            WriteSource::Ui,
            100,
            None,
        )
        .await
        .unwrap()
        .id
}

pub(super) fn target_with_channel(channel_id: Uuid) -> Target {
    target_with_channel_recovery(channel_id, true)
}

fn target_with_channel_recovery(channel_id: Uuid, notify_recovery: bool) -> Target {
    Target {
        id: Uuid::now_v7(),
        name: "api".into(),
        check: CheckSpec::Http(HttpCheck {
            url: url::Url::parse("https://example.com/").unwrap(),
            method: HttpMethod::Get,
            timeout: StdDuration::from_secs(5),
            follow_redirects: false,
            max_redirects: 0,
            expected_status: ExpectedStatus::Exact(200),
            expected_body_contains: None,
            headers: HashMap::new(),
            body: None,
            verify_tls: true,
            basic_auth: None,
            bearer_token: None,
        }),
        interval: StdDuration::from_secs(30),
        enabled: true,
        tags: vec![],
        alerts: TargetAlerts(vec![AlertBinding { channel_id }]),
        alert_confirmations: 1,
        notify_recovery,
        renotify_interval_secs: 3600,
        region_policy: Default::default(),
        group_name: None,
        owner_user_id: None,
        write_source: WriteSource::Ui,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        plan_hold_at: None,
    }
}

fn bare_target() -> Target {
    let mut t = target_with_channel(Uuid::now_v7());
    t.alerts = TargetAlerts(vec![]);
    t
}

pub(super) fn seed_incident(ops: &InMemoryIncidentOpsStore, target_id: Option<Uuid>) -> Uuid {
    let now = Utc::now();
    let id = Uuid::now_v7();
    ops.seed(OpsIncident {
        id,
        target_id,
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
        next_escalation_at: None,
        check_count: 2,
        error_sample: Some("boom".into()),
        regions_down: Vec::new(),
        regions_up: Vec::new(),
        created_at: now,
        updated_at: now,
    });
    id
}

fn channel_step(level: i32, delay: i32, channel: Uuid) -> NewEscalationStep {
    NewEscalationStep {
        level,
        delay_secs: delay,
        targets: vec![NewEscalationTarget {
            target_type: EscalationTargetType::Channel,
            user_id: None,
            schedule_id: None,
            channel_id: Some(channel),
        }],
    }
}

fn engine(
    ops: Arc<dyn IncidentOpsStore>,
    policies: Arc<dyn EscalationPolicyStore>,
    targets: Arc<dyn TargetStore>,
    channels: Arc<dyn NotificationChannelStore>,
) -> EscalationEngine {
    engine_with(
        ops,
        policies,
        Arc::new(InMemoryOnCallStore::new()),
        Arc::new(InMemoryContactStore::new()),
        targets,
        channels,
    )
}

/// Same as [`engine`], with the escalation config under test.
fn engine_cfg(
    ops: Arc<dyn IncidentOpsStore>,
    policies: Arc<dyn EscalationPolicyStore>,
    targets: Arc<dyn TargetStore>,
    channels: Arc<dyn NotificationChannelStore>,
    cfg: EscalationConfig,
) -> EscalationEngine {
    let (_tx, rx) = mpsc::channel(4);
    EscalationEngine::new(
        rx,
        EngineDeps {
            ops,
            policies,
            on_call: Arc::new(InMemoryOnCallStore::new()),
            contacts: Arc::new(InMemoryContactStore::new()),
            linked_apps: Arc::new(crate::storage::InMemoryLinkedAppStore::new()),
            targets,
            channels,
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

/// [`engine_cfg`] with a capturing mail sender and an org that has an owner.
pub(super) fn engine_mailing(
    ops: Arc<dyn IncidentOpsStore>,
    targets: Arc<dyn TargetStore>,
    channels: Arc<dyn NotificationChannelStore>,
    cfg: EscalationConfig,
) -> (EscalationEngine, crate::email::InMemoryEmailSender) {
    engine_mailing_owned(ops, targets, channels, cfg, true)
}

/// [`engine_mailing`], with `owner` controlling whether the org has anyone to
/// mail at all.
fn engine_mailing_owned(
    ops: Arc<dyn IncidentOpsStore>,
    targets: Arc<dyn TargetStore>,
    channels: Arc<dyn NotificationChannelStore>,
    cfg: EscalationConfig,
    owner: bool,
) -> (EscalationEngine, crate::email::InMemoryEmailSender) {
    let sender = crate::email::InMemoryEmailSender::new();
    let orgs = Arc::new(crate::storage::orgs::InMemoryOrgDirectory::new());
    orgs.insert(org(), "Acme Inc");
    if owner {
        orgs.insert_owner_email(org(), "owner@example.test");
    }
    let (_tx, rx) = mpsc::channel(4);
    let engine = EscalationEngine::new(
        rx,
        EngineDeps {
            ops,
            policies: Arc::new(InMemoryEscalationPolicyStore::new()),
            on_call: Arc::new(InMemoryOnCallStore::new()),
            contacts: Arc::new(InMemoryContactStore::new()),
            linked_apps: Arc::new(crate::storage::InMemoryLinkedAppStore::new()),
            targets,
            channels,
            maintenance: Arc::new(crate::storage::InMemoryMaintenanceStore::new()),
            orgs,
            http: crate::http_outbound::build_outbound_client(
                crate::security::SsrfGuard::relaxed_for_tests(),
            ),
            cfg,
            base_url: "https://app.test".into(),
            alert_channel_stop_secret: String::new(),
            incident_ack_secret: String::new(),
            app_link_secret: String::new(),
            central_bot: None,
            pressed_apps: Vec::new(),
            central_whatsapp: None,
            email: Some(crate::notifier::EmailDelivery {
                sender: Arc::new(sender.clone()),
                from_address: "alerts@example.test".into(),
                from_name: "Uptimepage".into(),
            }),
        },
    );
    (engine, sender)
}

/// [`engine`] with a maintenance store the test controls.
fn engine_maint(
    ops: Arc<dyn IncidentOpsStore>,
    targets: Arc<dyn TargetStore>,
    channels: Arc<dyn NotificationChannelStore>,
    maintenance: Arc<dyn crate::storage::MaintenanceStore>,
) -> EscalationEngine {
    engine_maint_policies(
        ops,
        Arc::new(InMemoryEscalationPolicyStore::new()),
        targets,
        channels,
        maintenance,
    )
}

/// [`engine_maint`] with an escalation policy store the test controls.
fn engine_maint_policies(
    ops: Arc<dyn IncidentOpsStore>,
    policies: Arc<dyn EscalationPolicyStore>,
    targets: Arc<dyn TargetStore>,
    channels: Arc<dyn NotificationChannelStore>,
    maintenance: Arc<dyn crate::storage::MaintenanceStore>,
) -> EscalationEngine {
    let (_tx, rx) = mpsc::channel(4);
    EscalationEngine::new(
        rx,
        EngineDeps {
            ops,
            policies,
            on_call: Arc::new(InMemoryOnCallStore::new()),
            contacts: Arc::new(InMemoryContactStore::new()),
            linked_apps: Arc::new(crate::storage::InMemoryLinkedAppStore::new()),
            targets,
            channels,
            maintenance,
            orgs: Arc::new(crate::storage::orgs::InMemoryOrgDirectory::new()),
            http: crate::http_outbound::build_outbound_client(
                crate::security::SsrfGuard::relaxed_for_tests(),
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

fn engine_with(
    ops: Arc<dyn IncidentOpsStore>,
    policies: Arc<dyn EscalationPolicyStore>,
    on_call: Arc<dyn OnCallStore>,
    contacts: Arc<dyn ContactStore>,
    targets: Arc<dyn TargetStore>,
    channels: Arc<dyn NotificationChannelStore>,
) -> EscalationEngine {
    let (_tx, rx) = mpsc::channel(4);
    EscalationEngine::new(
        rx,
        EngineDeps {
            ops,
            policies,
            on_call,
            contacts,
            linked_apps: Arc::new(crate::storage::InMemoryLinkedAppStore::new()),
            targets,
            channels,
            maintenance: Arc::new(crate::storage::InMemoryMaintenanceStore::new()),
            orgs: Arc::new(crate::storage::orgs::InMemoryOrgDirectory::new()),
            http: crate::http_outbound::build_outbound_client(
                crate::security::SsrfGuard::relaxed_for_tests(),
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

#[tokio::test]
async fn no_policy_falls_back_to_bound_channels_once() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let policies = Arc::new(InMemoryEscalationPolicyStore::new());

    let eng = engine(ops.clone(), policies, targets, channels);
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    assert_eq!(ops.notifications_for(org(), id).await.unwrap().len(), 1);
    // A duplicate Opened signal does not re-page the same episode.
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    assert_eq!(ops.notifications_for(org(), id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn an_outstanding_page_reads_its_monitors_paused_flag() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let mut target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    let ack = |channel_id| crate::storage::EmergencyAck {
        id: Uuid::now_v7(),
        org: org(),
        incident_id: id,
        channel_id,
        receipt: "rcpt".into(),
        generation: 0,
    };

    let watched = Arc::new(InMemoryTargetStore::from_vec(vec![target.clone()]));
    let eng = engine(
        ops.clone(),
        Arc::new(InMemoryEscalationPolicyStore::new()),
        watched,
        channels.clone(),
    );
    assert!(!eng.page_is_spent(&ack(cid)).await);

    target.enabled = false;
    let paused = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let eng = engine(
        ops.clone(),
        Arc::new(InMemoryEscalationPolicyStore::new()),
        paused,
        channels,
    );
    assert!(eng.page_is_spent(&ack(cid)).await);
}

#[tokio::test]
async fn a_page_with_no_readable_monitor_keeps_polling() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let targetless = seed_incident(&ops, None);
    let missing_target = seed_incident(&ops, Some(Uuid::now_v7()));
    let eng = engine(
        ops.clone(),
        Arc::new(InMemoryEscalationPolicyStore::new()),
        Arc::new(InMemoryTargetStore::new()),
        channels,
    );

    let ack = |incident_id| crate::storage::EmergencyAck {
        id: Uuid::now_v7(),
        org: org(),
        incident_id,
        channel_id: cid,
        receipt: "rcpt".into(),
        generation: 0,
    };
    for incident_id in [targetless, missing_target] {
        assert!(!eng.page_is_spent(&ack(incident_id)).await);
    }
    // An incident that is not there at all is a different matter: nothing is
    // left to acknowledge, so the page should stop rather than sound on.
    assert!(eng.page_is_spent(&ack(Uuid::now_v7())).await);
}

#[tokio::test]
async fn a_paused_monitor_is_never_paged() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let mut target = target_with_channel(cid);
    target.enabled = false;
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    let eng = engine(
        ops.clone(),
        Arc::new(InMemoryEscalationPolicyStore::new()),
        Arc::new(InMemoryTargetStore::from_vec(vec![target])),
        channels,
    );

    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    eng.renotify_one(&DueIncident {
        id,
        org: org(),
        target_id: Some(tid),
        escalation_policy_id: None,
        escalation_level: 0,
        escalation_round: 0,
    })
    .await
    .unwrap();
    assert!(ops.notifications_for(org(), id).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_resolve_still_reaches_a_paused_monitors_channels() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let mut target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));

    let watched = engine(
        ops.clone(),
        Arc::new(InMemoryEscalationPolicyStore::new()),
        Arc::new(InMemoryTargetStore::from_vec(vec![target.clone()])),
        channels.clone(),
    );
    watched
        .page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    assert_eq!(ops.notifications_for(org(), id).await.unwrap().len(), 1);

    target.enabled = false;
    let paused = engine(
        ops.clone(),
        Arc::new(InMemoryEscalationPolicyStore::new()),
        Arc::new(InMemoryTargetStore::from_vec(vec![target])),
        channels,
    );
    paused
        .page(org(), id, NotificationReason::Resolved)
        .await
        .unwrap();
    assert_eq!(ops.notifications_for(org(), id).await.unwrap().len(), 2);
}

#[tokio::test]
async fn unverified_email_channel_records_failure_without_sending() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = channels
        .create(
            org(),
            NewNotificationChannel {
                name: "oncall-mail".into(),
                config: ChannelConfig::Email(crate::domain::EmailConfig {
                    to: "oncall@example.com".into(),
                }),
                enabled: true,
                auto_bind_tags: Vec::new(),
                acknowledge_button: true,
                resolve_button: false,
            },
            WriteSource::Ui,
            100,
            None,
        )
        .await
        .unwrap()
        .id;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let policies = Arc::new(InMemoryEscalationPolicyStore::new());

    let eng = engine(ops.clone(), policies, targets, channels.clone());
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    let rows = ops.notifications_for(org(), id).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, NotificationStatus::Failed);
    assert_eq!(rows[0].error.as_deref(), Some("email address not verified"));

    // Once verified, delivery is attempted for real (and fails on the
    // missing email sender — a different error than the gate's).
    let upd = channels.get(org(), cid).await.unwrap().unwrap().updated_at;
    assert!(channels.set_verified(org(), cid, upd).await.unwrap());
    let id2 = seed_incident(&ops, Some(tid));
    eng.page(org(), id2, NotificationReason::Opened)
        .await
        .unwrap();
    let rows = ops.notifications_for(org(), id2).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, NotificationStatus::Failed);
    assert_ne!(rows[0].error.as_deref(), Some("email address not verified"));
}

#[tokio::test]
async fn a_single_walk_ends_right_after_its_last_page() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let c1 = failing_channel(&channels).await;
    let target = bare_target();
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let policies = Arc::new(InMemoryEscalationPolicyStore::new());
    let p = policies
        .create(
            org(),
            NewEscalationPolicy {
                name: "p".into(),
                description: None,
                repeat_count: 0,
                steps: vec![channel_step(1, 300, c1)],
            },
            10,
        )
        .await
        .unwrap();
    policies
        .set_target_policy(org(), tid, Some(p.id))
        .await
        .unwrap();

    let eng = engine(ops.clone(), policies, targets, channels);
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    let rows = ops.notifications_for(org(), id).await.unwrap();
    assert_eq!(rows.len(), 1);
    let inc = ops.get(org(), id).await.unwrap().unwrap();
    assert_eq!(inc.escalation_level, 1);
    assert_eq!(inc.escalation_policy_id, Some(p.id));
    assert!(
        inc.next_escalation_at
            .is_some_and(|at| at <= chrono::Utc::now()),
        "a single walk's only level leaves nothing to wait out"
    );
}

#[tokio::test]
async fn sweep_walks_to_the_next_level_then_exhausts() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let c1 = failing_channel(&channels).await;
    let c2 = failing_channel(&channels).await;
    let target = bare_target();
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let policies = Arc::new(InMemoryEscalationPolicyStore::new());
    let p = policies
        .create(
            org(),
            NewEscalationPolicy {
                name: "p".into(),
                description: None,
                repeat_count: 0,
                steps: vec![channel_step(1, 0, c1), channel_step(2, 0, c2)],
            },
            10,
        )
        .await
        .unwrap();
    policies
        .set_target_policy(org(), tid, Some(p.id))
        .await
        .unwrap();

    let eng = engine(ops.clone(), policies, targets, channels);
    // Level 1 page + arm (delay 0 → immediately due).
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    assert_eq!(
        ops.get(org(), id).await.unwrap().unwrap().escalation_level,
        1
    );

    // Sweep escalates to level 2.
    eng.escalate_due().await;
    let inc = ops.get(org(), id).await.unwrap().unwrap();
    assert_eq!(inc.escalation_level, 2);
    assert_eq!(ops.notifications_for(org(), id).await.unwrap().len(), 2);

    // Next sweep: last level, no repeat → exhausted, timer disarmed.
    eng.escalate_due().await;
    let inc = ops.get(org(), id).await.unwrap().unwrap();
    assert!(inc.next_escalation_at.is_none());
    // No further pages once exhausted.
    eng.escalate_due().await;
    assert_eq!(ops.notifications_for(org(), id).await.unwrap().len(), 2);
}

#[tokio::test]
async fn acknowledge_halts_the_sweep() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let c1 = failing_channel(&channels).await;
    let c2 = failing_channel(&channels).await;
    let target = bare_target();
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let policies = Arc::new(InMemoryEscalationPolicyStore::new());
    let p = policies
        .create(
            org(),
            NewEscalationPolicy {
                name: "p".into(),
                description: None,
                repeat_count: 0,
                steps: vec![channel_step(1, 0, c1), channel_step(2, 0, c2)],
            },
            10,
        )
        .await
        .unwrap();
    policies
        .set_target_policy(org(), tid, Some(p.id))
        .await
        .unwrap();

    let eng = engine(ops.clone(), policies, targets, channels);
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    // A responder acks → next_escalation_at cleared, state acknowledged.
    ops.acknowledge(org(), id, Actor::System, None, None)
        .await
        .unwrap();
    eng.escalate_due().await;
    // Still only the level-1 page; the sweep found nothing due.
    assert_eq!(ops.notifications_for(org(), id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn resolution_notifies_paged_channels_and_dedups() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let policies = Arc::new(InMemoryEscalationPolicyStore::new());

    let eng = engine(ops.clone(), policies, targets, channels);
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    ops.resolve(org(), id, Actor::System, None).await.unwrap();
    eng.page(org(), id, NotificationReason::Resolved)
        .await
        .unwrap();
    assert_eq!(ops.notifications_for(org(), id).await.unwrap().len(), 2);
    // A duplicate Resolved signal is absorbed.
    eng.page(org(), id, NotificationReason::Resolved)
        .await
        .unwrap();
    assert_eq!(ops.notifications_for(org(), id).await.unwrap().len(), 2);
}

#[tokio::test]
async fn notify_recovery_false_suppresses_the_resolved_page() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel_recovery(cid, false);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let policies = Arc::new(InMemoryEscalationPolicyStore::new());

    let eng = engine(ops.clone(), policies, targets, channels);
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    ops.resolve(org(), id, Actor::System, None).await.unwrap();
    eng.page(org(), id, NotificationReason::Resolved)
        .await
        .unwrap();
    // Opened paged once; recovery opt-out blocked the resolution page.
    assert_eq!(ops.notifications_for(org(), id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn renotify_re_pages_an_open_unacked_incident_then_stops_once_acked() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let policies = Arc::new(InMemoryEscalationPolicyStore::new());

    let eng = engine(ops.clone(), policies, targets, channels);
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    assert_eq!(ops.notifications_for(org(), id).await.unwrap().len(), 1);

    let due = DueIncident {
        id,
        org: org(),
        target_id: Some(tid),
        escalation_policy_id: None,
        escalation_level: 0,
        escalation_round: 0,
    };
    eng.renotify_one(&due).await.unwrap();
    assert_eq!(
        ops.notifications_for(org(), id).await.unwrap().len(),
        2,
        "reminder re-pages the bound channel while down + unacked"
    );

    ops.acknowledge(org(), id, Actor::System, None, None)
        .await
        .unwrap();
    eng.renotify_one(&due).await.unwrap();
    assert_eq!(
        ops.notifications_for(org(), id).await.unwrap().len(),
        2,
        "an acknowledged incident is not reminded"
    );
}

#[tokio::test]
async fn a_reminder_is_logged_as_a_reminder_not_a_fresh_open() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));

    let eng = engine(
        ops.clone(),
        Arc::new(InMemoryEscalationPolicyStore::new()),
        targets,
        channels,
    );
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    eng.renotify_one(&DueIncident {
        id,
        org: org(),
        target_id: Some(tid),
        escalation_policy_id: None,
        escalation_level: 0,
        escalation_round: 0,
    })
    .await
    .unwrap();

    let rows = ops.notifications_for(org(), id).await.unwrap();
    let reasons: Vec<_> = rows.iter().map(|r| r.reason).collect();
    assert_eq!(
        reasons,
        vec![NotificationReason::Opened, NotificationReason::Reminder]
    );
    assert_eq!(ops.renotify_count(id), 1);
}

#[tokio::test]
async fn a_reminder_that_reaches_nobody_does_not_widen_the_backoff() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let due = DueIncident {
        id,
        org: org(),
        target_id: Some(tid),
        escalation_policy_id: None,
        escalation_level: 0,
        escalation_round: 0,
    };

    engine(
        ops.clone(),
        Arc::new(InMemoryEscalationPolicyStore::new()),
        targets.clone(),
        channels,
    )
    .page(org(), id, NotificationReason::Opened)
    .await
    .unwrap();

    // The channel is gone by the time the reminder is due, so it pages nothing.
    engine(
        ops.clone(),
        Arc::new(InMemoryEscalationPolicyStore::new()),
        targets,
        Arc::new(InMemoryNotificationChannelStore::new()),
    )
    .renotify_one(&due)
    .await
    .unwrap();

    assert_eq!(ops.notifications_for(org(), id).await.unwrap().len(), 1);
    assert_eq!(ops.renotify_count(id), 0);
}

async fn window_over(
    target_id: uuid::Uuid,
    suppress_alerts: bool,
) -> Arc<crate::storage::InMemoryMaintenanceStore> {
    use crate::domain::{NewMaintenanceWindow, WriteSource};
    use crate::storage::MaintenanceStore;
    let store = Arc::new(crate::storage::InMemoryMaintenanceStore::new());
    store
        .create(
            org(),
            NewMaintenanceWindow {
                title: "Upgrade".into(),
                description: None,
                starts_at: chrono::Utc::now() - chrono::Duration::minutes(5),
                ends_at: chrono::Utc::now() + chrono::Duration::hours(1),
                component_ids: vec![target_id],
                suppress_alerts,
            },
            WriteSource::Ui,
            None,
        )
        .await
        .unwrap();
    store
}

#[tokio::test]
async fn a_monitor_in_maintenance_opens_its_incident_but_pages_nobody() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));

    let eng = engine_maint(ops.clone(), targets, channels, window_over(tid, true).await);
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();

    let rows = ops.notifications_for(org(), id).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].channel_id.is_none() && rows[0].transport == "maintenance",
        "a hold records a marker, never a delivery: {rows:?}"
    );
    // The incident is still on the books, and says why it went unanswered.
    assert!(ops.get(org(), id).await.unwrap().is_some());
    let events = ops.timeline(org(), id).await.unwrap();
    assert!(
        events.iter().any(|e| e
            .message
            .as_deref()
            .is_some_and(|m| m.contains("maintenance window"))),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_held_alert_pages_once_the_window_is_over() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));

    let eng = engine_maint(
        ops.clone(),
        targets.clone(),
        channels.clone(),
        window_over(tid, true).await,
    );
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    assert_eq!(ops.notifications_for(org(), id).await.unwrap().len(), 1);

    // Window gone: the release pages the channel the hold never reached.
    let eng = engine_maint(
        ops.clone(),
        targets,
        channels,
        Arc::new(crate::storage::InMemoryMaintenanceStore::new()),
    );
    eng.release_maintenance().await;

    let rows = ops.notifications_for(org(), id).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter().any(|n| n.channel_id == Some(cid)),
        "the release reaches a real channel: {rows:?}"
    );

    // The marker is spent: a second sweep must not page the same hold again.
    eng.release_maintenance().await;
    assert_eq!(ops.notifications_for(org(), id).await.unwrap().len(), 2);
}

#[tokio::test]
async fn maintenance_parks_the_escalation_timer_without_losing_the_ladder() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let c1 = failing_channel(&channels).await;
    let c2 = failing_channel(&channels).await;
    let target = bare_target();
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let policies = Arc::new(InMemoryEscalationPolicyStore::new());
    let p = policies
        .create(
            org(),
            NewEscalationPolicy {
                name: "p".into(),
                description: None,
                repeat_count: 0,
                steps: vec![channel_step(1, 0, c1), channel_step(2, 0, c2)],
            },
            10,
        )
        .await
        .unwrap();
    policies
        .set_target_policy(org(), tid, Some(p.id))
        .await
        .unwrap();

    let quiet = Arc::new(crate::storage::InMemoryMaintenanceStore::new());
    let eng = engine_maint_policies(
        ops.clone(),
        policies.clone(),
        targets.clone(),
        channels.clone(),
        quiet,
    );
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    assert!(
        ops.get(org(), id)
            .await
            .unwrap()
            .unwrap()
            .next_escalation_at
            .is_some()
    );

    let eng = engine_maint_policies(
        ops.clone(),
        policies,
        targets,
        channels,
        window_over(tid, true).await,
    );
    let before = ops.notifications_for(org(), id).await.unwrap().len();
    eng.escalate_due().await;

    let inc = ops.get(org(), id).await.unwrap().unwrap();
    assert_eq!(
        ops.notifications_for(org(), id).await.unwrap().len(),
        before
    );
    assert!(
        inc.next_escalation_at
            .is_some_and(|at| at > chrono::Utc::now()),
        "clearing the timer would strand the rest of the ladder: only a fresh \
         open re-arms one"
    );
    assert_eq!(inc.escalation_level, 1, "the rung it reached is kept");
}

#[tokio::test]
async fn a_window_never_silences_an_incident_someone_declared_by_hand() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    ops.edit(id, |i| i.origin = crate::domain::IncidentOrigin::Manual);
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));

    let eng = engine_maint(ops.clone(), targets, channels, window_over(tid, true).await);
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();

    let rows = ops.notifications_for(org(), id).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].channel_id,
        Some(cid),
        "the operator declared this during the window on purpose: {rows:?}"
    );
}

#[tokio::test]
async fn a_public_only_window_leaves_alerting_alone() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));

    let eng = engine_maint(
        ops.clone(),
        targets,
        channels,
        window_over(tid, false).await,
    );
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();

    assert_eq!(ops.notifications_for(org(), id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn maintenance_holds_a_reminder_without_discarding_the_backoff() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let due = DueIncident {
        id,
        org: org(),
        target_id: Some(tid),
        escalation_policy_id: None,
        escalation_level: 0,
        escalation_round: 0,
    };

    let quiet = Arc::new(crate::storage::InMemoryMaintenanceStore::new());
    let eng = engine_maint(ops.clone(), targets.clone(), channels.clone(), quiet);
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    eng.renotify_one(&due).await.unwrap();
    eng.renotify_one(&due).await.unwrap();
    assert_eq!(ops.renotify_count(id), 2);

    let eng = engine_maint(ops.clone(), targets, channels, window_over(tid, true).await);
    eng.renotify_one(&due).await.unwrap();

    assert_eq!(
        ops.notifications_for(org(), id).await.unwrap().len(),
        3,
        "the window holds the reminder"
    );
    assert_eq!(
        ops.renotify_count(id),
        2,
        "and leaves the backoff alone: this incident was already paged, so a \
         window is not a reason to restart its ladder"
    );
}

#[tokio::test]
async fn schedule_target_pages_the_on_call_responders_contact_channel() {
    use crate::domain::{
        NewOnCallLayer, NewOnCallParticipant, NewOnCallSchedule, RotationType, UserId,
    };

    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let personal = failing_channel(&channels).await;
    let responder = UserId(Uuid::now_v7());

    // On-call schedule with the responder, and their personal contact channel.
    let on_call = Arc::new(InMemoryOnCallStore::new());
    on_call.add_member(org(), responder);
    let sched = on_call
        .create(
            org(),
            NewOnCallSchedule {
                name: "primary".into(),
                timezone: "UTC".into(),
                layers: vec![NewOnCallLayer {
                    name: None,
                    rotation_type: RotationType::Daily,
                    rotation_length_secs: 86_400,
                    handoff_at: "2020-01-01T00:00:00Z".parse().unwrap(),
                    layer_order: 0,
                    windows: vec![],
                    participants: vec![NewOnCallParticipant { user_id: responder }],
                }],
            },
            10,
        )
        .await
        .unwrap();
    let contacts = Arc::new(InMemoryContactStore::new());
    contacts.add_channel(org(), personal);
    contacts
        .replace_for_user(org(), responder, vec![personal])
        .await
        .unwrap();

    // A policy whose only rung is a schedule target.
    let policies = Arc::new(InMemoryEscalationPolicyStore::new());
    let p = policies
        .create(
            org(),
            crate::domain::NewEscalationPolicy {
                name: "p".into(),
                description: None,
                repeat_count: 0,
                steps: vec![NewEscalationStep {
                    level: 1,
                    delay_secs: 300,
                    targets: vec![NewEscalationTarget {
                        target_type: EscalationTargetType::Schedule,
                        user_id: None,
                        schedule_id: Some(sched.schedule.id),
                        channel_id: None,
                    }],
                }],
            },
            10,
        )
        .await
        .unwrap();
    let target = bare_target();
    let tid = target.id;
    policies
        .set_target_policy(org(), tid, Some(p.id))
        .await
        .unwrap();
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));

    let eng = engine_with(ops.clone(), policies, on_call, contacts, targets, channels);
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    let rows = ops.notifications_for(org(), id).await.unwrap();
    assert_eq!(
        rows.len(),
        1,
        "the on-call responder's contact channel was paged"
    );
    assert_eq!(rows[0].channel_id, Some(personal));
    assert_eq!(rows[0].target_user_id, Some(responder));
}

#[tokio::test]
async fn reconcile_pages_an_incident_whose_open_signal_was_dropped() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    // A triggered incident older than the grace window, never paged (its
    // Opened signal was dropped): no notifications, no policy, no timer.
    let now = Utc::now();
    let id = Uuid::now_v7();
    ops.seed(OpsIncident {
        id,
        target_id: Some(tid),
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
        started_at: now - chrono::Duration::seconds(120),
        ended_at: None,
        acknowledged_at: None,
        acknowledged_by: None,
        assigned_to: None,
        resolved_by: None,
        escalation_policy_id: None,
        escalation_level: 0,
        escalation_round: 0,
        next_escalation_at: None,
        check_count: 2,
        error_sample: None,
        regions_down: Vec::new(),
        regions_up: Vec::new(),
        created_at: now - chrono::Duration::seconds(120),
        updated_at: now - chrono::Duration::seconds(120),
    });
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let policies = Arc::new(InMemoryEscalationPolicyStore::new());

    let eng = engine(ops.clone(), policies, targets, channels);
    assert!(ops.notifications_for(org(), id).await.unwrap().is_empty());
    eng.reconcile().await;
    assert_eq!(
        ops.notifications_for(org(), id).await.unwrap().len(),
        1,
        "reconcile re-pages the never-paged incident"
    );
    // Now that it has been paged, reconcile no longer picks it up.
    eng.reconcile().await;
    assert_eq!(ops.notifications_for(org(), id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn reconcile_leaves_an_incident_older_than_its_window_alone() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let target = target_with_channel(Uuid::now_v7());
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let now = Utc::now();
    let id = Uuid::now_v7();
    let old = now - chrono::Duration::days(21);
    ops.seed(OpsIncident {
        id,
        target_id: Some(tid),
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
        started_at: old,
        ended_at: None,
        acknowledged_at: None,
        acknowledged_by: None,
        assigned_to: None,
        resolved_by: None,
        escalation_policy_id: None,
        escalation_level: 0,
        escalation_round: 0,
        next_escalation_at: None,
        check_count: 2,
        error_sample: None,
        regions_down: Vec::new(),
        regions_up: Vec::new(),
        created_at: old,
        updated_at: old,
    });
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let policies = Arc::new(InMemoryEscalationPolicyStore::new());

    let eng = engine(ops.clone(), policies, targets, channels);
    eng.reconcile().await;
    assert!(
        ops.notifications_for(org(), id).await.unwrap().is_empty(),
        "an incident past the reconcile window is not re-attempted"
    );
}

#[test]
fn retry_backoff_doubles_then_dead_letters() {
    // base 30s, cap 1h, 5 attempts: 30, 60, 120, 240, then None (exhausted).
    assert_eq!(retry_delay_secs(1, 30, 3600, 5), Some(30));
    assert_eq!(retry_delay_secs(2, 30, 3600, 5), Some(60));
    assert_eq!(retry_delay_secs(3, 30, 3600, 5), Some(120));
    assert_eq!(retry_delay_secs(4, 30, 3600, 5), Some(240));
    assert_eq!(
        retry_delay_secs(5, 30, 3600, 5),
        None,
        "attempt cap → dead-letter"
    );
    // The doubling is bounded by the configured cap.
    assert_eq!(retry_delay_secs(10, 30, 3600, 100), Some(3600));
    assert_eq!(retry_delay_secs(4, 30, 90, 100), Some(90), "cap bites");
}

#[test]
fn retry_after_hint_reads_telegram_429_body() {
    let body = r#"sending request failed: 429 Too Many Requests: {"ok":false,"error_code":429,"description":"Too Many Requests: retry after 31","parameters":{"retry_after":31}}"#;
    assert_eq!(
        retry_after_hint(Some(body)),
        Some(chrono::Duration::seconds(31))
    );
    assert_eq!(retry_after_hint(Some("connection refused")), None);
    assert_eq!(retry_after_hint(None), None);
    // A hostile/buggy body can't park the retry for days.
    assert_eq!(
        retry_after_hint(Some(r#"{"retry_after":9999999}"#)),
        Some(chrono::Duration::seconds(3600))
    );
    assert_eq!(retry_after_hint(Some(r#"{"retry_after":"x"}"#)), None);
}

/// A fraction that failed to parse would take the deferred classification with
/// it and log a rate limit as a broken transport.
#[test]
fn retry_after_hint_reads_discords_fractional_wait() {
    let body = r#"endpoint returned 429 Too Many Requests: {"message": "You are being rate limited.", "retry_after": 0.671, "global": false}"#;
    assert_eq!(
        retry_after_hint(Some(body)),
        Some(chrono::Duration::zero()),
        "sub-second floors to zero; the retry backoff supplies the real wait"
    );
    assert_eq!(
        retry_after_hint(Some(r#"{"retry_after":5}"#)),
        Some(chrono::Duration::seconds(5))
    );
    // A tenant-controlled body must not overflow the worker's arithmetic.
    assert_eq!(
        retry_after_hint(Some(r#"{"retry_after":9223372036854775807.5}"#)),
        Some(chrono::Duration::seconds(3600))
    );
}

#[test]
fn retry_after_hint_survives_redaction() {
    // The hint is parsed AFTER redact_url_paths; this pins that the
    // URL-token redaction never eats the JSON fragment.
    let raw = r#"https://api.telegram.org/bot123:SECRET/sendMessage returned 429 Too Many Requests: {"ok":false,"error_code":429,"parameters":{"retry_after":31}}"#;
    let redacted = redact_url_paths(raw);
    assert_eq!(
        retry_after_hint(Some(&redacted)),
        Some(chrono::Duration::seconds(31))
    );
}

#[test]
fn redact_secrets_strips_channel_url_paths() {
    let slack = "POST https://hooks.slack.com/services/T01/B02/abcSECRETxyz failed: 404";
    let out = redact_url_paths(slack);
    assert!(out.contains("https://hooks.slack.com"));
    assert!(
        !out.contains("abcSECRETxyz"),
        "the webhook secret must not survive"
    );
    assert!(!out.contains("/services/"));

    let tg = "https://api.telegram.org/bot123456:AAH-SECRET-TOKEN/sendMessage 401";
    let out = redact_url_paths(tg);
    assert!(out.contains("https://api.telegram.org"));
    assert!(
        !out.contains("SECRET-TOKEN"),
        "the bot token must not survive"
    );

    // Non-URL text is untouched.
    assert_eq!(redact_url_paths("connection refused"), "connection refused");

    // A "://"-bearing token that does not cleanly parse is dropped wholesale
    // rather than echoed (it might still carry the secret path).
    let bad = redact_url_paths("weird://[bad/SECRET-path");
    assert!(
        !bad.contains("SECRET-path"),
        "an unparseable url token must not survive"
    );
    assert!(bad.contains("[redacted-url]"));
}

#[test]
fn log_error_snippet_masks_addresses_and_clips() {
    let out = log_error_snippet("delivery rejected for <oncall@example.com>: mailbox full");
    assert!(!out.contains("oncall@example.com"));
    assert!(out.contains("[redacted-address]"));
    assert!(out.contains("mailbox full"));

    // A telegram-style @handle has no local part and stays readable.
    assert_eq!(
        log_error_snippet("chat @ops_team not found"),
        "chat @ops_team not found"
    );

    let big = format!("endpoint returned 503: {}", "x".repeat(10_000));
    let out = log_error_snippet(&big);
    assert!(
        out.chars().count() <= 257,
        "clipped to the cap plus ellipsis"
    );
    assert!(out.ends_with('…'));
}

#[tokio::test]
async fn manual_incident_without_monitor_pages_nothing() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, None);
    let targets = Arc::new(InMemoryTargetStore::new());
    let policies = Arc::new(InMemoryEscalationPolicyStore::new());
    let eng = engine(ops.clone(), policies, targets, channels);
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    assert!(ops.notifications_for(org(), id).await.unwrap().is_empty());
}

#[tokio::test]
async fn retry_sweep_increments_attempts_then_exhausts() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let policies = Arc::new(InMemoryEscalationPolicyStore::new());

    let eng = engine(ops.clone(), policies, targets, channels);
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    for _ in 0..10 {
        // Each round simulates the exponential backoff having elapsed.
        ops.clear_retry_backoff();
        eng.retry_pending().await;
    }
    let rows = ops.notifications_for(org(), id).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].attempt,
        EscalationConfig::default().max_attempts as i32
    );
    assert_eq!(rows[0].status, NotificationStatus::Failed);
}

/// A page that runs out of retries is one strike. Enough in a row and the
/// endpoint is dead, not busy, and the whole remedy is that somebody is told.
#[tokio::test]
async fn a_channel_that_stops_delivering_mails_the_owners_once() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let cfg = EscalationConfig {
        channel_failure_limit: 2,
        ..Default::default()
    };
    let max_attempts = cfg.max_attempts;
    let (eng, mail) = engine_mailing(ops.clone(), targets, channels.clone(), cfg);

    // One incident short of the limit, the one that crosses it, and one more
    // that must not mail again.
    for round in 1..=3 {
        let id = seed_incident(&ops, Some(tid));
        eng.page(org(), id, NotificationReason::Opened)
            .await
            .unwrap();
        for _ in 0..max_attempts {
            ops.clear_retry_backoff();
            eng.retry_pending().await;
        }
        settle().await;
        let live = channels.get(org(), cid).await.unwrap().unwrap();
        assert!(live.enabled, "round {round}: the channel stays on the air");
        assert_eq!(
            mail.len(),
            usize::from(round >= 2),
            "round {round}: one mail per run of failures, not per incident"
        );
    }

    let flagged = channels.get(org(), cid).await.unwrap().unwrap();
    assert!(flagged.is_failing(2));
    assert!(flagged.failing_since.is_some());
    assert!(
        flagged.disabled_reason.is_none(),
        "nothing disabled it, so it owes no disable note"
    );

    settle().await;
    let sent = mail.sent();
    let to = &sent[0].to;
    assert_eq!(to.address, "owner@example.test");
    let rendered = sent[0].template.render("Uptimepage");
    assert!(rendered.subject.contains(&flagged.name));
    assert!(
        rendered.text_body.contains("Acme Inc"),
        "mail should attribute the org: {}",
        rendered.text_body
    );
    // Without the transport's own error the owner cannot tell what broke.
    assert!(
        rendered.text_body.contains("What the endpoint returned"),
        "mail should carry the transport error: {}",
        rendered.text_body
    );
    assert!(
        rendered.text_body.contains("still being sent"),
        "the owner must not read this as the channel having been turned off"
    );
}

/// The owner mail is spawned off the paging path, so a test has to hand the
/// runtime enough turns for it to finish before counting what was sent.
async fn settle() {
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
}

/// Answers 200 to anything, so a delivery through the engine really lands.
async fn spawn_ok_endpoint() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut scratch = [0u8; 4096];
                let _ = sock.read(&mut scratch).await;
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                    .await;
            });
        }
    });
    format!("http://{addr}/notify")
}

async fn page_and_exhaust(
    eng: &EscalationEngine,
    ops: &Arc<InMemoryIncidentOpsStore>,
    tid: Uuid,
    max_attempts: u32,
) {
    let id = seed_incident(ops, Some(tid));
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    for _ in 0..max_attempts {
        ops.clear_retry_backoff();
        eng.retry_pending().await;
    }
}

/// A send that lands restarts the run, so the next outage is its own.
#[tokio::test]
async fn a_delivery_that_lands_re_arms_the_alert() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let cfg = EscalationConfig {
        channel_failure_limit: 1,
        ..Default::default()
    };
    let max_attempts = cfg.max_attempts;
    let (eng, mail) = engine_mailing(ops.clone(), targets, channels.clone(), cfg);

    page_and_exhaust(&eng, &ops, tid, max_attempts).await;
    settle().await;
    assert_eq!(mail.len(), 1, "a dead endpoint is reported once");

    // The engine has to notice the delivery landed, not just that it was tried.
    let url = spawn_ok_endpoint().await;
    channels
        .update(
            org(),
            cid,
            NotificationChannelUpdate {
                config: Some(ChannelConfig::Webhook(WebhookConfig {
                    url,
                    headers: Default::default(),
                    secret: None,
                })),
                ..Default::default()
            },
            WriteSource::Ui,
            None,
        )
        .await
        .unwrap();
    let id = seed_incident(&ops, Some(tid));
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    settle().await;

    let recovered = channels.get(org(), cid).await.unwrap().unwrap();
    assert_eq!(recovered.consecutive_failures, 0);
    assert_eq!(recovered.failing_since, None);
    assert!(!recovered.is_failing(1));
    assert_eq!(mail.len(), 1, "recovery is not itself news");
    // The run restarts, so the next outage is a run of its own. It owes no
    // second mail yet: the cooldown is what stops a flapping endpoint from
    // mailing the owners on every cycle.
    channels
        .record_delivery_outcome(org(), cid, false)
        .await
        .unwrap();
    let again = channels.get(org(), cid).await.unwrap().unwrap();
    assert!(again.is_failing(1));
    assert!(
        channels
            .claim_failure_alert(org(), cid)
            .await
            .unwrap()
            .is_none(),
        "a run reported minutes ago does not report again"
    );
}

/// Claiming before sending is only safe if a claim that cannot be sent comes
/// back; otherwise nobody is ever told until the cooldown expires.
#[tokio::test]
async fn a_claim_that_cannot_be_mailed_is_handed_back() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let cfg = EscalationConfig {
        channel_failure_limit: 1,
        ..Default::default()
    };
    let max_attempts = cfg.max_attempts;
    let (eng, mail) = engine_mailing_owned(ops.clone(), targets, channels.clone(), cfg, false);

    page_and_exhaust(&eng, &ops, tid, max_attempts).await;
    settle().await;
    assert_eq!(mail.len(), 0, "there was nobody to mail");
    // A claim was taken and handed back, so the run is unreported and the next
    // caller can take it. Without the release this second claim would be None.
    let live = channels.get(org(), cid).await.unwrap().unwrap();
    assert!(live.is_failing(1), "the run itself is still open");
    assert!(
        channels
            .claim_failure_alert(org(), cid)
            .await
            .unwrap()
            .is_some(),
        "the unsent alert is still owed"
    );
}

/// A channel turned off mid-outage stops receiving the retries already queued
/// for it, rather than draining them into a destination nobody is watching.
#[tokio::test]
async fn disabling_a_channel_suppresses_its_queued_retries() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let eng = engine_cfg(
        ops.clone(),
        Arc::new(InMemoryEscalationPolicyStore::new()),
        targets,
        channels.clone(),
        EscalationConfig::default(),
    );

    let id = seed_incident(&ops, Some(tid));
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    channels
        .update(
            org(),
            cid,
            NotificationChannelUpdate {
                enabled: Some(false),
                ..Default::default()
            },
            WriteSource::Ui,
            None,
        )
        .await
        .unwrap();
    ops.clear_retry_backoff();
    eng.retry_pending().await;

    let rows = ops.notifications_for(org(), id).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, NotificationStatus::Suppressed);
    assert!(
        rows[0]
            .error
            .as_deref()
            .is_some_and(|e| e.contains("turned off")),
        "expected the disabled-channel reason, got {:?}",
        rows[0].error
    );
}

/// An unverified address fails every delivery by design, and its own badge
/// already says so.
#[tokio::test]
async fn an_unverified_email_channel_is_not_flagged_as_failing() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = channels
        .create(
            org(),
            NewNotificationChannel {
                name: "owner mailbox".into(),
                config: ChannelConfig::Email(EmailConfig {
                    to: "unconfirmed@example.test".into(),
                }),
                enabled: true,
                auto_bind_tags: Vec::new(),
                acknowledge_button: true,
                resolve_button: false,
            },
            WriteSource::Ui,
            100,
            None,
        )
        .await
        .unwrap()
        .id;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let cfg = EscalationConfig {
        channel_failure_limit: 1,
        ..Default::default()
    };
    let max_attempts = cfg.max_attempts;
    let (eng, mail) = engine_mailing(ops.clone(), targets, channels.clone(), cfg);

    for _ in 0..3 {
        page_and_exhaust(&eng, &ops, tid, max_attempts).await;
    }

    settle().await;
    let live = channels.get(org(), cid).await.unwrap().unwrap();
    assert_eq!(live.consecutive_failures, 0);
    assert!(!live.is_failing(1));
    assert_eq!(mail.len(), 0, "the unverified badge is the whole message");
}

#[tokio::test]
async fn retry_drops_a_page_whose_reason_no_longer_matches_state() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let policies = Arc::new(InMemoryEscalationPolicyStore::new());

    let eng = engine(ops.clone(), policies, targets, channels);
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    ops.resolve(org(), id, Actor::System, None).await.unwrap();
    ops.clear_retry_backoff();
    eng.retry_pending().await;
    let rows = ops.notifications_for(org(), id).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, NotificationStatus::Suppressed);
    assert!(
        rows[0]
            .error
            .as_deref()
            .is_some_and(|e| e.contains("state moved on")),
        "expected the stale-reason cause, not the disabled-channel one, got {:?}",
        rows[0].error
    );
}

#[test]
fn flap_damping_engages_on_the_crossing_open_and_holds_the_rest() {
    assert_eq!(flap_state(1, 3), FlapState::Steady);
    assert_eq!(flap_state(2, 3), FlapState::Steady);
    // The open that crosses still pages, so the flapping is visible.
    assert_eq!(flap_state(3, 3), FlapState::Crossing);
    assert_eq!(flap_state(4, 3), FlapState::Damped);
    assert_eq!(flap_state(900, 3), FlapState::Damped);
}

#[test]
fn a_zero_threshold_delivers_every_open() {
    assert_eq!(flap_state(1, 0), FlapState::Steady);
    assert_eq!(flap_state(10_000, 0), FlapState::Steady);
}

/// The flood this exists to stop.
#[tokio::test]
async fn a_flapping_monitor_stops_paging_once_it_crosses_the_threshold() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let policies = Arc::new(InMemoryEscalationPolicyStore::new());
    let cfg = EscalationConfig {
        flap_max_opens: 3,
        ..Default::default()
    };
    let eng = engine_cfg(ops.clone(), policies, targets, channels, cfg);

    let mut ids = Vec::new();
    for _ in 0..5 {
        let id = seed_incident(&ops, Some(tid));
        eng.page(org(), id, NotificationReason::Opened)
            .await
            .unwrap();
        ids.push(id);
    }

    // The first three reached the channel; the last two were held.
    for id in &ids[..3] {
        let rows = ops.notifications_for(org(), *id).await.unwrap();
        assert_eq!(rows.len(), 1, "expected a delivery attempt");
        assert_eq!(rows[0].channel_id, Some(cid));
    }
    for id in &ids[3..] {
        let rows = ops.notifications_for(org(), *id).await.unwrap();
        assert_eq!(rows.len(), 1, "the held open is still recorded");
        assert_eq!(rows[0].status, NotificationStatus::Suppressed);
        assert_eq!(rows[0].channel_id, None, "held opens reach no channel");
        assert_eq!(rows[0].transport, "damped");
    }
}

/// What makes holding safe: no further incident can open while the held one
/// is, so a hold that never released would page nobody for a real outage.
#[tokio::test]
async fn a_held_alert_still_pages_when_the_incident_stays_open() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let policies = Arc::new(InMemoryEscalationPolicyStore::new());
    let cfg = EscalationConfig {
        flap_max_opens: 1,
        flap_hold_secs: 600,
        ..Default::default()
    };
    let eng = engine_cfg(ops.clone(), policies, targets, channels, cfg);

    let crossing = seed_incident(&ops, Some(tid));
    eng.page(org(), crossing, NotificationReason::Opened)
        .await
        .unwrap();
    let held = seed_incident(&ops, Some(tid));
    eng.page(org(), held, NotificationReason::Opened)
        .await
        .unwrap();
    assert_eq!(
        ops.notifications_for(org(), held).await.unwrap()[0].channel_id,
        None,
        "the second open is held"
    );

    // Nothing is due while the hold is running.
    eng.release_held().await;
    assert_eq!(ops.notifications_for(org(), held).await.unwrap().len(), 1);

    // The endpoint never recovered: age the hold past its window.
    ops.age_held_rows(chrono::Duration::seconds(900));
    eng.release_held().await;

    let rows = ops.notifications_for(org(), held).await.unwrap();
    assert!(
        rows.iter().any(|n| n.channel_id == Some(cid)),
        "a still-open incident must reach the channel once the hold expires"
    );
}

/// A flapping monitor recovers between the release scan and the page, and its
/// all-clear already reached nobody — so paging then announces a dead outage.
#[tokio::test]
async fn a_hold_that_recovered_before_release_pages_nobody() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let policies = Arc::new(InMemoryEscalationPolicyStore::new());
    let cfg = EscalationConfig {
        flap_max_opens: 1,
        ..Default::default()
    };
    let eng = engine_cfg(ops.clone(), policies, targets, channels, cfg);

    eng.page(
        org(),
        seed_incident(&ops, Some(tid)),
        NotificationReason::Opened,
    )
    .await
    .unwrap();
    let held = seed_incident(&ops, Some(tid));
    eng.page(org(), held, NotificationReason::Opened)
        .await
        .unwrap();
    ops.age_held_rows(chrono::Duration::seconds(900));

    // The window the scan's own state filter cannot cover.
    ops.resolve(org(), held, Actor::System, None).await.unwrap();
    eng.release_page(org(), held).await.unwrap();

    let rows = ops.notifications_for(org(), held).await.unwrap();
    assert!(
        rows.iter().all(|n| n.channel_id.is_none()),
        "a resolved incident must not page an outage after the fact"
    );
}

/// A release reaching no channel records nothing, so without its own marker
/// the incident would match the scan again every tick, forever.
#[tokio::test]
async fn releasing_a_hold_that_reaches_no_channel_happens_once() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let target = target_with_channel(Uuid::now_v7()); // bound channel does not exist
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let policies = Arc::new(InMemoryEscalationPolicyStore::new());
    let cfg = EscalationConfig {
        flap_max_opens: 1,
        ..Default::default()
    };
    let eng = engine_cfg(ops.clone(), policies, targets, channels, cfg);

    eng.page(
        org(),
        seed_incident(&ops, Some(tid)),
        NotificationReason::Opened,
    )
    .await
    .unwrap();
    let held = seed_incident(&ops, Some(tid));
    eng.page(org(), held, NotificationReason::Opened)
        .await
        .unwrap();
    ops.age_held_rows(chrono::Duration::seconds(900));

    eng.release_held().await;
    let after_first = ops.notifications_for(org(), held).await.unwrap().len();
    eng.release_held().await;
    eng.release_held().await;
    assert_eq!(
        ops.notifications_for(org(), held).await.unwrap().len(),
        after_first,
        "a release that reaches no channel must not repeat every tick"
    );
}

/// Turning damping off must not strand what is already held.
#[tokio::test]
async fn a_zero_threshold_still_releases_existing_holds() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let policies = Arc::new(InMemoryEscalationPolicyStore::new());
    let damping_on = EscalationConfig {
        flap_max_opens: 1,
        ..Default::default()
    };
    let eng = engine_cfg(
        ops.clone(),
        policies.clone(),
        targets.clone(),
        channels.clone(),
        damping_on,
    );
    eng.page(
        org(),
        seed_incident(&ops, Some(tid)),
        NotificationReason::Opened,
    )
    .await
    .unwrap();
    let held = seed_incident(&ops, Some(tid));
    eng.page(org(), held, NotificationReason::Opened)
        .await
        .unwrap();
    ops.age_held_rows(chrono::Duration::seconds(900));

    // Operator switches damping off while an alert is still held.
    let damping_off = EscalationConfig {
        flap_max_opens: 0,
        ..Default::default()
    };
    let eng = engine_cfg(ops.clone(), policies, targets, channels, damping_off);
    eng.release_held().await;

    let rows = ops.notifications_for(org(), held).await.unwrap();
    assert!(
        rows.iter().any(|n| n.channel_id == Some(cid)),
        "an already-held alert must still be released"
    );
}

/// A held open must not produce an all-clear, or damping would halve the
/// flood rather than stop it.
#[tokio::test]
async fn a_held_open_sends_no_recovery() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let policies = Arc::new(InMemoryEscalationPolicyStore::new());
    let cfg = EscalationConfig {
        flap_max_opens: 1,
        ..Default::default()
    };
    let eng = engine_cfg(ops.clone(), policies, targets, channels, cfg);

    let crossing = seed_incident(&ops, Some(tid));
    eng.page(org(), crossing, NotificationReason::Opened)
        .await
        .unwrap();
    let held = seed_incident(&ops, Some(tid));
    eng.page(org(), held, NotificationReason::Opened)
        .await
        .unwrap();

    ops.resolve(org(), held, Actor::System, None).await.unwrap();
    eng.page(org(), held, NotificationReason::Resolved)
        .await
        .unwrap();

    let rows = ops.notifications_for(org(), held).await.unwrap();
    assert_eq!(
        rows.len(),
        1,
        "no recovery page for an outage nobody heard about"
    );
    assert_eq!(rows[0].reason, NotificationReason::Opened);
}

/// A failed delivery still wrote a row the retry sweep owns, so it is not an
/// unreachable episode — recording a second marker for it would double-count
/// and hide the retry.
#[tokio::test]
async fn a_failed_delivery_is_not_marked_unreachable() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let policies = Arc::new(InMemoryEscalationPolicyStore::new());
    let eng = engine(ops.clone(), policies, targets, channels);

    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();

    let rows = ops.notifications_for(org(), id).await.unwrap();
    assert_eq!(rows.len(), 1, "one row for the one channel it tried");
    assert_eq!(rows[0].channel_id, Some(cid));
}

/// A monitor whose bindings reach nothing records a marker, or the reconcile
/// scan re-runs the episode every tick for the whole window.
#[tokio::test]
async fn an_episode_that_reaches_no_channel_is_marked() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let target = target_with_channel(Uuid::now_v7()); // bound channel does not exist
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let policies = Arc::new(InMemoryEscalationPolicyStore::new());
    let eng = engine(ops.clone(), policies, targets, channels);

    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();

    let rows = ops.notifications_for(org(), id).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].transport, "unreachable");
    assert_eq!(rows[0].channel_id, None);
}

/// The operator declared this one deliberately; the damper must not hold it.
#[tokio::test]
async fn a_manually_declared_incident_is_never_held() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let policies = Arc::new(InMemoryEscalationPolicyStore::new());
    let cfg = EscalationConfig {
        flap_max_opens: 1,
        ..Default::default()
    };
    let eng = engine_cfg(ops.clone(), policies, targets, channels, cfg);

    eng.page(
        org(),
        seed_incident(&ops, Some(tid)),
        NotificationReason::Opened,
    )
    .await
    .unwrap();
    let manual = seed_incident(&ops, Some(tid));
    ops.edit(manual, |i| i.origin = crate::domain::IncidentOrigin::Manual);
    eng.page(org(), manual, NotificationReason::Opened)
        .await
        .unwrap();

    let rows = ops.notifications_for(org(), manual).await.unwrap();
    assert_eq!(
        rows[0].channel_id,
        Some(cid),
        "a declared incident still pages"
    );
}

/// A mail cannot carry a control that reports back, so its button opens the
/// page where the signed-in reader takes the episode the alert was about. An
/// all-clear has nothing left to take.
#[tokio::test]
async fn an_alert_mail_links_the_acknowledge_page_for_its_episode() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = verified_mail_channel(&channels, true).await;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let (eng, mail) = engine_mailing(ops.clone(), targets, channels, EscalationConfig::default());

    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    ops.resolve(org(), id, Actor::System, None).await.unwrap();
    eng.page(org(), id, NotificationReason::Resolved)
        .await
        .unwrap();

    let sent = mail.sent();
    assert_eq!(sent.len(), 2);
    let opened = sent[0].template.render("Uptimepage").text_body;
    let page = format!(
        "https://app.test/incidents/{id}/acknowledge?org={}&channel={cid}&episode=0",
        org().0
    );
    assert!(opened.contains(&format!("Acknowledge: {page}")), "{opened}");
    let resolved = sent[1].template.render("Uptimepage").text_body;
    assert!(!resolved.contains("/acknowledge"), "{resolved}");
}

/// What a monitor delete leaves behind: the incident closed, the link gone,
/// the name kept.
fn close_with_deleted_monitor(ops: &InMemoryIncidentOpsStore, id: Uuid, monitor: Uuid) {
    ops.edit(id, |i| {
        i.state = IncidentState::Resolved;
        i.ended_at = Some(Utc::now());
        i.next_escalation_at = None;
        i.target_id = None;
        i.target_ref = Some(monitor);
        i.target_name = Some("api".into());
        i.closed_by_monitor_delete = true;
    });
}

/// The channel the incident paged hears it closed with its monitor, once, in
/// words that claim no recovery. The monitor's recovery opt-out went with it.
#[tokio::test]
async fn a_monitor_delete_tells_the_paged_channel_it_closed() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = verified_mail_channel(&channels, true).await;
    let target = target_with_channel_recovery(cid, false);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let (eng, mail) = engine_mailing(
        ops.clone(),
        targets.clone(),
        channels,
        EscalationConfig::default(),
    );
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();

    targets.delete(org(), tid, None).await.unwrap();
    close_with_deleted_monitor(&ops, id, tid);
    for _ in 0..2 {
        eng.page(org(), id, NotificationReason::MonitorDeleted)
            .await
            .unwrap();
    }

    let sent = mail.sent();
    assert_eq!(sent.len(), 2, "told once");
    let closed = sent[1].template.render("Uptimepage");
    assert!(
        closed
            .subject
            .contains("api — Monitoring was removed; this incident was closed."),
        "{}",
        closed.subject
    );
    assert!(
        closed.html_body.contains("MONITORING REMOVED"),
        "{}",
        closed.html_body
    );
    assert!(
        !closed.text_body.contains("Recovered"),
        "{}",
        closed.text_body
    );
    assert!(
        !closed.text_body.contains("/acknowledge"),
        "{}",
        closed.text_body
    );
}

/// A recovery notice still retrying when the monitor goes is true news and
/// still goes out; a page about the outage has nothing left to say.
#[tokio::test]
async fn a_delete_drops_pending_pages_but_not_a_pending_recovery() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let policies = Arc::new(InMemoryEscalationPolicyStore::new());
    let eng = engine(ops.clone(), policies, targets.clone(), channels);
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    ops.resolve(org(), id, Actor::System, None).await.unwrap();
    eng.page(org(), id, NotificationReason::Resolved)
        .await
        .unwrap();

    targets.delete(org(), tid, None).await.unwrap();
    ops.edit(id, |i| {
        i.target_id = None;
        i.target_ref = Some(tid);
        i.target_name = Some("api".into());
    });
    ops.clear_retry_backoff();
    eng.retry_pending().await;

    let rows = ops.notifications_for(org(), id).await.unwrap();
    let by_reason = |r: NotificationReason| rows.iter().find(|n| n.reason == r).unwrap();
    let opened = by_reason(NotificationReason::Opened);
    assert_eq!(opened.status, NotificationStatus::Suppressed);
    let resolved = by_reason(NotificationReason::Resolved);
    assert_eq!(
        resolved.status,
        NotificationStatus::Failed,
        "retried, not dropped"
    );
    assert_eq!(resolved.attempt, 2);
}

/// A declared incident stays open when its monitor goes. Paging stops, so an
/// emergency still sounding is spent, and the person who later resolves it
/// still tells whoever it paged.
#[tokio::test]
async fn a_declared_incident_outliving_its_monitor_goes_quiet_then_reports_its_resolve() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = verified_mail_channel(&channels, true).await;
    let target = target_with_channel_recovery(cid, false);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    ops.edit(id, |i| i.origin = IncidentOrigin::Manual);
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let (eng, mail) = engine_mailing(
        ops.clone(),
        targets.clone(),
        channels,
        EscalationConfig::default(),
    );
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();

    targets.delete(org(), tid, None).await.unwrap();
    ops.edit(id, |i| {
        i.target_ref = i.target_id.take();
        i.target_name = Some("api".into());
    });
    let ack = crate::storage::EmergencyAck {
        id: Uuid::now_v7(),
        org: org(),
        incident_id: id,
        channel_id: cid,
        receipt: "rcpt".into(),
        generation: 0,
    };
    assert!(
        eng.page_is_spent(&ack).await,
        "still open, no longer paging"
    );

    ops.resolve(org(), id, Actor::System, None).await.unwrap();
    eng.page(org(), id, NotificationReason::Resolved)
        .await
        .unwrap();
    let sent = mail.sent();
    assert_eq!(sent.len(), 2);
    let resolved = sent[1].template.render("Uptimepage");
    assert!(
        resolved.subject.contains("api — incident RESOLVED"),
        "{}",
        resolved.subject
    );
}

pub(super) async fn verified_mail_channel(
    channels: &InMemoryNotificationChannelStore,
    acknowledge_button: bool,
) -> Uuid {
    let cid = channels
        .create(
            org(),
            NewNotificationChannel {
                name: "oncall-mail".into(),
                config: ChannelConfig::Email(EmailConfig {
                    to: "oncall@example.test".into(),
                }),
                enabled: true,
                auto_bind_tags: Vec::new(),
                acknowledge_button,
                resolve_button: false,
            },
            WriteSource::Ui,
            100,
            None,
        )
        .await
        .unwrap()
        .id;
    let upd = channels.get(org(), cid).await.unwrap().unwrap().updated_at;
    assert!(channels.set_verified(org(), cid, upd).await.unwrap());
    cid
}

/// A room whose readers should not take incidents still gets the alert, only
/// without the button.
#[tokio::test]
async fn a_channel_with_the_button_off_pages_without_it() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = verified_mail_channel(&channels, false).await;
    let target = target_with_channel(cid);
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(target.id));
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let (eng, mail) = engine_mailing(ops, targets, channels, EscalationConfig::default());

    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();

    let sent = mail.sent();
    assert_eq!(sent.len(), 1);
    let body = sent[0].template.render("Uptimepage").text_body;
    assert!(!body.contains("/acknowledge"), "{body}");
}
