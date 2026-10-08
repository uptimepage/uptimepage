//! The closing notice a close owes reaches every channel the episode paged
//! exactly once, whether its signal lands or not.

use super::tests::{
    Ladder, channel_step, close_with_deleted_monitor, engine, engine_mailing, engine_maint,
    failing_channel, org, seed_incident, target_with_channel_recovery, two_step_ladder,
    two_step_ladder_with, verified_mail_channel, webhook_channel, window_over,
};
use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::EscalationConfig;
use crate::domain::{
    AlertBinding, NewEscalationPolicy, NotificationChannelUpdate, Target, TargetAlerts, WriteSource,
};
use crate::storage::{
    Actor, InMemoryEscalationPolicyStore, InMemoryIncidentOpsStore,
    InMemoryNotificationChannelStore, InMemoryTargetStore,
};

/// Past the sweep's wait for the signal.
fn past_grace() -> chrono::Duration {
    chrono::Duration::minutes(1)
}

struct Paged {
    ops: Arc<InMemoryIncidentOpsStore>,
    channels: Arc<InMemoryNotificationChannelStore>,
    eng: EscalationEngine,
    id: Uuid,
    cid: Uuid,
}

/// An open incident whose outage page reached one channel.
async fn paged(notify_recovery: bool) -> Paged {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel_recovery(cid, notify_recovery);
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(target.id));
    let eng = engine(
        ops.clone(),
        Arc::new(InMemoryEscalationPolicyStore::new()),
        Arc::new(InMemoryTargetStore::from_vec(vec![target])),
        channels.clone(),
    );
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    Paged {
        ops,
        channels,
        eng,
        id,
        cid,
    }
}

async fn closing_rows(ops: &InMemoryIncidentOpsStore, id: Uuid) -> Vec<(NotificationReason, Uuid)> {
    ops.notifications_for(org(), id)
        .await
        .unwrap()
        .into_iter()
        .filter(|n| n.reason.closes_incident())
        .map(|n| (n.reason, n.channel_id.expect("a delivery row")))
        .collect()
}

#[tokio::test]
async fn a_lost_resolve_signal_is_sent_once_its_grace_has_passed() {
    let p = paged(true).await;
    p.ops
        .resolve(org(), p.id, Actor::System, None)
        .await
        .unwrap();

    p.eng.reconcile_closed().await;
    assert!(
        closing_rows(&p.ops, p.id).await.is_empty(),
        "the signal still has time to land"
    );

    p.ops.age_closing_notices(past_grace());
    p.eng.reconcile_closed().await;
    assert_eq!(
        closing_rows(&p.ops, p.id).await,
        vec![(NotificationReason::Resolved, p.cid)]
    );

    p.ops.age_closing_notices(past_grace());
    p.eng.reconcile_closed().await;
    assert_eq!(closing_rows(&p.ops, p.id).await.len(), 1, "sent once");
    assert!(!p.ops.closing_notice_pending(p.id));
}

#[tokio::test]
async fn a_signal_that_landed_leaves_the_sweep_nothing_to_send() {
    let p = paged(true).await;
    p.ops
        .resolve(org(), p.id, Actor::System, None)
        .await
        .unwrap();
    p.eng
        .page(org(), p.id, NotificationReason::Resolved)
        .await
        .unwrap();
    assert_eq!(closing_rows(&p.ops, p.id).await.len(), 1);

    p.ops.age_closing_notices(past_grace());
    p.eng.reconcile_closed().await;
    assert_eq!(closing_rows(&p.ops, p.id).await.len(), 1);
    assert!(!p.ops.closing_notice_pending(p.id));
}

/// Another engine instance's sweep took the notice first; the signal landing
/// here leaves it to that sender.
#[tokio::test]
async fn a_signal_landing_while_a_sweep_holds_the_notice_sends_nothing() {
    let p = paged(true).await;
    p.ops
        .resolve(org(), p.id, Actor::System, None)
        .await
        .unwrap();
    p.ops.age_closing_notices(past_grace());
    let held = p.ops.due_closing_notices(15, 86_400, 60, 10).await.unwrap();
    assert_eq!(held.claimed.len(), 1);

    p.eng
        .page(org(), p.id, NotificationReason::Resolved)
        .await
        .unwrap();
    p.eng.reconcile_closed().await;
    assert!(closing_rows(&p.ops, p.id).await.is_empty());
    assert!(p.ops.closing_notice_pending(p.id), "still the holder's");
}

