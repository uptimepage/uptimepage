use super::tests::{
    engine_mailing, org, seed_incident, target_with_channel, verified_mail_channel,
};
use super::*;
use std::sync::Arc;

use crate::domain::WriteSource;
use crate::storage::{
    Actor, InMemoryIncidentOpsStore, InMemoryNotificationChannelStore, InMemoryTargetStore,
};

/// Resolve is off until the channel switches it on, and then the mail carries
/// a second link, to the resolve page for the same episode. An all-clear has
/// nothing to resolve.
#[tokio::test]
async fn an_alert_mail_links_the_resolve_page_only_once_the_channel_switches_it_on() {
    let mails = |resolve_on: bool| async move {
        let channels = Arc::new(InMemoryNotificationChannelStore::new());
        let cid = verified_mail_channel(&channels, true).await;
        if resolve_on {
            channels
                .update(
                    org(),
                    cid,
                    crate::domain::NotificationChannelUpdate {
                        resolve_button: Some(true),
                        ..Default::default()
                    },
                    WriteSource::Ui,
                    None,
                )
                .await
                .unwrap()
                .expect("channel");
        }
        let target = target_with_channel(cid);
        let ops = Arc::new(InMemoryIncidentOpsStore::new());
        let id = seed_incident(&ops, Some(target.id));
        let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
        let (eng, mail) =
            engine_mailing(ops.clone(), targets, channels, EscalationConfig::default());
        eng.page(org(), id, NotificationReason::Opened)
            .await
            .unwrap();
        ops.resolve(org(), id, Actor::System, None).await.unwrap();
        eng.page(org(), id, NotificationReason::Resolved)
            .await
            .unwrap();
        let sent = mail.sent();
        assert_eq!(sent.len(), 2);
        let text = |n: usize| sent[n].template.render("Uptimepage").text_body;
        (id, cid, text(0), text(1))
    };

    let (_, _, off, _) = mails(false).await;
    assert!(off.contains("Acknowledge: "), "{off}");
    assert!(!off.contains("Resolve: "), "{off}");

    let (id, cid, on, resolved) = mails(true).await;
    let page = format!(
        "https://app.test/incidents/{id}/resolve?org={}&channel={cid}&episode=0",
        org().0
    );
    assert!(on.contains(&format!("Resolve: {page}")), "{on}");
    assert!(on.contains("Acknowledge: "), "{on}");
    assert!(!resolved.contains("/resolve"), "{resolved}");
}

/// Every kind that offers a control gets one only while its switch is on,
/// the bearer ones included: an ntfy link or a bot button lets anyone reading
/// the room take the incident. Resolve has a switch of its own, off by default,
/// and a kind whose press cannot name the presser never carries it.
#[tokio::test]
async fn the_switch_decides_the_control_for_every_kind_that_offers_one() {
    use crate::domain::AlertAction::{Acknowledge, Resolve};
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = verified_mail_channel(&channels, true).await;
    let mut channel = channels.get(org(), cid).await.unwrap().unwrap();
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let mut notice = crate::notifier::card::tests::notice(NotificationReason::Opened);
    notice.incident_id = seed_incident(&ops, None);
    let targets = Arc::new(InMemoryTargetStore::from_vec(Vec::new()));
    let (mut eng, _) = engine_mailing(ops, targets, channels, EscalationConfig::default());
    let w = Arc::get_mut(&mut eng.w).expect("sole owner");
    w.incident_ack_secret = "engine-acknowledge-test-secret".into();
    w.pressed_apps = crate::domain::LinkedApp::ALL.to_vec();

    for kind in crate::domain::ChannelKind::ALL {
        channel.kind = *kind;
        for (acknowledge, resolve) in [(true, true), (true, false), (false, true), (false, false)] {
            channel.acknowledge_button = acknowledge;
            channel.resolve_button = resolve;
            let got = eng.w.alert_controls(org(), &channel, &notice).await;
            assert_eq!(
                got.acknowledge.is_some(),
                acknowledge && kind.offers(Acknowledge),
                "{kind:?} acknowledge {acknowledge}/{resolve}"
            );
            assert_eq!(
                got.resolve.is_some(),
                resolve && kind.offers(Resolve),
                "{kind:?} resolve {acknowledge}/{resolve}"
            );
        }
    }
}

/// A button minted for an incident still to be taken must not ride along on
/// the all-clear, Resolve included.
#[tokio::test]
async fn an_all_clear_carries_no_control() {
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = verified_mail_channel(&channels, true).await;
    let mut channel = channels.get(org(), cid).await.unwrap().unwrap();
    channel.resolve_button = true;
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let mut notice = crate::notifier::card::tests::notice(NotificationReason::Resolved);
    notice.incident_id = seed_incident(&ops, None);
    let targets = Arc::new(InMemoryTargetStore::from_vec(Vec::new()));
    let (mut eng, _) = engine_mailing(ops, targets, channels, EscalationConfig::default());
    Arc::get_mut(&mut eng.w)
        .expect("sole owner")
        .incident_ack_secret = "engine-acknowledge-test-secret".into();

    let got = eng.w.alert_controls(org(), &channel, &notice).await;
    assert!(got.acknowledge.is_none() && got.resolve.is_none());
}

/// A channel connected through one of our apps carries a button only where
/// this deployment receives that app's presses; elsewhere it links to the
/// acknowledge page, rather than carry a button nothing answers.
#[tokio::test]
async fn an_app_channel_gets_its_button_only_where_presses_arrive() {
    use crate::domain::{AlertVia, ChannelKind};
    use crate::notifier::AckControl;
    let channels = Arc::new(InMemoryNotificationChannelStore::new());
    let cid = verified_mail_channel(&channels, true).await;
    let mut channel = channels.get(org(), cid).await.unwrap().unwrap();
    let ops = Arc::new(InMemoryIncidentOpsStore::new());
    let mut notice = crate::notifier::card::tests::notice(NotificationReason::Opened);
    notice.incident_id = seed_incident(&ops, None);
    let targets = Arc::new(InMemoryTargetStore::from_vec(Vec::new()));
    let (mut eng, _) = engine_mailing(ops, targets, channels, EscalationConfig::default());
    Arc::get_mut(&mut eng.w)
        .expect("sole owner")
        .incident_ack_secret = "engine-acknowledge-test-secret".into();

    for kind in ChannelKind::ALL {
        let Some(AlertVia::Button(app)) = kind.acknowledge_via() else {
            continue;
        };
        channel.kind = *kind;
        channel.resolve_button = true;
        Arc::get_mut(&mut eng.w).expect("sole owner").pressed_apps = Vec::new();
        let unreceived = eng.w.alert_controls(org(), &channel, &notice).await;
        for control in [unreceived.acknowledge, unreceived.resolve] {
            assert!(matches!(control, Some(AckControl::Page(_))), "{kind:?}");
        }

        Arc::get_mut(&mut eng.w).expect("sole owner").pressed_apps = vec![app];
        let received = eng.w.alert_controls(org(), &channel, &notice).await;
        for control in [received.acknowledge, received.resolve] {
            assert!(matches!(control, Some(AckControl::Button(_))), "{kind:?}");
        }
    }
}
