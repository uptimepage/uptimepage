//! An episode owes its first page from the moment it starts until that page
//! is recorded, so a dropped signal or a pause is caught up whatever its age.

use super::*;

use crate::domain::{
    NewEscalationPolicy, NewIncidentNotification, NotificationOutcome, NotificationStatus,
};
use crate::storage::{
    Actor, InMemoryEscalationPolicyStore, InMemoryIncidentOpsStore,
    InMemoryNotificationChannelStore, InMemoryTargetStore,
};

use super::tests::{
    channel_step, engine, engine_cfg, failing_channel, org, seed_incident, target_with_channel,
    window_over,
};

fn sent_row(incident_id: Uuid, channel_id: Uuid, episode: i64) -> NewIncidentNotification {
    NewIncidentNotification {
        org: org(),
        incident_id,
        escalation_level: Some(0),
        target_user_id: None,
        channel_id: Some(channel_id),
        transport: "webhook".into(),
        reason: NotificationReason::Opened,
        status: NotificationStatus::Sent,
        attempt: 1,
        error: None,
        sent_at: Some(Utc::now()),
        episode,
    }
}

async fn ladder_to(policies: &InMemoryEscalationPolicyStore, tid: Uuid, channel: Uuid, wait: i32) {
    let p = policies
        .create(
            org(),
            NewEscalationPolicy {
                name: "p".into(),
                description: None,
                repeat_count: 0,
                steps: vec![channel_step(1, wait, channel)],
            },
            10,
        )
        .await
        .unwrap();
    policies
        .set_target_policy(org(), tid, Some(p.id))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_monitor_paused_before_its_first_page_is_paged_once_it_is_resumed() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    targets
        .set_enabled(org(), &[tid], false, None)
        .await
        .unwrap();
    let eng = engine(
        ops.clone(),
        Arc::new(InMemoryEscalationPolicyStore::new()),
        targets.clone(),
        channels,
    );

    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    eng.page_owed().await;
    assert!(ops.notifications_for(org(), id).await.unwrap().is_empty());
    assert!(ops.owes_page(id), "still owed while paused");

    targets
        .set_enabled(org(), &[tid], true, None)
        .await
        .unwrap();
    // The try while paused moved it behind the rest; a tick passes.
    ops.age_owed_pages(chrono::Duration::minutes(1));
    eng.page_owed().await;
    eng.page_owed().await;
    let rows = ops.notifications_for(org(), id).await.unwrap();
    assert_eq!(rows.len(), 1, "paged once after the resume");
    assert_eq!(rows[0].reason, NotificationReason::Opened);
    assert!(!ops.owes_page(id));
}

/// A reopen whose signal was dropped is paged as a reopen, and its ladder is
/// armed again.
#[tokio::test]
async fn a_dropped_reopen_signal_is_paged_and_re_arms_the_ladder() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    let policies = Arc::new(InMemoryEscalationPolicyStore::new());
    ladder_to(&policies, tid, cid, 600).await;
    let eng = engine(
        ops.clone(),
        policies,
        Arc::new(InMemoryTargetStore::from_vec(vec![target])),
        channels,
    );
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    ops.resolve(org(), id, Actor::System, None).await.unwrap();
    ops.reopen(org(), id, Actor::System, None).await.unwrap();
    assert_eq!(
        ops.get(org(), id)
            .await
            .unwrap()
            .unwrap()
            .next_escalation_at,
        None
    );

    ops.age_owed_pages(chrono::Duration::minutes(1));
    eng.page_owed().await;

    let rows = ops.notifications_for(org(), id).await.unwrap();
    let reopened: Vec<_> = rows.iter().filter(|n| n.episode == 1).collect();
    assert_eq!(reopened.len(), 1);
    assert_eq!(reopened[0].reason, NotificationReason::Reopened);
    let inc = ops.get(org(), id).await.unwrap().unwrap();
    assert!(inc.next_escalation_at.is_some(), "the ladder runs again");
}

/// The sweep and a late signal for the same episode open it once, even when
/// its first rung reached nobody and so left no row behind.
#[tokio::test]
async fn a_late_signal_after_the_sweep_opened_the_episode_changes_nothing() {
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let target = super::tests::bare_target();
    let tid = target.id;
    let id = seed_incident(&ops, Some(tid));
    let policies = Arc::new(InMemoryEscalationPolicyStore::new());
    ladder_to(&policies, tid, Uuid::now_v7(), 600).await;
    let eng = engine(
        ops.clone(),
        policies,
        Arc::new(InMemoryTargetStore::from_vec(vec![target])),
        Arc::new(InMemoryNotificationChannelStore::new()),
    );
    ops.age_owed_pages(chrono::Duration::minutes(1));
    eng.page_owed().await;
    let armed = ops.get(org(), id).await.unwrap().unwrap();
    assert!(armed.next_escalation_at.is_some());

    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();

    let again = ops.get(org(), id).await.unwrap().unwrap();
    assert_eq!(again.next_escalation_at, armed.next_escalation_at);
    let empty_rung_lines = ops
        .timeline(org(), id)
        .await
        .unwrap()
        .iter()
        .filter(|e| {
            e.message
                .as_deref()
                .is_some_and(|m| m.contains("reached no channel"))
        })
        .count();
    assert_eq!(empty_rung_lines, 1);
}