#[tokio::test]
async fn a_recovery_opt_out_is_settled_once_and_never_scanned_again() {
    let p = paged(false).await;
    p.ops
        .resolve(org(), p.id, Actor::System, None)
        .await
        .unwrap();
    p.ops.age_closing_notices(past_grace());

    p.eng.reconcile_closed().await;
    assert!(closing_rows(&p.ops, p.id).await.is_empty());
    assert!(!p.ops.closing_notice_pending(p.id));
    let due = p.ops.due_closing_notices(0, 86_400, 60, 10).await.unwrap();
    assert!(due.claimed.is_empty() && due.expired == 0);
}

/// The delete's own notice, not a recovery, even though the monitor had
/// opted out of recovery notices: that went with it.
#[tokio::test]
async fn a_monitor_delete_whose_signal_was_lost_still_says_monitoring_was_removed() {
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

    ops.age_closing_notices(past_grace());
    eng.reconcile_closed().await;

    assert_eq!(
        closing_rows(&ops, id).await,
        vec![(NotificationReason::MonitorDeleted, cid)]
    );
    let sent = mail.sent();
    assert_eq!(sent.len(), 2);
    let closed = sent[1].template.render("Uptimepage");
    assert!(
        closed.html_body.contains("MONITORING REMOVED"),
        "{}",
        closed.html_body
    );
}

#[tokio::test]
async fn a_notice_for_an_incident_that_ended_outside_the_window_is_withdrawn_unsent() {
    let p = paged(true).await;
    p.ops
        .resolve(org(), p.id, Actor::System, None)
        .await
        .unwrap();
    p.ops.age_closing_notices(chrono::Duration::days(2));

    p.eng.reconcile_closed().await;
    assert!(closing_rows(&p.ops, p.id).await.is_empty());
    assert!(!p.ops.closing_notice_pending(p.id));
}

/// A reopen withdraws what the last close owed, so a signal arriving late
/// says nothing about an incident running again. The next close owes its own
/// notice, to the channels this episode paged.
#[tokio::test]
async fn a_reopen_withdraws_the_notice_and_the_next_close_owes_its_own() {
    let p = paged(true).await;
    p.ops
        .resolve(org(), p.id, Actor::System, None)
        .await
        .unwrap();
    p.eng
        .page(org(), p.id, NotificationReason::Resolved)
        .await
        .unwrap();
    p.ops
        .reopen(org(), p.id, Actor::System, None)
        .await
        .unwrap();
    p.eng
        .page(org(), p.id, NotificationReason::Reopened)
        .await
        .unwrap();

    p.eng
        .page(org(), p.id, NotificationReason::Resolved)
        .await
        .unwrap();
    p.ops.age_closing_notices(past_grace());
    p.eng.reconcile_closed().await;
    assert_eq!(
        closing_rows(&p.ops, p.id).await.len(),
        1,
        "nothing while open"
    );

    p.ops
        .resolve(org(), p.id, Actor::System, None)
        .await
        .unwrap();
    p.ops.age_closing_notices(past_grace());
    p.eng.reconcile_closed().await;
    assert_eq!(
        closing_rows(&p.ops, p.id).await,
        vec![
            (NotificationReason::Resolved, p.cid),
            (NotificationReason::Resolved, p.cid)
        ]
    );
}

#[tokio::test]
async fn a_close_that_paged_nobody_settles_without_sending() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel_recovery(cid, true);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    let eng = engine_maint(
        ops.clone(),
        Arc::new(InMemoryTargetStore::from_vec(vec![target])),
        channels,
        window_over(tid, true).await,
    );
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    ops.resolve(org(), id, Actor::System, None).await.unwrap();

    ops.age_closing_notices(past_grace());
    eng.reconcile_closed().await;
    let rows = ops.notifications_for(org(), id).await.unwrap();
    assert!(
        rows.iter().all(|n| n.channel_id.is_none()),
        "only the hold's marker: {rows:?}"
    );
    assert!(!ops.closing_notice_pending(id));
}

