use std::sync::Arc;

use async_trait::async_trait;

use crate::email::templates::incident_alert::IncidentAlert;
use crate::email::{EmailAddress, EmailSender, EmailTemplate, TransactionalEmail};
use crate::error::Result;
use crate::notifier::card::alert_link;
use crate::notifier::event::IncidentNotice;
use crate::notifier::transport::Notifier;

/// Transactional-mail context for alert delivery: the process-wide sender
/// plus the product's From identity. Owned by long-lived senders (engine,
/// app state); `None` at a build site fails the email transport loudly.
#[derive(Clone)]
pub struct EmailDelivery {
    pub sender: Arc<dyn EmailSender>,
    pub from_address: String,
    pub from_name: String,
}

/// Per-send attribution for alert mail: the sending org's name and the
/// recipient's one-click stop link. Absent for test sends.
#[derive(Default)]
pub struct EmailAlert {
    pub org_name: Option<String>,
    pub stop_url: Option<String>,
}

/// Attribution + stop link for an email channel; `None` for every other
/// transport, so the org lookup is skipped unless the recipient is an inbox.
/// Shared by the escalation engine and the silence sweep so both alert streams
/// carry the same footer.
pub async fn email_alert_for(
    orgs: &dyn crate::storage::orgs::OrgDirectory,
    base_url: &str,
    stop_secret: &str,
    org: crate::domain::OrgId,
    channel: &crate::domain::NotificationChannel,
) -> Option<EmailAlert> {
    if channel.kind != crate::domain::ChannelKind::Email {
        return None;
    }
    Some(EmailAlert {
        org_name: orgs.display_name(org).await.ok().flatten(),
        stop_url: crate::storage::notification_channels::channel_stop_url(
            base_url,
            stop_secret,
            channel.id,
        ),
    })
}

pub struct EmailNotifier {
    delivery: EmailDelivery,
    to: String,
    alert: EmailAlert,
    ack_link: Option<String>,
    resolve_link: Option<String>,
}

impl EmailNotifier {
    pub fn new(delivery: &EmailDelivery, to: &str, alert: EmailAlert) -> Self {
        Self {
            delivery: delivery.clone(),
            to: to.to_string(),
            alert,
            ack_link: None,
            resolve_link: None,
        }
    }

    pub fn with_ack_link(mut self, ack_link: Option<String>) -> Self {
        self.ack_link = ack_link;
        self
    }

    pub fn with_resolve_link(mut self, resolve_link: Option<String>) -> Self {
        self.resolve_link = resolve_link;
        self
    }
}

/// An inbox wraps and scrolls, so it takes far more of a failure than the
/// 200 chars a chat line gets — but a page of HTML from a broken endpoint still
/// has no business in a card.
const MAX_EMAIL_ERROR_CHARS: usize = 800;

#[async_trait]
impl Notifier for EmailNotifier {
    async fn notify_incident(&self, notice: &IncidentNotice) -> Result<()> {
        let outgoing = TransactionalEmail {
            from: EmailAddress::new(
                self.delivery.from_address.clone(),
                self.delivery.from_name.clone(),
            ),
            to: EmailAddress::new(self.to.clone(), self.to.clone()),
            template: EmailTemplate::IncidentAlert(IncidentAlert {
                summary: notice.summary(),
                label: notice.label().to_string(),
                reason: notice.reason,
                severity: notice.severity,
                urgency: notice.urgency,
                origin: notice.origin,
                started_at: notice.started_at,
                ended_at: notice.ended_at,
                error_sample: notice
                    .error_sample
                    .as_deref()
                    .map(|e| crate::text::truncate_chars(e, MAX_EMAIL_ERROR_CHARS)),
                regions_down: notice.regions_down.clone(),
                regions_up: notice.regions_up.clone(),
                url: notice.url.clone(),
                ack_url: alert_link(notice, self.ack_link.as_deref()),
                resolve_url: alert_link(notice, self.resolve_link.as_deref()),
                note: notice.note.clone(),
                org_name: self.alert.org_name.clone(),
                stop_url: self.alert.stop_url.clone(),
            }),
        };
        self.delivery
            .sender
            .send(outgoing)
            .await
            .map(|_| ())
            .map_err(|e| {
                crate::error::AppError::Other(anyhow::anyhow!("email delivery failed: {e}"))
            })
    }
}
