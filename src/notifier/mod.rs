pub mod card;
pub mod discord;
pub mod email;
pub mod event;
pub mod google_chat;
pub mod gotify;
pub mod mattermost;
pub mod msteams;
pub mod ntfy;
pub mod pagerduty;
pub mod pushover;
pub mod slack;
pub mod sms;
pub mod telegram;
pub mod webhook;
pub mod whatsapp;

use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::ChannelConfig;
use crate::error::Result;
use crate::http_outbound::OutboundHttpClient;
use crate::notifier::discord::DiscordNotifier;
use crate::notifier::email::EmailNotifier;
use crate::notifier::event::IncidentNotice;
use crate::notifier::google_chat::GoogleChatNotifier;
use crate::notifier::gotify::GotifyNotifier;
use crate::notifier::mattermost::MattermostNotifier;
use crate::notifier::msteams::MsTeamsNotifier;
use crate::notifier::ntfy::NtfyNotifier;
use crate::notifier::pagerduty::PagerDutyNotifier;
use crate::notifier::pushover::PushoverNotifier;
use crate::notifier::slack::SlackNotifier;
use crate::notifier::sms::SmsNotifier;
use crate::notifier::telegram::TelegramNotifier;
use crate::notifier::webhook::WebhookNotifier;
use crate::notifier::whatsapp::WhatsAppNotifier;

pub use crate::notifier::email::{EmailAlert, EmailDelivery, email_alert_for};
pub use crate::notifier::ntfy::PushAck;

#[async_trait]
pub trait Notifier: Send + Sync {
    /// Page an incident lifecycle event (opened/resolved/reopened/escalated).
    async fn notify_incident(&self, notice: &IncidentNotice) -> Result<()>;

    /// Provider receipt captured by the preceding successful send, when the
    /// transport returns one to track for acknowledgement/cancel (Pushover
    /// emergency). `None` for every other transport. A notifier instance
    /// serves a single send, so the receipt belongs to that send.
    fn taken_receipt(&self) -> Option<String> {
        None
    }

    /// A notifier instance serves a single send, so the move belongs to it.
    fn taken_chat_migration(&self) -> Option<ChatMigration> {
        None
    }
}

/// String-scanned because transports flatten the vendor body into the error
/// text. Sign allowed, fraction dropped.
pub(crate) fn json_int_field(error: &str, key: &str) -> Option<i64> {
    let needle = format!("\"{key}\":");
    let rest = error[error.find(&needle)? + needle.len()..].trim_start();
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '-'))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMigration {
    pub from: String,
    pub to: String,
}

/// Send, then persist any chat move the send followed. Best-effort on the
/// second half: the page already landed, and an operator who re-pointed the
/// channel meanwhile wins the compare-and-swap. A linked chat moves for every
/// org that shares it, the way the bot's webhook would move it.
pub async fn notify_following_moves(
    channels: &dyn crate::storage::NotificationChannelStore,
    org: crate::domain::OrgId,
    channel: &crate::domain::NotificationChannel,
    notifier: &dyn Notifier,
    notice: &IncidentNotice,
) -> Result<()> {
    let sent = notifier.notify_incident(notice).await;
    let Some(moved) = notifier.taken_chat_migration() else {
        return sent;
    };
    let followed = if channel.kind == crate::domain::ChannelKind::TelegramApp {
        channels
            .follow_linked_chat_migration(&moved.from, &moved.to)
            .await
    } else {
        channels
            .follow_chat_migration(org, channel.id, &moved.from, &moved.to)
            .await
            .map(u64::from)
    };
    match followed {
        Ok(0) => {}
        Ok(n) => tracing::info!(
            org_id = %org.0,
            channel_id = %channel.id,
            channels = n,
            from = %moved.from,
            to = %moved.to,
            "notification channel followed its telegram chat migration"
        ),
        Err(err) => tracing::warn!(
            org_id = %org.0,
            channel_id = %channel.id,
            error = %err,
            "telegram chat migration not persisted"
        ),
    }
    sent
}

/// Central-bot delivery context for one factory call: operator token plus
/// the process-wide send budget.
#[derive(Clone, Copy)]
pub struct CentralTelegram<'a> {
    pub bot_token: &'a str,
    pub budget: &'a Arc<crate::telegram::TelegramSendBudget>,
}

/// Owned counterpart held by long-lived senders (the escalation engine).
/// The budget Arc must be the process-wide instance — a second instance
/// would double the bot's rate budget.
pub struct CentralBotDelivery {
    pub token: secrecy::SecretString,
    pub budget: Arc<crate::telegram::TelegramSendBudget>,
}

