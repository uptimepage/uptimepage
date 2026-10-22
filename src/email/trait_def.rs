use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hyper::body::Bytes;
use thiserror::Error;

use super::templates::incident_alert::IncidentAlert;
use super::templates::monitors_slowed::SlowedMonitor;
use super::templates::subscriber_maintenance::MaintenanceNotice;
use super::templates::{self, RenderedEmail};
use crate::domain::{Landing, Locale};
use crate::i18n::Tr;

pub type EmailResult<T> = Result<T, EmailError>;

/// Who is waiting on a mail, which decides how it queues for a send slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendClass {
    /// A person waiting on it, or an alert.
    Immediate,
    /// A sweep over many recipients that can wait seconds.
    Bulk,
}

impl SendClass {
    pub fn as_label(self) -> &'static str {
        match self {
            Self::Immediate => "immediate",
            Self::Bulk => "bulk",
        }
    }
}

/// Every provider answer keeps the `email provider rejected:` prefix it has
/// always logged with; the kind is what a retry decision reads.
#[derive(Debug, Error)]
pub enum EmailError {
    /// The provider's rate limit, or no send slot before the caller's
    /// deadline. The `"retry_after":N` fragment rides the escalation engine's
    /// vendor-hint path.
    #[error("email provider rejected: {detail} {{\"retry_after\":{secs}}}", secs = .retry_after.as_secs().max(1))]
    RateLimited {
        retry_after: Duration,
        detail: String,
    },
    /// The daily quota resets at `until`; the monthly one has no time we know.
    #[error("email provider rejected: {detail}")]
    QuotaExceeded {
        until: Option<DateTime<Utc>>,
        detail: String,
    },
    /// Our key or sending domain, not the recipient.
    #[error("email provider refused our account: {0}")]
    Refused(String),
    /// This message will never be accepted as it is.
    #[error("email provider rejected: {0}")]
    Rejected(String),
    /// A request with the same idempotency key is still being processed; only
    /// the same key with the same bytes may follow it.
    #[error("email provider rejected: {0}")]
    InProgress(String),
    /// The send budget had no slot before the caller's deadline. Nothing was
    /// sent; the fragment tells the escalation engine when to come back.
    #[error("email send deferred by the send budget {{\"retry_after\":{secs}}}", secs = .retry_after.as_secs().max(1))]
    Deferred { retry_after: Duration },
    /// This instance's send gate cannot reach the shared budget, and Bulk
    /// mail does not send on the local fallback. Nothing was sent.
    #[error("email send budget unavailable on this instance")]
    GateUnavailable,
    /// A 5xx, a timeout or a dropped connection: the provider may or may not
    /// have taken the mail.
    #[error("transport error: {0}")]
    Transport(String),
}

impl EmailError {
    /// Held back by pacing, ours or the provider's: nothing about this mail
    /// was refused, so it says nothing against sending it again.
    pub fn is_pacing(&self) -> bool {
        matches!(
            self,
            Self::RateLimited { .. } | Self::Deferred { .. } | Self::GateUnavailable
        )
    }

    pub fn as_label(&self) -> &'static str {
        match self {
            Self::RateLimited { .. } => "rate_limited",
            Self::QuotaExceeded { .. } => "quota_exceeded",
            Self::Refused(_) => "refused",
            Self::Rejected(_) => "rejected",
            Self::InProgress(_) => "in_progress",
            Self::Deferred { .. } => "deferred",
            Self::GateUnavailable => "gate_unavailable",
            Self::Transport(_) => "transport",
        }
    }
}

/// The most mails one batch request may carry.
pub const MAX_BATCH: usize = 100;

/// Every provider refuses the same batch sizes, so a fake cannot accept what
/// the real one would not.
pub(crate) fn check_batch_size(len: usize) -> EmailResult<()> {
    if (1..=MAX_BATCH).contains(&len) {
        Ok(())
    } else {
        Err(EmailError::Rejected(format!(
            "a batch takes 1 to {MAX_BATCH} mails, not {len}"
        )))
    }
}