/// A channel switched off after the outage page hears nothing more, and the
/// notice it would have had is not looked for again.
#[tokio::test]
async fn a_channel_turned_off_since_the_page_settles_without_sending() {
    let p = paged(true).await;
    p.channels
        .update(
            org(),
            p.cid,
            NotificationChannelUpdate {
                enabled: Some(false),
                ..Default::default()
            },
            WriteSource::Ui,
            None,
        )
        .await
        .unwrap()
        .expect("channel");
    p.ops
        .resolve(org(), p.id, Actor::System, None)
        .await
        .unwrap();

    p.ops.age_closing_notices(past_grace());
    p.eng.reconcile_closed().await;
    assert!(closing_rows(&p.ops, p.id).await.is_empty());
    assert!(!p.ops.closing_notice_pending(p.id));
}

/// The resolve signal lands only after a reopen withdrew its notice, so it
/// tells nobody. The reopen still pages and re-arms the ladder: the episode
/// began at the reopen, whether or not the last one's end was announced.
#[tokio::test]
async fn a_reopen_that_overtook_its_resolve_signal_still_pages_and_rearms() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = failing_channel(&channels).await;
    let target = target_with_channel_recovery(cid, true);
    let tid = target.id;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let id = seed_incident(&ops, Some(tid));
    let policies = Arc::new(InMemoryEscalationPolicyStore::new());
    let policy = policies
        .create(
            org(),
            NewEscalationPolicy {
                name: "p".into(),
                description: None,
                repeat_count: 0,
                steps: vec![channel_step(1, 300, cid)],
            },
            10,
        )
        .await
        .unwrap();
    policies
        .set_target_policy(org(), tid, Some(policy.id))
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

    ops.resolve(org(), id, Actor::System, None).await.unwrap();
    ops.reopen(org(), id, Actor::System, None).await.unwrap();
    eng.page(org(), id, NotificationReason::Resolved)
        .await
        .unwrap();
    eng.page(org(), id, NotificationReason::Reopened)
        .await
        .unwrap();

    let reasons: Vec<NotificationReason> = ops
        .notifications_for(org(), id)
        .await
        .unwrap()
        .into_iter()
        .map(|n| n.reason)
        .collect();
    assert_eq!(
        reasons,
        vec![NotificationReason::Opened, NotificationReason::Reopened]
    );
    let incident = ops.get(org(), id).await.unwrap().unwrap();
    assert!(incident.next_escalation_at.is_some(), "the ladder is armed");
}

/// A sweep took the next escalation step, then the incident was resolved and
/// reopened before the step ran. The step belonged to the episode that ended:
/// it pages nobody, and the reopen still pages the first step.
#[tokio::test]
async fn an_escalation_step_taken_before_a_reopen_pages_nobody() {
    let Ladder {
        ops,
        eng,
        id,
        first,
    } = two_step_ladder().await;
    let taken = ops.due_for_escalation(Utc::now(), 10, 60).await.unwrap();
    assert_eq!(taken.len(), 1);

    ops.resolve(org(), id, Actor::System, None).await.unwrap();
    ops.reopen(org(), id, Actor::System, None).await.unwrap();
    eng.w.escalate_one(&taken[0]).await.unwrap();
    eng.page(org(), id, NotificationReason::Reopened)
        .await
        .unwrap();

    let sent: Vec<(NotificationReason, Option<Uuid>)> = ops
        .notifications_for(org(), id)
        .await
        .unwrap()
        .into_iter()
        .map(|n| (n.reason, n.channel_id))
        .collect();
    assert_eq!(
        sent,
        vec![
            (NotificationReason::Opened, Some(first)),
            (NotificationReason::Reopened, Some(first))
        ]
    );
}

/// No maintenance windows. Once armed, the next check first runs what
/// happens elsewhere meanwhile.
#[derive(Default)]
struct Interrupted {
    inner: crate::storage::InMemoryMaintenanceStore,
    meanwhile: parking_lot::Mutex<Option<futures::future::BoxFuture<'static, ()>>>,
}

