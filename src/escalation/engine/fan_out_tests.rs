//! A page is recorded whole before any of it is sent, so a crash part way
//! leaves every row for the retry sweep instead of channels nobody pages.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration as StdDuration;

use crate::domain::{
    AlertBinding, EscalationTargetType, NewEscalationPolicy, NewEscalationStep,
    NewEscalationTarget, NewIncidentNotification, NotificationStatus, Target, TargetAlerts,
};
use crate::storage::{
    DueIncident, FanOut, FanOutKind, InMemoryEscalationPolicyStore, InMemoryIncidentOpsStore,
    InMemoryNotificationChannelStore, InMemoryTargetStore, QUEUED_TAKEOVER_SECS,
};

use super::tests::{
    bare_target, channel_step, engine, engine_cfg, failing_channel, org, seed_incident,
    webhook_channel,
};

/// A webhook endpoint that notes, on each request, how many `reason` rows the
/// incident already had, then holds the request until `together` requests
/// are open at once (or three seconds pass) and answers 200.
struct Witness {
    url: String,
    rows_seen: Arc<parking_lot::Mutex<Vec<usize>>>,
    met: Arc<AtomicUsize>,
}

async fn witness(
    ops: Arc<InMemoryIncidentOpsStore>,
    incident: Uuid,
    reason: NotificationReason,
    together: usize,
) -> Witness {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/notify", listener.local_addr().unwrap());
    let rows_seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let met = Arc::new(AtomicUsize::new(0));
    let open = Arc::new(AtomicUsize::new(0));
    let (seen, joined) = (rows_seen.clone(), met.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let (ops, seen, joined, open) =
                (ops.clone(), seen.clone(), joined.clone(), open.clone());
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut scratch = [0u8; 8192];
                let _ = sock.read(&mut scratch).await;
                let rows = ops.notifications_for(org(), incident).await.unwrap();
                seen.lock()
                    .push(rows.iter().filter(|n| n.reason == reason).count());
                open.fetch_add(1, Ordering::SeqCst);
                let deadline = tokio::time::Instant::now() + StdDuration::from_secs(3);
                while open.load(Ordering::SeqCst) < together
                    && tokio::time::Instant::now() < deadline
                {
                    tokio::time::sleep(StdDuration::from_millis(10)).await;
                }
                if open.load(Ordering::SeqCst) >= together {
                    joined.fetch_add(1, Ordering::SeqCst);
                }
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                    .await;
            });
        }
    });
    Witness {
        url,
        rows_seen,
        met,
    }
}

fn bound_to(mut target: Target, channels: &[Uuid]) -> Target {
    target.alerts = TargetAlerts(
        channels
            .iter()
            .map(|&channel_id| AlertBinding { channel_id })
            .collect(),
    );
    target
}

fn queued(incident_id: Uuid, channel_id: Uuid) -> NewIncidentNotification {
    NewIncidentNotification {
        org: org(),
        incident_id,
        escalation_level: Some(0),
        target_user_id: None,
        channel_id: Some(channel_id),
        transport: "webhook".into(),
        reason: NotificationReason::Opened,
        status: NotificationStatus::Queued,
        attempt: 1,
        error: None,
        sent_at: None,
        episode: 0,
    }
}

fn opening(incident_id: Uuid, rows: Vec<NewIncidentNotification>) -> FanOut {
    FanOut {
        org: org(),
        incident_id,
        episode: 0,
        rows,
        events: Vec::new(),
        kind: FanOutKind::Opening(None),
    }
}

#[tokio::test]
async fn an_opening_records_every_row_before_it_sends_any_and_sends_them_at_once() {
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let target = bare_target();
    let id = seed_incident(&ops, Some(target.id));
    let w = witness(ops.clone(), id, NotificationReason::Opened, 2).await;
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let a = webhook_channel(&channels, &w.url).await;
    let b = webhook_channel(&channels, &w.url).await;
    let eng = engine(
        ops.clone(),
        Arc::new(InMemoryEscalationPolicyStore::new()),
        Arc::new(InMemoryTargetStore::from_vec(vec![bound_to(
            target,
            &[a, b],
        )])),
        channels,
    );

    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();

    assert_eq!(
        *w.rows_seen.lock(),
        vec![2, 2],
        "both rows exist before the first send"
    );
    assert_eq!(w.met.load(Ordering::SeqCst), 2, "both sends run at once");
    let rows = ops.notifications_for(org(), id).await.unwrap();
    assert!(rows.iter().all(|n| n.status == NotificationStatus::Sent));
}