/// One batch request as it goes on the wire, so a retry after an unknown
/// outcome sends the same bytes under the same idempotency key. The provider
/// answers a reused key with a changed body as a rejection, so a batch is
/// never rendered twice.
#[derive(Debug, Clone)]
pub struct PreparedBatch {
    key: String,
    body: Bytes,
    len: usize,
}

impl PreparedBatch {
    /// Also how a batch stored after an unknown outcome is rebuilt.
    pub fn from_parts(key: String, body: Bytes, len: usize) -> Self {
        Self { key, body, len }
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn body(&self) -> &Bytes {
        &self.body
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// The whole request failed; `batch` is handed back unchanged for a retry.
#[derive(Debug)]
pub struct BatchFailure {
    pub error: EmailError,
    pub batch: PreparedBatch,
}

/// One result per mail, in the order they were prepared.
pub type BatchResult = Result<Vec<EmailResult<MessageId>>, BatchFailure>;

#[async_trait]
pub trait EmailSender: Send + Sync {
    async fn send(&self, email: TransactionalEmail, class: SendClass) -> EmailResult<MessageId>;

    /// Renders up to [`MAX_BATCH`] mails into one request under `key`.
    fn prepare_batch(
        &self,
        emails: Vec<TransactionalEmail>,
        key: String,
    ) -> EmailResult<PreparedBatch>;

    /// Bulk only. Sends exactly the prepared bytes.
    async fn send_prepared(&self, batch: PreparedBatch) -> BatchResult;
}

#[derive(Debug, Clone)]
pub struct EmailAddress {
    pub address: String,
    pub name: String,
}

impl EmailAddress {
    pub fn new(address: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            address: address.into(),
            name: name.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TransactionalEmail {
    pub to: EmailAddress,
    pub from: EmailAddress,
    pub template: EmailTemplate,
}

#[derive(Debug, Clone)]
pub enum EmailTemplate {
    Invitation {
        org_name: String,
        inviter_display: String,
        accept_url: String,
        decline_url: String,
        expires_at: DateTime<Utc>,
    },
    /// Schema lands now so the enum is stable; flow is gated by
    /// `auth.enabled_methods` config until the verify endpoint is wired.
    MagicLink {
        url: String,
        /// Typed back in the tab that asked.
        code: String,
        expires_in_minutes: u32,
        ip_hint: Option<String>,
        /// A property of the deployment, never of the recipient: the same
        /// mail goes to every address either way.
        opens_accounts: bool,
    },
    /// Account-deletion notification. Restoring is a signed-in confirmation
    /// before the data is permanently purged on `scheduled_purge_at`.
    AccountDeletion {
        scheduled_purge_at: DateTime<Utc>,
    },
    /// So an account never comes back without its owner hearing about it.
    AccountRestored,
    IdentityLinked {
        provider_label: String,
        account_url: String,
    },
    IdentityUnlinked {
        provider_label: String,
        account_url: String,
    },
    /// Confirms an `email` notification channel's address before any alert
    /// is delivered to it. `org_name` names the org that added the address and
    /// `decline_url` lets the recipient refuse in one click.
    ChannelVerification {
        channel_name: String,
        verify_url: String,
        expires_hours: u32,
        org_name: Option<String>,
        decline_url: Option<String>,
    },
    /// An incident page delivered over email. Carries the incident's fields, not
    /// a rendered line: an inbox has room to lay out state, timing and regions.
    IncidentAlert(IncidentAlert),
    /// Confirms a public status-page subscription (double opt-in) before any
    /// update is delivered to the address.
    SubscriberConfirm {
        locale: Locale,
        page_name: String,
        confirm_url: String,
        expires_hours: u32,
        unsubscribe_url: String,
    },
    /// Signs a visitor in to a private status page: a code for the tab that
    /// asked and a link for another device, in the page's language.
    PageAccessCode {
        locale: Locale,
        page_name: String,
        url: String,
        code: String,
        expires_in_minutes: u32,
    },
    /// A public incident update delivered to a confirmed subscriber.
    SubscriberIncident {
        locale: Locale,
        page_name: String,
        incident_title: String,
        phase: String,
        message: String,
        incident_url: String,
        unsubscribe_url: String,
    },
    /// Told once to the owner. A pending heartbeat alerts nobody by design, so
    /// without this a monitor somebody half-wired stays quiet forever.
    HeartbeatNeverPinged {
        monitor_name: String,
        waiting_secs: i64,
        monitor_url: Option<String>,
        docs_url: Option<String>,
        org_name: Option<String>,
    },
    /// Told once per slow episode, to one person per monitor, listing every
    /// monitor slowed for them in that pass.
    MonitorsSlowed {
        monitors: Vec<SlowedMonitor>,
        docs_url: Option<String>,
    },
    /// An in-app help request relayed to the operator's support address.
    /// Everything but `message` and `page_url` is server-derived, and
    /// `from_email` becomes the Reply-To so an answer goes straight back.
    SupportRequest {
        request_id: String,
        topic: String,
        message: String,
        from_email: String,
        org_slug: String,
        org_id: String,
        plan: String,
        page_url: Option<String>,
        app_version: String,
    },
    /// Sent to the org's owners once per run of failures, not per swallowed
    /// alert.
    ChannelFailing {
        channel_name: String,
        transport: String,
        org_name: Option<String>,
        consecutive_failures: i32,
        failing_secs: Option<i64>,
        last_error: Option<String>,
        channel_url: Option<String>,
    },
    /// A maintenance-window announcement, completion or posted update for a
    /// confirmed subscriber.
    SubscriberMaintenance {
        locale: Locale,
        page_name: String,
        title: String,
        notice: MaintenanceNotice,
        starts_at: DateTime<Utc>,
        ends_at: DateTime<Utc>,
        page_url: String,
        unsubscribe_url: String,
    },
    /// The account owner's payment failed; full service continues until
    /// `retry_by`. Sent again at each reminder stage while it stays unpaid.
    PaymentFailed {
        plan_name: String,
        retry_by: DateTime<Utc>,
        fix_url: String,
    },
    PaymentRecovered {
        plan_name: String,
    },
    /// A move to a smaller plan is booked for `at`; what will not fit is
    /// counted so the owner can pick what to keep before then.
    DowngradeScheduled {
        plan_name: String,
        at: DateTime<Utc>,
        over_monitors: i64,
        over_pages: i64,
        keep_url: String,
    },
    /// The account is now on `plan_name`, with `held_monitors` and
    /// `held_pages` parked, not deleted; `landing` says why.
    DowngradeApplied {
        plan_name: String,
        landing: Landing,
        held_monitors: usize,
        held_pages: usize,
        keep_url: String,
    },
    /// The cancel is booked: paid service ends at `ends_at`, and the account
    /// falls to `plan_name`.
    SubscriptionCanceled {
        plan_name: String,
        ends_at: DateTime<Utc>,
    },
}

impl EmailTemplate {
    pub fn render(&self, site_name: &str) -> RenderedEmail {
        let mut rendered = self.render_template(site_name);
        // Every subject carries operator- or customer-authored text, and a
        // control character in a mail header starts a new header. One owner,
        // so no template can forget it.
        rendered.subject = templates::single_line(&rendered.subject);
        rendered
    }

    fn render_template(&self, site_name: &str) -> RenderedEmail {
        match self {
            EmailTemplate::Invitation {
                org_name,
                inviter_display,
                accept_url,
                decline_url,
                expires_at,
            } => templates::invitation::render(
                site_name,
                org_name,
                inviter_display,
                accept_url,
                decline_url,
                *expires_at,
            ),
            EmailTemplate::MagicLink {
                url,
                code,
                expires_in_minutes,
                ip_hint,
                opens_accounts,
            } => templates::magic_link::render(
                site_name,
                url,
                code,
                *expires_in_minutes,
                ip_hint.as_deref(),
                *opens_accounts,
            ),
            EmailTemplate::AccountDeletion { scheduled_purge_at } => {
                templates::account_deletion::render(site_name, *scheduled_purge_at)
            }
            EmailTemplate::AccountRestored => templates::account_restored::render(site_name),
            EmailTemplate::IdentityLinked {
                provider_label,
                account_url,
            } => templates::identity_linked::render(site_name, provider_label, account_url),
            EmailTemplate::IdentityUnlinked {
                provider_label,
                account_url,
            } => templates::identity_unlinked::render(site_name, provider_label, account_url),
            EmailTemplate::ChannelVerification {
                channel_name,
                verify_url,
                expires_hours,
                org_name,
                decline_url,
            } => templates::channel_verification::render(
                site_name,
                channel_name,
                verify_url,
                *expires_hours,
                org_name.as_deref(),
                decline_url.as_deref(),
            ),
            EmailTemplate::ChannelFailing {
                channel_name,
                transport,
                org_name,
                consecutive_failures,
                failing_secs,
                last_error,
                channel_url,
            } => templates::channel_failing::render(
                site_name,
                &templates::channel_failing::FailingChannel {
                    channel_name,
                    transport,
                    org_name: org_name.as_deref(),
                    consecutive_failures: *consecutive_failures,
                    failing_secs: *failing_secs,
                    last_error: last_error.as_deref(),
                    channel_url: channel_url.as_deref(),
                },
            ),
            EmailTemplate::HeartbeatNeverPinged {
                monitor_name,
                waiting_secs,
                monitor_url,
                docs_url,
                org_name,
            } => templates::heartbeat_never_pinged::render(
                site_name,
                &templates::heartbeat_never_pinged::UnwiredHeartbeat {
                    monitor_name,
                    waiting_secs: *waiting_secs,
                    monitor_url: monitor_url.as_deref(),
                    docs_url: docs_url.as_deref(),
                    org_name: org_name.as_deref(),
                },
            ),
            EmailTemplate::MonitorsSlowed { monitors, docs_url } => {
                templates::monitors_slowed::render(site_name, monitors, docs_url.as_deref())
            }
            EmailTemplate::IncidentAlert(alert) => {
                templates::incident_alert::render(site_name, alert)
            }
            EmailTemplate::SubscriberConfirm {
                locale,
                page_name,
                confirm_url,
                expires_hours,
                unsubscribe_url,
            } => templates::subscriber_confirm::render(
                Tr::new(*locale),
                site_name,
                page_name,
                confirm_url,
                *expires_hours,
                unsubscribe_url,
            ),
            EmailTemplate::PageAccessCode {
                locale,
                page_name,
                url,
                code,
                expires_in_minutes,
            } => templates::page_access_code::render(
                Tr::new(*locale),
                site_name,
                page_name,
                url,
                code,
                *expires_in_minutes,
            ),
            EmailTemplate::SubscriberIncident {
                locale,
                page_name,
                incident_title,
                phase,
                message,
                incident_url,
                unsubscribe_url,
            } => templates::subscriber_incident::render(
                Tr::new(*locale),
                page_name,
                incident_title,
                phase,
                message,
                incident_url,
                unsubscribe_url,
            ),
            EmailTemplate::SupportRequest {
                request_id,
                topic,
                message,
                from_email,
                org_slug,
                org_id,
                plan,
                page_url,
                app_version,
            } => templates::support_request::render(
                site_name,
                &templates::support_request::SupportContext {
                    request_id,
                    topic,
                    message,
                    from_email,
                    org_slug,
                    org_id,
                    plan,
                    page_url: page_url.as_deref(),
                    app_version,
                },
            ),
            EmailTemplate::SubscriberMaintenance {
                locale,
                page_name,
                title,
                notice,
                starts_at,
                ends_at,
                page_url,
                unsubscribe_url,
            } => templates::subscriber_maintenance::render(
                Tr::new(*locale),
                page_name,
                title,
                notice,
                *starts_at,
                *ends_at,
                page_url,
                unsubscribe_url,
            ),
            EmailTemplate::PaymentFailed {
                plan_name,
                retry_by,
                fix_url,
            } => templates::billing::payment_failed(site_name, plan_name, *retry_by, fix_url),
            EmailTemplate::PaymentRecovered { plan_name } => {
                templates::billing::payment_recovered(site_name, plan_name)
            }
            EmailTemplate::DowngradeScheduled {
                plan_name,
                at,
                over_monitors,
                over_pages,
                keep_url,
            } => templates::billing::downgrade_scheduled(
                site_name,
                plan_name,
                *at,
                templates::billing::Excess {
                    monitors: *over_monitors,
                    pages: *over_pages,
                },
                keep_url,
            ),
            EmailTemplate::DowngradeApplied {
                plan_name,
                landing,
                held_monitors,
                held_pages,
                keep_url,
            } => templates::billing::downgrade_applied(
                site_name,
                plan_name,
                *landing,
                templates::billing::Excess {
                    monitors: *held_monitors as i64,
                    pages: *held_pages as i64,
                },
                keep_url,
            ),
            EmailTemplate::SubscriptionCanceled { plan_name, ends_at } => {
                templates::billing::subscription_canceled(site_name, plan_name, *ends_at)
            }
        }
    }

    /// Action URL surfaced by log-only sender for copy-paste-driven dev flows.
    pub fn primary_url(&self) -> Option<&str> {
        match self {
            EmailTemplate::Invitation { accept_url, .. } => Some(accept_url),
            EmailTemplate::MagicLink { url, .. } => Some(url),
            EmailTemplate::AccountDeletion { .. } | EmailTemplate::AccountRestored => None,
            EmailTemplate::IdentityLinked { account_url, .. }
            | EmailTemplate::IdentityUnlinked { account_url, .. } => Some(account_url),
            EmailTemplate::ChannelVerification { verify_url, .. } => Some(verify_url),
            EmailTemplate::ChannelFailing { channel_url, .. } => channel_url.as_deref(),
            EmailTemplate::HeartbeatNeverPinged { monitor_url, .. } => monitor_url.as_deref(),
            EmailTemplate::MonitorsSlowed { monitors, .. } => {
                monitors.first().and_then(|m| m.url.as_deref())
            }
            EmailTemplate::IncidentAlert(_) => None,
            EmailTemplate::SubscriberConfirm { confirm_url, .. } => Some(confirm_url),
            EmailTemplate::PageAccessCode { url, .. } => Some(url),
            EmailTemplate::SubscriberIncident { incident_url, .. } => Some(incident_url),
            EmailTemplate::SubscriberMaintenance { page_url, .. } => Some(page_url),
            EmailTemplate::SupportRequest { .. } => None,
            EmailTemplate::PaymentFailed { fix_url, .. } => Some(fix_url),
            EmailTemplate::DowngradeScheduled { keep_url, .. }
            | EmailTemplate::DowngradeApplied { keep_url, .. } => Some(keep_url),
            EmailTemplate::PaymentRecovered { .. } | EmailTemplate::SubscriptionCanceled { .. } => {
                None
            }
        }
    }

    /// Sent by the platform, answered to the customer who wrote it.
    pub fn reply_to(&self) -> Option<&str> {
        match self {
            EmailTemplate::SupportRequest { from_email, .. } => Some(from_email),
            _ => None,
        }
    }

    /// Lets a filter rule match without parsing a subject. Names are
    /// unprefixed per RFC 6648, which deprecates `X-` for new headers.
    pub fn custom_headers(&self) -> Vec<(&'static str, String)> {
        match self {
            EmailTemplate::SupportRequest {
                request_id,
                topic,
                org_slug,
                ..
            } => vec![
                ("Uptimepage-Request-Id", request_id.clone()),
                ("Uptimepage-Topic", topic.clone()),
                ("Uptimepage-Org", org_slug.clone()),
            ],
            _ => Vec::new(),
        }
    }

    /// One-click unsubscribe URL surfaced as RFC 8058 List-Unsubscribe headers
    /// by the transport. Subscriber mail carries its unsubscribe link; the
    /// still-unverified verification mail carries a one-click decline so a
    /// non-consenting inbox can shut it down at the mail-client level. Incident
    /// alerts deliberately do NOT: one-click is auto-actuated by mail gateways
    /// and native clients, and silently disabling a live paging channel is a
    /// worse failure than a missed unsubscribe. Their stop link lives in the
    /// body, behind a confirmation, so disabling stays a deliberate act.
    pub fn list_unsubscribe_url(&self) -> Option<&str> {
        match self {
            EmailTemplate::SubscriberIncident {
                unsubscribe_url, ..
            }
            | EmailTemplate::SubscriberMaintenance {
                unsubscribe_url, ..
            }
            | EmailTemplate::SubscriberConfirm {
                unsubscribe_url, ..
            } => Some(unsubscribe_url),
            EmailTemplate::ChannelVerification { decline_url, .. } => decline_url.as_deref(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageId(pub String);

impl fmt::Display for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_support() -> EmailTemplate {
        EmailTemplate::SupportRequest {
            request_id: "0198f0e1-1111-7222-8333-a1b2c3d4e5f6".into(),
            topic: "bug".into(),
            message: "checks are flapping".into(),
            from_email: "jane@acme.test".into(),
            org_slug: "acme".into(),
            org_id: "0198f0e1-0000-7000-8000-000000000000".into(),
            plan: "founding".into(),
            page_url: None,
            app_version: "1.0.0".into(),
        }
    }

    #[test]
    fn support_request_answers_to_the_customer_not_the_platform() {
        assert_eq!(sample_support().reply_to(), Some("jane@acme.test"));
    }

    fn sample_alert(stop_url: Option<&str>) -> EmailTemplate {
        EmailTemplate::IncidentAlert(IncidentAlert {
            summary: "api — major incident OPEN".into(),
            label: "api".into(),
            reason: crate::domain::NotificationReason::Opened,
            severity: crate::domain::IncidentSeverity::Major,
            urgency: crate::domain::IncidentUrgency::High,
            origin: crate::domain::IncidentOrigin::Monitor,
            started_at: Utc::now(),
            ended_at: None,
            error_sample: None,
            regions_down: Vec::new(),
            regions_up: Vec::new(),
            url: None,
            ack_url: None,
            resolve_url: None,
            note: None,
            org_name: Some("Acme".into()),
            stop_url: stop_url.map(str::to_string),
        })
    }

    #[test]
    fn only_the_support_relay_sets_a_reply_to() {
        let alert = sample_alert(None);
        assert_eq!(alert.reply_to(), None);
        assert_eq!(EmailTemplate::AccountRestored.reply_to(), None);
    }

    #[test]
    fn support_request_carries_filterable_headers() {
        let headers = sample_support().custom_headers();
        assert_eq!(
            headers,
            vec![
                (
                    "Uptimepage-Request-Id",
                    "0198f0e1-1111-7222-8333-a1b2c3d4e5f6".to_string()
                ),
                ("Uptimepage-Topic", "bug".to_string()),
                ("Uptimepage-Org", "acme".to_string()),
            ],
            "unprefixed per RFC 6648, so a filter rule need not parse a subject"
        );
    }

    #[test]
    fn other_templates_add_no_application_headers() {
        assert!(EmailTemplate::AccountRestored.custom_headers().is_empty());
    }

    #[test]
    fn support_request_offers_no_unsubscribe_or_action_url() {
        // It is operator mail, not subscriber mail: a List-Unsubscribe header
        // here would let a mail gateway silently mute the help channel.
        let support = sample_support();
        assert_eq!(support.list_unsubscribe_url(), None);
        assert_eq!(support.primary_url(), None);
    }

    #[test]
    fn incident_alert_withholds_one_click_but_verification_offers_it() {
        let alert = sample_alert(Some("https://app/alert-channel/stop?c=1&t=2"));
        assert_eq!(alert.list_unsubscribe_url(), None);

        let verify = EmailTemplate::ChannelVerification {
            channel_name: "c".into(),
            verify_url: "https://app/verify".into(),
            expires_hours: 24,
            org_name: None,
            decline_url: Some("https://app/alert-channel/stop?c=3&t=4".into()),
        };
        assert_eq!(
            verify.list_unsubscribe_url(),
            Some("https://app/alert-channel/stop?c=3&t=4")
        );
    }

    #[test]
    fn subscriber_confirm_offers_one_click_unsubscribe() {
        let confirm = EmailTemplate::SubscriberConfirm {
            locale: Locale::En,
            page_name: "Acme".into(),
            confirm_url: "https://acme/subscribe/confirm?token=x".into(),
            expires_hours: 24,
            unsubscribe_url: "https://acme/subscribe/unsubscribe?s=1&t=2".into(),
        };
        assert_eq!(
            confirm.list_unsubscribe_url(),
            Some("https://acme/subscribe/unsubscribe?s=1&t=2")
        );
    }
}