/// A hold, a maintenance window and a monitor nobody can be paged about each
/// settle what is owed, so the sweep does not run them again every tick.
#[tokio::test]
async fn whatever_stands_in_for_the_first_page_settles_it() {
    // Nobody to page.
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let target = target_with_channel(Uuid::now_v7());
    let lonely = seed_incident(&ops, Some(target.id));
    let eng = engine(
        ops.clone(),
        Arc::new(InMemoryEscalationPolicyStore::new()),
        Arc::new(InMemoryTargetStore::from_vec(vec![target])),
        Arc::new(InMemoryNotificationChannelStore::new()),
    );
    eng.page(org(), lonely, NotificationReason::Opened)
        .await
        .unwrap();
    assert!(!ops.owes_page(lonely), "reached nobody");

    // Held by the flap damper.
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let cfg = EscalationConfig {
        flap_max_opens: 1,
        ..Default::default()
    };
    let eng = engine_cfg(
        ops.clone(),
        Arc::new(InMemoryEscalationPolicyStore::new()),
        Arc::new(InMemoryTargetStore::from_vec(vec![target.clone()])),
        channels.clone(),
        cfg,
    );
    eng.page(
        org(),
        seed_incident(&ops, Some(tid)),
        NotificationReason::Opened,
    )
    .await
    .unwrap();
    let damped = seed_incident(&ops, Some(tid));
    eng.page(org(), damped, NotificationReason::Opened)
        .await
        .unwrap();
    assert!(!ops.owes_page(damped), "held by the damper");

    // Held by a maintenance window.
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let maintenance = window_over(tid, true).await;
    let eng = super::tests::engine_maint(
        ops.clone(),
        Arc::new(InMemoryTargetStore::from_vec(vec![target])),
        channels,
        maintenance,
    );
    let held = seed_incident(&ops, Some(tid));
    eng.page(org(), held, NotificationReason::Opened)
        .await
        .unwrap();
    assert!(!ops.owes_page(held), "held by the window");
}

/// After a reopen whose signal was lost, the last episode's channels that
/// never heard it close must not be reminded before the reopen is paged:
/// that row would read as the episode's first page.
#[tokio::test]
async fn a_reminder_waits_for_the_first_page_of_its_episode() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    let eng = engine(
        ops.clone(),
        Arc::new(InMemoryEscalationPolicyStore::new()),
        Arc::new(InMemoryTargetStore::from_vec(vec![target])),
        channels,
    );
    ops.record_notification(sent_row(id, cid, 0)).await.unwrap();
    ops.settle_owed_page(id);
    ops.resolve(org(), id, Actor::System, None).await.unwrap();
    ops.reopen(org(), id, Actor::System, None).await.unwrap();
    let due = crate::storage::DueIncident {
        id,
        org: org(),
        target_id: Some(tid),
        escalation_policy_id: None,
        escalation_level: 0,
        escalation_round: 0,
    };

    eng.renotify_one(&due).await.unwrap();
    assert!(
        ops.notifications_for(org(), id)
            .await
            .unwrap()
            .iter()
            .all(|n| n.episode == 0),
        "no reminder before the reopen is paged"
    );

    ops.age_owed_pages(chrono::Duration::minutes(1));
    eng.page_owed().await;
    eng.renotify_one(&due).await.unwrap();
    let reasons: Vec<_> = ops
        .notifications_for(org(), id)
        .await
        .unwrap()
        .iter()
        .filter(|n| n.episode == 1)
        .map(|n| n.reason)
        .collect();
    assert_eq!(
        reasons,
        vec![NotificationReason::Reopened, NotificationReason::Reminder]
    );
}