#[tokio::test]
async fn an_escalation_rung_records_every_row_before_it_sends_any() {
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let target = bare_target();
    let tid = target.id;
    let id = seed_incident(&ops, Some(tid));
    let w = witness(ops.clone(), id, NotificationReason::Escalated, 2).await;
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let first = failing_channel(&channels).await;
    let a = webhook_channel(&channels, &w.url).await;
    let b = webhook_channel(&channels, &w.url).await;
    let to = |channel_id| NewEscalationTarget {
        target_type: EscalationTargetType::Channel,
        user_id: None,
        schedule_id: None,
        channel_id: Some(channel_id),
    };
    let policies = Arc::new(InMemoryEscalationPolicyStore::new());
    let p = policies
        .create(
            org(),
            NewEscalationPolicy {
                name: "p".into(),
                description: None,
                repeat_count: 0,
                steps: vec![
                    channel_step(1, 0, first),
                    NewEscalationStep {
                        level: 2,
                        delay_secs: 0,
                        targets: vec![to(a), to(b)],
                    },
                ],
            },
            10,
        )
        .await
        .unwrap();
    policies
        .set_target_policy(org(), tid, Some(p.id))
        .await
        .unwrap();
    let eng = engine(
        ops.clone(),
        policies,
        Arc::new(InMemoryTargetStore::from_vec(vec![target])),
        channels,
    );
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();

    eng.escalate_due().await;

    assert_eq!(*w.rows_seen.lock(), vec![2, 2]);
    assert_eq!(w.met.load(Ordering::SeqCst), 2);
    assert_eq!(
        ops.get(org(), id).await.unwrap().unwrap().escalation_level,
        2
    );
}

#[tokio::test]
async fn a_reminder_records_every_row_before_it_sends_any() {
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let target = bare_target();
    let tid = target.id;
    let id = seed_incident(&ops, Some(tid));
    let w = witness(ops.clone(), id, NotificationReason::Reminder, 2).await;
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let a = webhook_channel(&channels, &w.url).await;
    let b = webhook_channel(&channels, &w.url).await;
    for channel in [a, b] {
        ops.record_notification(NewIncidentNotification {
            status: NotificationStatus::Sent,
            ..queued(id, channel)
        })
        .await
        .unwrap();
    }
    let eng = engine(
        ops.clone(),
        Arc::new(InMemoryEscalationPolicyStore::new()),
        Arc::new(InMemoryTargetStore::from_vec(vec![bound_to(
            target,
            &[a, b],
        )])),
        channels,
    );

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

    assert_eq!(*w.rows_seen.lock(), vec![2, 2]);
    assert_eq!(w.met.load(Ordering::SeqCst), 2);
    assert_eq!(ops.renotify_count(id), 1);
}

/// What a crash right after the commit leaves: rows nobody sent.
#[tokio::test]
async fn a_page_recorded_but_never_sent_is_sent_once_by_the_retry_sweep() {
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let target = bare_target();
    let id = seed_incident(&ops, Some(target.id));
    let w = witness(ops.clone(), id, NotificationReason::Opened, 1).await;
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let a = webhook_channel(&channels, &w.url).await;
    let b = webhook_channel(&channels, &w.url).await;
    let eng = engine(
        ops.clone(),
        Arc::new(InMemoryEscalationPolicyStore::new()),
        Arc::new(InMemoryTargetStore::from_vec(vec![bound_to(
            target,
            &[a, b],
        )])),
        channels,
    );
    ops.record_fan_out(opening(id, vec![queued(id, a), queued(id, b)]))
        .await
        .unwrap()
        .expect("recorded");

    eng.retry_pending().await;
    assert!(
        w.rows_seen.lock().is_empty(),
        "a first attempt may still be under way"
    );
    ops.age_notifications(chrono::Duration::seconds(QUEUED_TAKEOVER_SECS + 1));
    eng.retry_pending().await;
    eng.retry_pending().await;

    assert_eq!(w.rows_seen.lock().len(), 2, "each row sent exactly once");
    let rows = ops.notifications_for(org(), id).await.unwrap();
    assert!(rows.iter().all(|n| n.status == NotificationStatus::Sent));
}

#[tokio::test]
async fn with_one_attempt_allowed_a_recorded_row_is_still_taken_over() {
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = bound_to(bare_target(), &[cid]);
    let id = seed_incident(&ops, Some(target.id));
    let cfg = EscalationConfig {
        max_attempts: 1,
        ..Default::default()
    };
    let eng = engine_cfg(
        ops.clone(),
        Arc::new(InMemoryEscalationPolicyStore::new()),
        Arc::new(InMemoryTargetStore::from_vec(vec![target])),
        channels,
        cfg,
    );
    ops.record_fan_out(opening(id, vec![queued(id, cid)]))
        .await
        .unwrap()
        .expect("recorded");
    ops.age_notifications(chrono::Duration::seconds(QUEUED_TAKEOVER_SECS + 1));

    eng.retry_pending().await;
    ops.clear_retry_backoff();
    eng.retry_pending().await;

    let rows = ops.notifications_for(org(), id).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].attempt, 2, "taken over once, then left alone");
    assert_eq!(rows[0].status, NotificationStatus::Failed);
    assert!(rows[0].next_attempt_at.is_none(), "dead-lettered");
}