#[async_trait::async_trait]
impl crate::storage::MaintenanceStore for Interrupted {
    async fn create(
        &self,
        org: OrgId,
        new: crate::domain::NewMaintenanceWindow,
        source: WriteSource,
        actor: Option<crate::domain::UserId>,
    ) -> crate::error::Result<crate::domain::MaintenanceWindow> {
        self.inner.create(org, new, source, actor).await
    }
    async fn list(
        &self,
        org: OrgId,
        q: crate::storage::MaintenanceListQuery,
    ) -> crate::error::Result<Vec<crate::domain::MaintenanceWindow>> {
        self.inner.list(org, q).await
    }
    async fn get(
        &self,
        org: OrgId,
        id: Uuid,
    ) -> crate::error::Result<Option<crate::domain::MaintenanceWindow>> {
        self.inner.get(org, id).await
    }
    async fn update(
        &self,
        org: OrgId,
        id: Uuid,
        update: crate::domain::MaintenanceWindowUpdate,
        source: WriteSource,
        actor: Option<crate::domain::UserId>,
    ) -> crate::error::Result<Option<crate::domain::MaintenanceWindow>> {
        self.inner.update(org, id, update, source, actor).await
    }
    async fn delete(
        &self,
        org: OrgId,
        id: Uuid,
        source: WriteSource,
        actor: Option<crate::domain::UserId>,
    ) -> crate::error::Result<bool> {
        self.inner.delete(org, id, source, actor).await
    }
    async fn existing_target_ids(
        &self,
        org: OrgId,
        ids: &[Uuid],
    ) -> crate::error::Result<Vec<Uuid>> {
        self.inner.existing_target_ids(org, ids).await
    }
    async fn alerts_suppressed(&self, org: OrgId, target_id: Uuid) -> crate::error::Result<bool> {
        let meanwhile = self.meanwhile.lock().take();
        if let Some(meanwhile) = meanwhile {
            meanwhile.await;
        }
        self.inner.alerts_suppressed(org, target_id).await
    }
}

/// The step passed its claim check, then the incident was resolved and
/// reopened while it was still looking up where to page. Its page is the
/// episode it was claimed in, so the reopen still pages the first step.
#[tokio::test]
async fn a_reopen_while_an_escalation_step_is_under_way_still_pages() {
    let maintenance = Arc::new(Interrupted::default());
    let Ladder {
        ops,
        eng,
        id,
        first,
    } = two_step_ladder_with(maintenance.clone()).await;
    let taken = ops.due_for_escalation(Utc::now(), 10, 60).await.unwrap();
    assert_eq!(taken.len(), 1);
    let reopen = {
        let ops = ops.clone();
        async move {
            ops.resolve(org(), id, Actor::System, None).await.unwrap();
            ops.reopen(org(), id, Actor::System, None).await.unwrap();
        }
    };
    *maintenance.meanwhile.lock() = Some(Box::pin(reopen));

    eng.w.escalate_one(&taken[0]).await.unwrap();
    eng.page(org(), id, NotificationReason::Reopened)
        .await
        .unwrap();
    let reopened: Vec<Option<Uuid>> = ops
        .notifications_for(org(), id)
        .await
        .unwrap()
        .into_iter()
        .filter(|n| n.reason == NotificationReason::Reopened)
        .map(|n| n.channel_id)
        .collect();
    assert_eq!(reopened, vec![Some(first)]);
}

/// A recovery opt-out's close leaves nothing on the log to say the episode
/// ended. The reopen after it pages all the same, once.
#[tokio::test]
async fn a_reopen_after_a_close_that_told_nobody_still_pages() {
    let p = paged(false).await;
    p.ops
        .resolve(org(), p.id, Actor::System, None)
        .await
        .unwrap();
    p.eng
        .page(org(), p.id, NotificationReason::Resolved)
        .await
        .unwrap();
    assert!(closing_rows(&p.ops, p.id).await.is_empty());

    p.ops
        .reopen(org(), p.id, Actor::System, None)
        .await
        .unwrap();
    for _ in 0..2 {
        p.eng
            .page(org(), p.id, NotificationReason::Reopened)
            .await
            .unwrap();
    }
    let reopened: Vec<Option<Uuid>> = p
        .ops
        .notifications_for(org(), p.id)
        .await
        .unwrap()
        .into_iter()
        .filter(|n| n.reason == NotificationReason::Reopened)
        .map(|n| n.channel_id)
        .collect();
    assert_eq!(reopened, vec![Some(p.cid)]);
}

/// Something that happens elsewhere while a page is still being sent.
type Meanwhile = Arc<dyn Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

/// Answers 200 to anything. Once `armed`, the next request first runs
/// `meanwhile`, then answers.
async fn endpoint(meanwhile: Meanwhile, armed: Arc<AtomicBool>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let (meanwhile, armed) = (meanwhile.clone(), armed.clone());
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut scratch = [0u8; 4096];
                let _ = sock.read(&mut scratch).await;
                if armed.swap(false, Ordering::SeqCst) {
                    meanwhile().await;
                }
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                    .await;
            });
        }
    });
    format!("http://{addr}/notify")
}