/// Pages that cannot go out now must not hold up the ones behind them, however
/// many there are.
#[tokio::test]
async fn owed_pages_that_cannot_go_out_never_hold_up_the_rest() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let paused = target_with_channel(cid);
    let healthy = target_with_channel(cid);
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let stuck = [
        seed_incident(&ops, Some(paused.id)),
        seed_incident(&ops, Some(paused.id)),
    ];
    let waiting = seed_incident(&ops, Some(healthy.id));
    // The stuck pages are the longest owed, so they fill every batch first.
    ops.age_owed_pages(chrono::Duration::minutes(1));
    for id in stuck {
        ops.age_owed_page(id, chrono::Duration::hours(1));
    }
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![paused.clone(), healthy]));
    targets
        .set_enabled(org(), &[paused.id], false, None)
        .await
        .unwrap();
    let cfg = EscalationConfig {
        max_pages_per_tick: 2,
        ..Default::default()
    };
    let eng = engine_cfg(
        ops.clone(),
        Arc::new(InMemoryEscalationPolicyStore::new()),
        targets,
        channels,
        cfg,
    );

    eng.page_owed().await;
    eng.page_owed().await;

    assert_eq!(
        ops.notifications_for(org(), waiting).await.unwrap().len(),
        1
    );
    assert!(stuck.iter().all(|id| ops.owes_page(*id)));
}

/// An episode an earlier build paged has rows but still reads as owed; the
/// sweep settles it rather than paging it again.
#[tokio::test]
async fn an_episode_paged_without_settling_what_it_owed_is_not_paged_again() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(target.id));
    ops.record_notification(sent_row(id, cid, 0)).await.unwrap();
    let eng = engine(
        ops.clone(),
        Arc::new(InMemoryEscalationPolicyStore::new()),
        Arc::new(InMemoryTargetStore::from_vec(vec![target])),
        channels,
    );

    ops.age_owed_pages(chrono::Duration::minutes(1));
    eng.page_owed().await;

    assert_eq!(ops.notifications_for(org(), id).await.unwrap().len(), 1);
    assert!(!ops.owes_page(id));
}

/// A reopen stops the last episode's emergency pages, and only those: a second
/// signal for the reopen must not stop the one it already raised.
#[tokio::test]
async fn a_reopen_cancels_only_the_last_episodes_emergency_pages() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(target.id));
    let eng = engine(
        ops.clone(),
        Arc::new(InMemoryEscalationPolicyStore::new()),
        Arc::new(InMemoryTargetStore::from_vec(vec![target])),
        channels,
    );
    let with_receipt = async |episode| {
        let row = ops
            .record_notification(sent_row(id, cid, episode))
            .await
            .unwrap();
        ops.mark_notification(
            org(),
            row,
            NotificationOutcome {
                status: NotificationStatus::Sent,
                attempt: 1,
                error: None,
                sent_at: Some(Utc::now()),
                next_attempt_at: None,
                provider_receipt: Some(format!("receipt-{episode}")),
            },
        )
        .await
        .unwrap();
    };
    with_receipt(0).await;
    ops.settle_owed_page(id);
    ops.resolve(org(), id, Actor::System, None).await.unwrap();
    ops.reopen(org(), id, Actor::System, None).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    with_receipt(1).await;

    eng.page(org(), id, NotificationReason::Reopened)
        .await
        .unwrap();

    let left: Vec<String> = ops
        .emergency_acks_for_incident(org(), id)
        .await
        .unwrap()
        .into_iter()
        .map(|a| a.receipt)
        .collect();
    assert_eq!(left, vec!["receipt-1".to_string()]);
}

/// A hold left from an earlier episode is not released into a reopen that
/// still owes its page: the reopen pages as itself.
#[tokio::test]
async fn a_hold_from_before_a_reopen_does_not_stand_in_for_its_page() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel(cid);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let cfg = EscalationConfig {
        flap_max_opens: 1,
        ..Default::default()
    };
    let eng = engine_cfg(
        ops.clone(),
        Arc::new(InMemoryEscalationPolicyStore::new()),
        Arc::new(InMemoryTargetStore::from_vec(vec![target])),
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
    ops.resolve(org(), held, Actor::System, None).await.unwrap();
    ops.reopen(org(), held, Actor::System, None).await.unwrap();
    ops.age_held_rows(chrono::Duration::seconds(900));

    eng.release_held().await;
    let reopened = |rows: Vec<crate::domain::IncidentNotification>| {
        rows.into_iter()
            .filter(|n| n.episode == 1)
            .map(|n| (n.reason, n.transport))
            .collect::<Vec<_>>()
    };
    assert!(
        reopened(ops.notifications_for(org(), held).await.unwrap()).is_empty(),
        "the old hold released nothing into the reopen"
    );
    assert!(ops.owes_page(held));

    ops.age_owed_pages(chrono::Duration::minutes(1));
    eng.page_owed().await;
    // Its own opening, which the damper holds again: the monitor still flaps.
    assert_eq!(
        reopened(ops.notifications_for(org(), held).await.unwrap()),
        vec![(NotificationReason::Reopened, "damped".to_string())]
    );
    assert!(!ops.owes_page(held));
}