impl CentralBotDelivery {
    pub fn as_central(&self) -> CentralTelegram<'_> {
        use secrecy::ExposeSecret;
        CentralTelegram {
            bot_token: self.token.expose_secret(),
            budget: &self.budget,
        }
    }
}

/// Delivery-side factory: map a stored [`ChannelConfig`] to its transport.
/// The full add-a-transport checklist lives on
/// `crate::domain::notification_channel`. URLs were validated `https` on
/// channel create; re-parsing here is a defence-in-depth guard, not the
/// primary check.
///
/// Linked (`telegram_app`) channels deliver with the operator token and the
/// shared send budget in `central`; linked (`whatsapp_app`) channels with
/// the operator Cloud API credentials in `whatsapp`. `None` (operator
/// surface absent) fails their build with a clear error instead of a
/// broken send.
pub fn build_notifier(
    cfg: &ChannelConfig,
    http: &OutboundHttpClient,
    central: Option<CentralTelegram<'_>>,
    whatsapp: Option<&crate::config::WhatsAppAppBotConfig>,
    email: Option<&EmailDelivery>,
    email_alert: Option<EmailAlert>,
    push_ack: Option<PushAck>,
) -> Result<Arc<dyn Notifier>> {
    let parse = |s: &str| -> Result<url::Url> {
        s.parse::<url::Url>().map_err(|e| {
            crate::error::AppError::bad_request(
                crate::api::codes::INVALID_CONFIG,
                format!("notification channel URL is invalid: {e}"),
            )
        })
    };
    Ok(match cfg {
        ChannelConfig::Webhook(c) => Arc::new(WebhookNotifier::new(
            http.clone(),
            parse(&c.url)?,
            c.headers.clone(),
            c.secret.clone(),
        )) as Arc<dyn Notifier>,
        ChannelConfig::Slack(c) => Arc::new(SlackNotifier::new(
            http.clone(),
            parse(&c.webhook_url)?,
            c.mention_markup(),
        )) as Arc<dyn Notifier>,
        ChannelConfig::Telegram(c) => Arc::new(TelegramNotifier::new(
            http.clone(),
            &c.bot_token,
            c.chat_id.clone(),
        )?) as Arc<dyn Notifier>,
        ChannelConfig::TelegramApp(c) => {
            let central = central
                .filter(|c| !c.bot_token.trim().is_empty())
                .ok_or_else(|| {
                    crate::error::AppError::bad_request(
                        crate::api::codes::INVALID_CONFIG,
                        "linked telegram channels need the central bot, which is not configured \
                         on this deployment",
                    )
                })?;
            Arc::new(
                TelegramNotifier::new(http.clone(), central.bot_token.trim(), c.chat_id.clone())?
                    .with_budget(central.budget.clone()),
            ) as Arc<dyn Notifier>
        }
        ChannelConfig::WhatsApp(c) => {
            Arc::new(WhatsAppNotifier::new(http.clone(), c)?) as Arc<dyn Notifier>
        }
        ChannelConfig::WhatsAppApp(c) => {
            let wa = whatsapp.filter(|w| w.enabled()).ok_or_else(|| {
                crate::error::AppError::bad_request(
                    crate::api::codes::INVALID_CONFIG,
                    "linked whatsapp channels need the operator whatsapp number, which is not \
                     configured on this deployment",
                )
            })?;
            use secrecy::ExposeSecret;
            let synthesized = crate::domain::WhatsAppConfig {
                access_token: wa.access_token.expose_secret().trim().to_string(),
                phone_number_id: wa.phone_number_id.clone(),
                to: c.phone.clone(),
                template_name: wa.template_name.clone(),
                language_code: (!wa.language_code.is_empty()).then(|| wa.language_code.clone()),
            };
            Arc::new(WhatsAppNotifier::new(http.clone(), &synthesized)?) as Arc<dyn Notifier>
        }
        ChannelConfig::Discord(c) => Arc::new(DiscordNotifier::new(
            http.clone(),
            parse(&c.webhook_url)?,
            c.mention_targets(),
        )) as Arc<dyn Notifier>,
        ChannelConfig::MsTeams(c) => {
            Arc::new(MsTeamsNotifier::new(http.clone(), parse(&c.webhook_url)?))
                as Arc<dyn Notifier>
        }
        ChannelConfig::GoogleChat(c) => Arc::new(GoogleChatNotifier::new(
            http.clone(),
            parse(&c.webhook_url)?,
        )) as Arc<dyn Notifier>,
        ChannelConfig::Email(c) => {
            let email = email.ok_or_else(|| {
                crate::error::AppError::bad_request(
                    crate::api::codes::INVALID_CONFIG,
                    "email delivery is not configured on this deployment",
                )
            })?;
            Arc::new(EmailNotifier::new(
                email,
                &c.to,
                email_alert.unwrap_or_default(),
            )) as Arc<dyn Notifier>
        }
        ChannelConfig::PagerDuty(c) => {
            Arc::new(PagerDutyNotifier::new(http.clone(), c.routing_key.clone()))
                as Arc<dyn Notifier>
        }
        ChannelConfig::Ntfy(c) => Arc::new(NtfyNotifier::new(
            http.clone(),
            parse(&c.server_url)?,
            c.topic.clone(),
            c.access_token.clone(),
            push_ack,
        )) as Arc<dyn Notifier>,
        ChannelConfig::Gotify(c) => Arc::new(GotifyNotifier::new(
            http.clone(),
            parse(&c.publish_url())?,
            c.token.clone(),
        )) as Arc<dyn Notifier>,
        ChannelConfig::Pushover(c) => Arc::new(PushoverNotifier::new(
            http.clone(),
            c.token.clone(),
            c.user.clone(),
            c.device.clone(),
            c.emergency,
        )) as Arc<dyn Notifier>,
        ChannelConfig::Sms(c) => Arc::new(SmsNotifier::new(http.clone(), c)?) as Arc<dyn Notifier>,
        ChannelConfig::Mattermost(c) => Arc::new(MattermostNotifier::new(
            http.clone(),
            parse(&c.webhook_url)?,
            c.mention_markup(),
        )) as Arc<dyn Notifier>,
    })
}