/// An engine paging one monitor's incident to two channels on `url`.
async fn two_channels(
    ops: &Arc<InMemoryIncidentOpsStore>,
    id: Uuid,
    mut target: Target,
    url: &str,
) -> (EscalationEngine, Uuid, Uuid) {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let a = webhook_channel(&channels, url).await;
    let b = webhook_channel(&channels, url).await;
    target.alerts = TargetAlerts(vec![
        AlertBinding { channel_id: a },
        AlertBinding { channel_id: b },
    ]);
    let eng = engine(
        ops.clone(),
        Arc::new(InMemoryEscalationPolicyStore::new()),
        Arc::new(InMemoryTargetStore::from_vec(vec![target])),
        channels,
    );
    eng.page(org(), id, NotificationReason::Opened)
        .await
        .unwrap();
    (eng, a, b)
}

/// The incident is resolved and reopened while its outage page is still going
/// out. The rest of that page belongs to the episode it was sent for, so the
/// reopen still reaches every channel.
#[tokio::test]
async fn a_reopen_while_the_outage_page_is_still_sending_still_pages() {
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let target = target_with_channel_recovery(Uuid::now_v7(), true);
    let id = seed_incident(&ops, Some(target.id));
    let reopen: Meanwhile = {
        let ops = ops.clone();
        Arc::new(move || {
            let ops = ops.clone();
            Box::pin(async move {
                ops.resolve(org(), id, Actor::System, None).await.unwrap();
                ops.reopen(org(), id, Actor::System, None).await.unwrap();
            })
        })
    };
    let url = endpoint(reopen, Arc::new(AtomicBool::new(true))).await;
    let (eng, a, b) = two_channels(&ops, id, target, &url).await;

    eng.page(org(), id, NotificationReason::Resolved)
        .await
        .unwrap();
    eng.page(org(), id, NotificationReason::Reopened)
        .await
        .unwrap();
    let rows = ops.notifications_for(org(), id).await.unwrap();
    let told = |reason| {
        let mut ids: Vec<Uuid> = rows
            .iter()
            .filter(|n| n.reason == reason)
            .filter_map(|n| n.channel_id)
            .collect();
        ids.sort();
        ids
    };
    let mut both = vec![a, b];
    both.sort();
    assert_eq!(told(NotificationReason::Opened), both);
    assert_eq!(told(NotificationReason::Reopened), both);
    assert!(told(NotificationReason::Resolved).is_empty());
}

/// The notice is taken over while its first channel is still being told. The
/// first sender stops there; the one that took it over tells the rest, so
/// every channel hears it exactly once.
#[tokio::test]
async fn a_sender_that_loses_the_notice_mid_send_leaves_the_rest_to_the_next_holder() {
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let target = target_with_channel_recovery(Uuid::now_v7(), true);
    let id = seed_incident(&ops, Some(target.id));
    // What another engine instance does once this sender's lease runs out.
    let take_over: Meanwhile = {
        let ops = ops.clone();
        Arc::new(move || {
            let ops = ops.clone();
            Box::pin(async move {
                ops.age_closing_notices(chrono::Duration::minutes(2));
                let taken = ops.due_closing_notices(0, 86_400, 60, 10).await.unwrap();
                assert_eq!(taken.claimed.len(), 1);
            })
        })
    };
    let armed = Arc::new(AtomicBool::new(false));
    let url = endpoint(take_over, armed.clone()).await;
    let (eng, a, b) = two_channels(&ops, id, target, &url).await;

    ops.resolve(org(), id, Actor::System, None).await.unwrap();
    armed.store(true, Ordering::SeqCst);
    eng.page(org(), id, NotificationReason::Resolved)
        .await
        .unwrap();
    assert_eq!(closing_rows(&ops, id).await.len(), 1, "stopped once taken");
    assert!(ops.closing_notice_pending(id), "the new holder's to finish");

    // The holder that took it over: here the sweep, once that lease lapses.
    ops.age_closing_notices(chrono::Duration::minutes(2));
    eng.reconcile_closed().await;
    let mut told: Vec<Uuid> = closing_rows(&ops, id)
        .await
        .into_iter()
        .map(|(_, cid)| cid)
        .collect();
    told.sort();
    let mut both = vec![a, b];
    both.sort();
    assert_eq!(told, both);
    assert!(!ops.closing_notice_pending(id));
}