/// The row is what the retry sweep reads, so it lands first; a slow health
/// update after it cannot leave a delivered page looking unsent.
#[tokio::test]
async fn a_row_is_marked_before_its_channel_health_is_written() {
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let target = bare_target();
    let id = seed_incident(&ops, Some(target.id));
    let w = witness(ops.clone(), id, NotificationReason::Opened, 1).await;
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = webhook_channel(&channels, &w.url).await;
    channels.delay_delivery_outcomes(StdDuration::from_millis(500));
    let eng = engine(
        ops.clone(),
        Arc::new(InMemoryEscalationPolicyStore::new()),
        Arc::new(InMemoryTargetStore::from_vec(vec![bound_to(
            target,
            &[cid],
        )])),
        channels.clone(),
    );

    let watch = async {
        let deadline = tokio::time::Instant::now() + StdDuration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            let rows = ops.notifications_for(org(), id).await.unwrap();
            if rows.iter().any(|n| n.status == NotificationStatus::Sent) {
                return channels.get(org(), cid).await.unwrap().unwrap();
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
        panic!("the row was never marked sent");
    };
    let (paged, channel) = tokio::join!(eng.page(org(), id, NotificationReason::Opened), watch);
    paged.unwrap();

    assert!(
        channel.last_delivered_at.is_none(),
        "the row was marked while the health update was still pending"
    );
}

#[tokio::test]
async fn an_empty_first_rung_released_from_a_hold_ends_the_hold_with_its_arming() {
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let target = bare_target();
    let tid = target.id;
    let policies = Arc::new(InMemoryEscalationPolicyStore::new());
    let p = policies
        .create(
            org(),
            NewEscalationPolicy {
                name: "p".into(),
                description: None,
                repeat_count: 0,
                // A channel that no longer exists: the rung reaches nobody.
                steps: vec![channel_step(1, 600, Uuid::now_v7())],
            },
            10,
        )
        .await
        .unwrap();
    policies
        .set_target_policy(org(), tid, Some(p.id))
        .await
        .unwrap();
    let cfg = EscalationConfig {
        flap_max_opens: 1,
        ..Default::default()
    };
    let eng = engine_cfg(
        ops.clone(),
        policies,
        Arc::new(InMemoryTargetStore::from_vec(vec![target])),
        Arc::new(InMemoryNotificationChannelStore::new()),
        cfg,
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

    eng.release_held().await;
    let armed = ops.get(org(), held).await.unwrap().unwrap();
    let rows = ops.notifications_for(org(), held).await.unwrap();
    assert!(armed.next_escalation_at.is_some(), "the ladder is armed");
    assert_eq!(
        rows.iter()
            .filter(|n| n.transport == rules::RELEASED_TRANSPORT)
            .count(),
        1,
        "and the hold ended with it"
    );

    eng.release_held().await;
    let again = ops.get(org(), held).await.unwrap().unwrap();
    assert_eq!(
        again.next_escalation_at, armed.next_escalation_at,
        "a second release does not re-arm the ladder"
    );
    assert_eq!(
        ops.notifications_for(org(), held).await.unwrap().len(),
        rows.len()
    );
}

#[tokio::test]
async fn a_monitor_paused_between_the_release_scan_and_its_page_keeps_its_hold() {
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = bound_to(bare_target(), &[cid]);
    let tid = target.id;
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let cfg = EscalationConfig {
        flap_max_opens: 1,
        ..Default::default()
    };
    let eng = engine_cfg(
        ops.clone(),
        Arc::new(InMemoryEscalationPolicyStore::new()),
        targets.clone(),
        channels,
        cfg,
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
    let hold_only = ops.notifications_for(org(), held).await.unwrap();

    // The in-memory scan does not read the monitor, so this is the pause
    // landing after the scan and before the page.
    targets
        .set_enabled(org(), &[tid], false, None)
        .await
        .unwrap();
    eng.release_held().await;
    assert_eq!(
        ops.notifications_for(org(), held).await.unwrap().len(),
        hold_only.len(),
        "nothing recorded while paused, so the hold stays pending"
    );

    targets
        .set_enabled(org(), &[tid], true, None)
        .await
        .unwrap();
    eng.release_held().await;
    let rows = ops.notifications_for(org(), held).await.unwrap();
    assert!(
        rows.iter().any(|n| n.channel_id == Some(cid)),
        "released once the monitor is back"
    );
}

#[tokio::test]
async fn a_rung_whose_channels_are_all_gone_says_it_reached_no_channel() {
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let target = bare_target();
    let tid = target.id;
    let id = seed_incident(&ops, Some(tid));
    let policies = Arc::new(InMemoryEscalationPolicyStore::new());
    let p = policies
        .create(
            org(),
            NewEscalationPolicy {
                name: "p".into(),
                description: None,
                repeat_count: 0,
                steps: vec![channel_step(1, 600, Uuid::now_v7())],
            },
            10,
        )
        .await
        .unwrap();
    policies
        .set_target_policy(org(), tid, Some(p.id))
        .await
        .unwrap();
    let eng = engine(
        ops.clone(),
        policies,
        Arc::new(InMemoryTargetStore::from_vec(vec![target])),
        Arc::new(InMemoryNotificationChannelStore::new()),
    );

    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();

    let timeline = ops.timeline(org(), id).await.unwrap();
    assert!(
        timeline.iter().any(|e| e
            .message
            .as_deref()
            .is_some_and(|m| m.contains("reached no channel"))),
        "{timeline:?}"
    );
}