pub(crate) use crate::text::{truncate_bytes, truncate_chars};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        NewNotificationChannel, OrgId, TelegramAppConfig, TelegramConfig, WriteSource,
    };
    use crate::storage::{InMemoryNotificationChannelStore, NotificationChannelStore};

    struct Moved(parking_lot::Mutex<Option<ChatMigration>>);

    #[async_trait]
    impl Notifier for Moved {
        async fn notify_incident(&self, _: &IncidentNotice) -> Result<()> {
            Ok(())
        }

        fn taken_chat_migration(&self) -> Option<ChatMigration> {
            self.0.lock().take()
        }
    }

    fn moved(from: &str, to: &str) -> Moved {
        Moved(parking_lot::Mutex::new(Some(ChatMigration {
            from: from.into(),
            to: to.into(),
        })))
    }

    async fn create(
        store: &InMemoryNotificationChannelStore,
        org: OrgId,
        config: ChannelConfig,
    ) -> crate::domain::NotificationChannel {
        store
            .create(
                org,
                NewNotificationChannel {
                    name: "ops".into(),
                    config,
                    enabled: true,
                    auto_bind_tags: Vec::new(),
                },
                WriteSource::Ui,
                10,
                None,
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_followed_move_lands_on_the_channel_once() {
        let store = InMemoryNotificationChannelStore::new();
        let org = OrgId(uuid::Uuid::now_v7());
        let ch = create(
            &store,
            org,
            ChannelConfig::Telegram(TelegramConfig {
                bot_token: "123:abc".into(),
                chat_id: "-5".into(),
            }),
        )
        .await;
        let moved = moved("-5", "-1005");
        let notice = card::tests::notice(crate::domain::NotificationReason::Opened);

        notify_following_moves(&store, org, &ch, &moved, &notice)
            .await
            .unwrap();
        notify_following_moves(&store, org, &ch, &moved, &notice)
            .await
            .unwrap();

        let got = store.get(org, ch.id).await.unwrap().unwrap();
        let ChannelConfig::Telegram(c) = got.config else {
            unreachable!()
        };
        assert_eq!(c.chat_id, "-1005");
    }

    #[tokio::test]
    async fn a_linked_chat_moves_for_every_org_that_shares_it() {
        let store = InMemoryNotificationChannelStore::new();
        let (a, b) = (OrgId(uuid::Uuid::now_v7()), OrgId(uuid::Uuid::now_v7()));
        let linked = || {
            ChannelConfig::TelegramApp(TelegramAppConfig {
                chat_id: "-5".into(),
                chat_title: None,
            })
        };
        let ch_a = create(&store, a, linked()).await;
        let ch_b = create(&store, b, linked()).await;
        let notice = card::tests::notice(crate::domain::NotificationReason::Opened);

        notify_following_moves(&store, a, &ch_a, &moved("-5", "-1005"), &notice)
            .await
            .unwrap();

        for (org, id) in [(a, ch_a.id), (b, ch_b.id)] {
            let got = store.get(org, id).await.unwrap().unwrap();
            assert_eq!(got.config.lifecycle_ref(), Some("-1005"));
        }
    }
}
