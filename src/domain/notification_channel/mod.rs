//! Per-org notification channels. A channel is a named, typed delivery
//! destination (Slack hook, generic webhook, Telegram bot, …) that targets
//! bind to for Down/Recovered alerts.
//!
//! Each transport is a self-contained module implementing
//! [`TransportConfig`]; [`ChannelConfig`] only delegates. The full
//! add-a-transport checklist:
//!
//! 1. config module here + [`TransportConfig`] impl;
//! 2. [`ChannelKind`] variant, [`ChannelKind::ALL`], `as_db_str`, and the
//!    Postgres `kind` CHECK constraint (the enum-drift test compares them);
//! 3. [`ChannelConfig`] variant + `with_transport!` arm — the compiler then
//!    points at the remaining match sites (`build_notifier`, the form
//!    prefill);
//! 4. a `Notifier` impl in `crate::notifier` and its factory arm;
//! 5. the form UI: template variant panel + type card + JS config builder.
//!
//! The whole config blob is sealed at rest by the credentials KEK at the
//! storage edge, and secrets are never echoed back by the API — see
//! [`ChannelConfig::redacted`].

mod discord;
mod email;
mod google_chat;
mod gotify;
mod mattermost;
mod mention;
mod msteams;
mod ntfy;
mod pagerduty;
mod pushover;
mod slack;
mod sms;
mod telegram;
mod telegram_app;
mod transport;
mod webhook;
mod whatsapp;
mod whatsapp_app;

pub use discord::{DiscordConfig, DiscordMention};
pub use email::EmailConfig;
pub use google_chat::GoogleChatConfig;
pub use gotify::GotifyConfig;
pub use mattermost::MattermostConfig;
pub use msteams::MsTeamsConfig;
pub use ntfy::NtfyConfig;
pub use pagerduty::PagerDutyConfig;
pub use pushover::PushoverConfig;
pub use slack::SlackConfig;
pub use sms::SmsConfig;
pub use telegram::TelegramConfig;
pub use telegram_app::TelegramAppConfig;
pub use transport::TransportConfig;
pub use webhook::WebhookConfig;
pub use whatsapp::WhatsAppConfig;
pub use whatsapp_app::WhatsAppAppConfig;

use super::WriteSource;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Webhook,
    Slack,
    Telegram,
    #[serde(rename = "telegram_app")]
    TelegramApp,
    #[serde(rename = "whatsapp")]
    WhatsApp,
    #[serde(rename = "whatsapp_app")]
    WhatsAppApp,
    Discord,
    #[serde(rename = "msteams")]
    MsTeams,
    GoogleChat,
    Email,
    #[serde(rename = "pagerduty")]
    PagerDuty,
    Ntfy,
    Gotify,
    Pushover,
    Sms,
    Mattermost,
}

impl ChannelKind {
    /// Every variant in declaration order. Used by the enum-drift integration
    /// test to compare against the live Postgres CHECK constraint on
    /// `notification_channels.kind`; keep in lockstep with the enum body.
    pub const ALL: &'static [Self] = &[
        Self::Webhook,
        Self::Slack,
        Self::Telegram,
        Self::TelegramApp,
        Self::WhatsApp,
        Self::WhatsAppApp,
        Self::Discord,
        Self::MsTeams,
        Self::GoogleChat,
        Self::Email,
        Self::PagerDuty,
        Self::Ntfy,
        Self::Gotify,
        Self::Pushover,
        Self::Sms,
        Self::Mattermost,
    ];

    /// Stable string used in the Postgres `kind` CHECK constraint and the
    /// JSON wire form.
    pub const fn as_db_str(self) -> &'static str {
        match self {
            Self::Webhook => "webhook",
            Self::Slack => "slack",
            Self::Telegram => "telegram",
            Self::TelegramApp => "telegram_app",
            Self::WhatsApp => "whatsapp",
            Self::WhatsAppApp => "whatsapp_app",
            Self::Discord => "discord",
            Self::MsTeams => "msteams",
            Self::GoogleChat => "google_chat",
            Self::Email => "email",
            Self::PagerDuty => "pagerduty",
            Self::Ntfy => "ntfy",
            Self::Gotify => "gotify",
            Self::Pushover => "pushover",
            Self::Sms => "sms",
            Self::Mattermost => "mattermost",
        }
    }
}

/// Transport config, `type`-tagged on the wire (newtype variants flatten the
/// inner struct's fields into the same JSON object). Stored sealed at rest;
/// the in-memory domain value is always plaintext.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChannelConfig {
    Webhook(WebhookConfig),
    Slack(SlackConfig),
    Telegram(TelegramConfig),
    #[serde(rename = "telegram_app")]
    TelegramApp(TelegramAppConfig),
    #[serde(rename = "whatsapp")]
    WhatsApp(WhatsAppConfig),
    #[serde(rename = "whatsapp_app")]
    WhatsAppApp(WhatsAppAppConfig),
    Discord(DiscordConfig),
    #[serde(rename = "msteams")]
    MsTeams(MsTeamsConfig),
    GoogleChat(GoogleChatConfig),
    Email(EmailConfig),
    #[serde(rename = "pagerduty")]
    PagerDuty(PagerDutyConfig),
    Ntfy(NtfyConfig),
    Gotify(GotifyConfig),
    Pushover(PushoverConfig),
    Sms(SmsConfig),
    Mattermost(MattermostConfig),
}

/// Apply `$body` to the inner [`TransportConfig`] of any variant. The one
/// place that has to enumerate the variants for delegation.
macro_rules! with_transport {
    ($self:expr, |$c:ident| $body:expr) => {
        match $self {
            ChannelConfig::Webhook($c) => $body,
            ChannelConfig::Slack($c) => $body,
            ChannelConfig::Telegram($c) => $body,
            ChannelConfig::TelegramApp($c) => $body,
            ChannelConfig::WhatsApp($c) => $body,
            ChannelConfig::WhatsAppApp($c) => $body,
            ChannelConfig::Discord($c) => $body,
            ChannelConfig::MsTeams($c) => $body,
            ChannelConfig::GoogleChat($c) => $body,
            ChannelConfig::Email($c) => $body,
            ChannelConfig::PagerDuty($c) => $body,
            ChannelConfig::Ntfy($c) => $body,
            ChannelConfig::Gotify($c) => $body,
            ChannelConfig::Pushover($c) => $body,
            ChannelConfig::Sms($c) => $body,
            ChannelConfig::Mattermost($c) => $body,
        }
    };
}

impl ChannelConfig {
    pub fn kind(&self) -> ChannelKind {
        with_transport!(self, |c| c.kind())
    }

    /// Overwrite every secret-bearing field with the redaction mask in
    /// place. Single source of the masking policy: [`Self::redacted`] and
    /// the API `RedactInPlace` impl both route through here.
    pub fn redact_in_place(&mut self) {
        with_transport!(self, |c| c.redact_in_place())
    }

    /// JSON copy with every secret-bearing field masked, for API responses
    /// and the edit form.
    pub fn redacted(&self) -> serde_json::Value {
        let mut c = self.clone();
        c.redact_in_place();
        serde_json::to_value(&c).unwrap_or(serde_json::Value::Null)
    }

    pub fn has_redaction_sentinel(&self) -> bool {
        with_transport!(self, |c| c.has_redaction_sentinel())
    }

    pub fn validate(&self) -> Result<(), String> {
        with_transport!(self, |c| c.validate())
    }

    /// The customer-controlled destination URL for the abuse deny-list;
    /// `None` for transports with a fixed vendor endpoint.
    pub fn abuse_url(&self) -> Option<&str> {
        with_transport!(self, |c| c.abuse_url())
    }

    /// See [`TransportConfig::operator_managed`].
    pub fn operator_managed(&self) -> bool {
        with_transport!(self, |c| c.operator_managed())
    }

    /// See [`TransportConfig::normalize`].
    pub fn normalize(&mut self) {
        with_transport!(self, |c| c.normalize())
    }

    /// See [`TransportConfig::quiet_broadcast_mention`].
    pub fn quieted_for_test(&self) -> Self {
        let mut quieted = self.clone();
        with_transport!(&mut quieted, |c| c.quiet_broadcast_mention());
        quieted
    }

    /// See [`TransportConfig::lifecycle_ref`].
    pub fn lifecycle_ref(&self) -> Option<&str> {
        with_transport!(self, |c| c.lifecycle_ref())
    }
}

pub const MAX_CHANNEL_NAME_LEN: usize = 100;

/// Trimmed-name rule shared by create and update so both reject the same set.
pub fn validate_channel_name(name: &str) -> Result<(), String> {
    let n = name.trim();
    if n.is_empty() {
        return Err("name is required".into());
    }
    if n.chars().count() > MAX_CHANNEL_NAME_LEN {
        return Err(format!(
            "name must be at most {MAX_CHANNEL_NAME_LEN} characters"
        ));
    }
    Ok(())
}

/// No `org_id` on the wire type: the owning org is the caller's resolved
/// tenant, threaded explicitly into every store call, never trusted from the
/// request body.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct NotificationChannel {
    pub id: Uuid,
    pub name: String,
    pub kind: ChannelKind,
    /// Always plaintext in memory; the store seals/opens it at the DB edge.
    pub config: ChannelConfig,
    pub enabled: bool,
    /// Platform-disable note (e.g. the bot left the linked chat); `None`
    /// for operator-initiated disables, cleared on re-enable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    /// When the email address confirmed its verification link; reset on
    /// config change. Only consulted for `kind = email` — `None` elsewhere.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified_at: Option<DateTime<Utc>>,
    /// Deliveries in a row that used up every retry; any send that lands
    /// resets it, so it is the current run and not a lifetime total.
    pub consecutive_failures: i32,
    /// Start of the current run of failures.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failing_since: Option<DateTime<Utc>>,
    /// Last delivery that landed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_delivered_at: Option<DateTime<Utc>>,
    /// Tag rule: also pages any monitor carrying one of these tags, on top
    /// of the ones bound explicitly. Resolved at alert time, so a retagged
    /// monitor needs no rewrite here.
    #[serde(default)]
    pub auto_bind_tags: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Where this channel was last changed from (UI, API, or Terraform).
    pub write_source: WriteSource,
}

impl NotificationChannel {
    /// Email channels deliver only after the address confirms its
    /// verification link; every other kind is unaffected.
    pub fn awaiting_verification(&self) -> bool {
        self.kind == ChannelKind::Email && self.verified_at.is_none()
    }

    /// Whether a page aimed here would be attempted at all: a disabled
    /// channel and an unconfirmed address both swallow every delivery.
    pub fn can_deliver(&self) -> bool {
        self.enabled && !self.awaiting_verification()
    }

    /// One tag in common is enough: a team owns a set of resources, not an
    /// intersection of labels.
    pub fn auto_binds(&self, tags: &[String]) -> bool {
        tag_rule_matches(&self.auto_bind_tags, tags)
    }

    /// Enabled, able to deliver at all, and nothing has landed for `limit`
    /// deliveries running. An unverified address is excluded because it fails
    /// every delivery by design and says so on its own.
    pub fn is_failing(&self, limit: u32) -> bool {
        self.enabled
            && !self.awaiting_verification()
            && failure_run_reached(self.consecutive_failures, limit)
    }
}

/// Whether a tag rule covers a monitor. The one owner of that decision: the
/// paging query, the console badges and the MCP coverage view all ask here, so
/// none of them can disagree about who gets woken.
///
/// Folds case, unlike the monitor-tag filter. A filter that matches nothing
/// says so on screen; a rule that matches nothing stays silent until an outage
/// nobody is told about.
pub fn tag_rule_matches(rule: &[String], monitor_tags: &[String]) -> bool {
    if rule.is_empty() || monitor_tags.is_empty() {
        return false;
    }
    let folded: Vec<String> = monitor_tags.iter().map(|t| t.to_lowercase()).collect();
    matches_folded(rule, &folded)
}

/// [`tag_rule_matches`] against tags already folded, for a caller checking many
/// rules against one monitor. Folded with `to_lowercase`, not an ASCII fold: a
/// tag may be written in any script.
pub fn matches_folded(rule: &[String], folded_tags: &[String]) -> bool {
    rule.iter().any(|r| folded_tags.contains(&r.to_lowercase()))
}

/// The threshold alone, for callers holding a fresh count; `0` never flags.
pub fn failure_run_reached(consecutive_failures: i32, limit: u32) -> bool {
    limit > 0 && consecutive_failures >= i32::try_from(limit).unwrap_or(i32::MAX)
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewNotificationChannel {
    #[schema(example = "Ops Slack", max_length = 100)]
    pub name: String,
    pub config: ChannelConfig,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Tag rule; see [`NotificationChannel::auto_bind_tags`].
    #[serde(default)]
    #[schema(example = json!(["db"]))]
    pub auto_bind_tags: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NotificationChannelUpdate {
    pub name: Option<String>,
    pub config: Option<ChannelConfig>,
    pub enabled: Option<bool>,
    /// Replaces the whole rule; `[]` clears it.
    pub auto_bind_tags: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::transport::MASK;
    use super::*;

    #[test]
    fn config_round_trips_per_variant() {
        for json in [
            r#"{"type":"webhook","url":"https://x.test/h","headers":{"X-Tok":"s"}}"#,
            r#"{"type":"webhook","url":"https://x.test/h","secret":"0123456789abcdef"}"#,
            r#"{"type":"slack","webhook_url":"https://hooks.slack.com/x"}"#,
            r#"{"type":"telegram","bot_token":"123:abc","chat_id":"-100"}"#,
            r#"{"type":"telegram_app","chat_id":"-100123"}"#,
            r#"{"type":"telegram_app","chat_id":"42","chat_title":"Ops"}"#,
            r#"{"type":"whatsapp","access_token":"tok","phone_number_id":"123","to":"15551234567","template_name":"uptime_alert"}"#,
            r#"{"type":"whatsapp","access_token":"tok","phone_number_id":"123","to":"15551234567","template_name":"uptime_alert","language_code":"en"}"#,
            r#"{"type":"discord","webhook_url":"https://discord.com/api/webhooks/1/x"}"#,
            r#"{"type":"msteams","webhook_url":"https://prod-77.westus.logic.azure.com/workflows/x"}"#,
            r#"{"type":"google_chat","webhook_url":"https://chat.googleapis.com/v1/spaces/A/messages?key=k&token=t"}"#,
            r#"{"type":"email","to":"ops@example.com"}"#,
            r#"{"type":"pagerduty","routing_key":"R0123456789abcdef0123456789abcde"}"#,
            r#"{"type":"ntfy","server_url":"https://ntfy.sh","topic":"uptime-alerts"}"#,
            r#"{"type":"ntfy","server_url":"https://ntfy.example.com","topic":"ops","access_token":"tk_x"}"#,
            r#"{"type":"gotify","server_url":"https://push.example.com","token":"AbCdEfGhIjKlMnO"}"#,
            r#"{"type":"mattermost","webhook_url":"https://mm.example.com/hooks/abc"}"#,
            r#"{"type":"mattermost","webhook_url":"https://mm.example.com/hooks/abc","mention":"@here"}"#,
            r#"{"type":"pushover","token":"azGDORePK8gMaC0QOYAMyEEuzJnyUi","user":"uQiRzpo4DXghDmr9QzzfQu27cmVRsG"}"#,
            r#"{"type":"pushover","token":"azGDORePK8gMaC0QOYAMyEEuzJnyUi","user":"uQiRzpo4DXghDmr9QzzfQu27cmVRsG","device":"droid2"}"#,
            r#"{"type":"sms","provider":"twilio","to":"+15551234567","from":"+15557654321","account_sid":"AC0123456789ABCDEF0123456789ABCDEF","auth_token":"tok"}"#,
            r#"{"type":"sms","provider":"telnyx","to":"+15551234567","from":"alerts","api_key":"KEY123"}"#,
            r#"{"type":"sms","provider":"telnyx","to":"+15551234567","from":"alerts","api_key":"KEY123","messaging_profile_id":"40000000-0000-0000-0000-000000000000"}"#,
            r#"{"type":"sms","provider":"vonage","to":"+15551234567","from":"Acme","api_key":"a1b2c3d4","api_secret":"sekret"}"#,
            r#"{"type":"sms","provider":"plivo","to":"+15551234567","from":"+15557654321","auth_id":"MAXXXXXXXXXXXXXXXXXX","auth_token":"tok"}"#,
            r#"{"type":"sms","provider":"sinch","to":"+15551234567","from":"Acme","service_plan_id":"abc123","api_token":"tok"}"#,
            r#"{"type":"sms","provider":"sinch","to":"+15551234567","from":"Acme","service_plan_id":"abc123","api_token":"tok","region":"eu"}"#,
        ] {
            let c: ChannelConfig = serde_json::from_str(json).unwrap();
            let back = serde_json::to_string(&c).unwrap();
            let c2: ChannelConfig = serde_json::from_str(&back).unwrap();
            assert_eq!(c, c2);
        }
    }

    #[test]
    fn kind_matches_wire_tag_for_every_variant() {
        // KIND is a per-struct constant, no longer tied to the enum variant
        // by a match — this pins each one to its serde `type` tag so a
        // copy-pasted transport can't desync the DB `kind` column from the
        // stored config.
        let configs = [
            ChannelConfig::Webhook(WebhookConfig {
                url: "https://x.test".into(),
                headers: BTreeMap::new(),
                secret: None,
            }),
            ChannelConfig::Slack(SlackConfig {
                webhook_url: "https://hooks.slack.com/x".into(),
                mention: None,
            }),
            ChannelConfig::Telegram(TelegramConfig {
                bot_token: "t".into(),
                chat_id: "1".into(),
            }),
            ChannelConfig::TelegramApp(TelegramAppConfig {
                chat_id: "1".into(),
                chat_title: None,
            }),
            ChannelConfig::WhatsApp(WhatsAppConfig {
                access_token: "tok".into(),
                phone_number_id: "123".into(),
                to: "15551234567".into(),
                template_name: "uptime_alert".into(),
                language_code: None,
            }),
            ChannelConfig::WhatsAppApp(WhatsAppAppConfig {
                phone: "15551234567".into(),
                profile_name: None,
            }),
            ChannelConfig::Discord(DiscordConfig {
                webhook_url: "https://discord.com/api/webhooks/1/x".into(),
                mention: None,
            }),
            ChannelConfig::MsTeams(MsTeamsConfig {
                webhook_url: "https://prod-77.westus.logic.azure.com/workflows/x".into(),
            }),
            ChannelConfig::GoogleChat(GoogleChatConfig {
                webhook_url: "https://chat.googleapis.com/v1/spaces/A/messages".into(),
            }),
            ChannelConfig::Email(EmailConfig {
                to: "ops@example.com".into(),
            }),
            ChannelConfig::PagerDuty(PagerDutyConfig {
                routing_key: "R0123456789abcdef0123456789abcde".into(),
            }),
            ChannelConfig::Ntfy(NtfyConfig {
                server_url: "https://ntfy.sh".into(),
                topic: "uptime-alerts".into(),
                access_token: None,
            }),
            ChannelConfig::Gotify(GotifyConfig {
                server_url: "https://push.example.com".into(),
                token: "AbCdEfGhIjKlMnO".into(),
            }),
            ChannelConfig::Pushover(PushoverConfig {
                token: "azGDORePK8gMaC0QOYAMyEEuzJnyUi".into(),
                user: "uQiRzpo4DXghDmr9QzzfQu27cmVRsG".into(),
                device: None,
                emergency: false,
            }),
            ChannelConfig::Sms(SmsConfig::Twilio {
                to: "+15551234567".into(),
                from: "+15557654321".into(),
                account_sid: "AC0123456789ABCDEF0123456789ABCDEF".into(),
                auth_token: "tok".into(),
            }),
            ChannelConfig::Mattermost(MattermostConfig {
                webhook_url: "https://mm.example.com/hooks/abc".into(),
                mention: None,
            }),
        ];
        assert_eq!(configs.len(), ChannelKind::ALL.len());
        for c in configs {
            let tag = serde_json::to_value(&c).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_string();
            assert_eq!(c.kind().as_db_str(), tag);
            // Only the linked kinds are mintable solely by the operator's flow.
            assert_eq!(
                c.operator_managed(),
                matches!(
                    c.kind(),
                    ChannelKind::TelegramApp | ChannelKind::WhatsAppApp
                ),
                "operator_managed drifted for {tag}"
            );
        }
    }

    #[test]
    fn redacted_masks_every_secret() {
        let c = ChannelConfig::Telegram(TelegramConfig {
            bot_token: "123:supersecret".into(),
            chat_id: "-100".into(),
        });
        let r = c.redacted();
        assert_eq!(r["bot_token"], "***");
        // Non-secret routing info is preserved so the UI stays useful.
        assert_eq!(r["chat_id"], "-100");
        assert!(!serde_json::to_string(&r).unwrap().contains("supersecret"));

        let w = ChannelConfig::Webhook(WebhookConfig {
            url: "https://x.test/hooks/abc/secret".into(),
            headers: BTreeMap::from([("Authorization".into(), "Bearer tkn".into())]),
            secret: Some("0123456789abcdef".into()),
        });
        let rw = w.redacted();
        assert_eq!(rw["url"], "***");
        assert_eq!(rw["headers"]["Authorization"], "***");
        assert_eq!(rw["secret"], "***");
        assert!(!serde_json::to_string(&rw).unwrap().contains("tkn"));
        // A re-submitted masked signing secret is caught as the sentinel.
        let mut masked = w.clone();
        masked.redact_in_place();
        assert!(masked.has_redaction_sentinel());
    }

    #[test]
    fn validate_rejects_non_https_and_empty() {
        assert!(
            ChannelConfig::Slack(SlackConfig {
                webhook_url: "http://insecure".into(),
                mention: None,
            })
            .validate()
            .is_err()
        );
        assert!(
            ChannelConfig::Telegram(TelegramConfig {
                bot_token: "  ".into(),
                chat_id: "1".into()
            })
            .validate()
            .is_err()
        );
        assert!(
            ChannelConfig::Webhook(WebhookConfig {
                url: "https://ok.test".into(),
                headers: BTreeMap::new(),
                secret: None,
            })
            .validate()
            .is_ok()
        );
        // A too-short signing secret is rejected.
        assert!(
            ChannelConfig::Webhook(WebhookConfig {
                url: "https://ok.test".into(),
                headers: BTreeMap::new(),
                secret: Some("short".into()),
            })
            .validate()
            .is_err()
        );
    }

    #[test]
    fn abuse_url_only_for_customer_destinations() {
        let slack = ChannelConfig::Slack(SlackConfig {
            webhook_url: "https://hooks.slack.com/x".into(),
            mention: None,
        });
        assert_eq!(slack.abuse_url(), Some("https://hooks.slack.com/x"));
        let tg = ChannelConfig::Telegram(TelegramConfig {
            bot_token: "t".into(),
            chat_id: "1".into(),
        });
        assert_eq!(tg.abuse_url(), None);
    }

    #[test]
    fn whatsapp_redacts_token_and_validates_phone_shape() {
        let mut c = ChannelConfig::WhatsApp(WhatsAppConfig {
            access_token: "EAAGsecret".into(),
            phone_number_id: "106540352242922".into(),
            to: "+15551234567".into(),
            template_name: "uptime_alert".into(),
            language_code: None,
        });
        assert!(c.validate().is_ok());
        let r = c.redacted();
        assert_eq!(r["access_token"], "***");
        // Routing shape survives so the UI stays useful.
        assert_eq!(r["to"], "+15551234567");
        assert_eq!(r["template_name"], "uptime_alert");
        c.redact_in_place();
        assert!(c.has_redaction_sentinel());

        let bad = |f: fn(&mut WhatsAppConfig)| {
            let mut w = WhatsAppConfig {
                access_token: "tok".into(),
                phone_number_id: "123".into(),
                to: "15551234567".into(),
                template_name: "uptime_alert".into(),
                language_code: None,
            };
            f(&mut w);
            ChannelConfig::WhatsApp(w).validate()
        };
        assert!(bad(|w| w.access_token = "to k".into()).is_err());
        assert!(bad(|w| w.access_token = "tok\n".into()).is_err());
        assert!(bad(|w| w.phone_number_id = "not-numeric".into()).is_err());
        assert!(bad(|w| w.to = "abc".into()).is_err());
        assert!(bad(|w| w.to = "12345".into()).is_err());
        assert!(bad(|w| w.to = " 15551234567".into()).is_err());
        assert!(bad(|w| w.template_name = "Uptime Alert".into()).is_err());
        assert!(bad(|w| w.template_name = "".into()).is_err());
        assert!(bad(|w| w.language_code = Some("en US".into())).is_err());
    }

    #[test]
    fn telegram_app_is_secretless_and_validates_chat_id() {
        let mut c = ChannelConfig::TelegramApp(TelegramAppConfig {
            chat_id: "-100123".into(),
            chat_title: Some("Ops".into()),
        });
        assert!(c.validate().is_ok());
        assert_eq!(c.abuse_url(), None);
        assert!(c.operator_managed());
        // Nothing to mask: the redacted copy is byte-identical and a
        // round-tripped config never reads as the sentinel.
        let r = c.redacted();
        assert_eq!(r["chat_id"], "-100123");
        assert_eq!(r["chat_title"], "Ops");
        c.redact_in_place();
        assert!(!c.has_redaction_sentinel());

        let bad = |chat_id: &str| {
            ChannelConfig::TelegramApp(TelegramAppConfig {
                chat_id: chat_id.into(),
                chat_title: None,
            })
            .validate()
        };
        assert!(bad("").is_err());
        assert!(bad("not-a-number").is_err());
        assert!(bad("@channelname").is_err());
    }

    #[test]
    fn provider_webhooks_pin_their_hosts() {
        let discord = |url: &str| {
            ChannelConfig::Discord(DiscordConfig {
                webhook_url: url.into(),
                mention: None,
            })
            .validate()
        };
        assert!(discord("https://discord.com/api/webhooks/123/tok").is_ok());
        assert!(discord("https://ptb.discord.com/api/webhooks/123/tok").is_ok());
        assert!(discord("https://discordapp.com/api/webhooks/123/tok").is_ok());
        // Wrong provider, lookalike suffix, wrong path, plain http.
        assert!(discord("https://hooks.slack.com/services/T/B/x").is_err());
        assert!(discord("https://evildiscord.com/api/webhooks/1/x").is_err());
        assert!(discord("https://discord.com.evil.test/api/webhooks/1/x").is_err());
        assert!(discord("https://discord.com/webhooks/1/x").is_err());
        assert!(discord("http://discord.com/api/webhooks/1/x").is_err());

        let teams = |url: &str| {
            ChannelConfig::MsTeams(MsTeamsConfig {
                webhook_url: url.into(),
            })
            .validate()
        };
        assert!(teams("https://prod-77.westus.logic.azure.com/workflows/x/triggers/y").is_ok());
        assert!(teams("https://acme.api.powerplatform.com/workflows/x").is_ok());
        assert!(teams("https://logic.azure.com.evil.test/workflows/x").is_err());
        assert!(teams("https://example.com/workflows/x").is_err());

        let gchat = |url: &str| {
            ChannelConfig::GoogleChat(GoogleChatConfig {
                webhook_url: url.into(),
            })
            .validate()
        };
        assert!(gchat("https://chat.googleapis.com/v1/spaces/A/messages?key=k&token=t").is_ok());
        assert!(gchat("https://chat.googleapis.com./v1/spaces/A/messages").is_ok());
        assert!(gchat("https://googleapis.com/v1/spaces/A/messages").is_err());
        assert!(gchat("https://chat.example.com/v1/spaces/A/messages").is_err());

        // The mismatch message points at the escape hatch.
        let err = discord("https://example.com/hook").unwrap_err();
        assert!(err.contains("use the webhook type"), "{err}");

        // Whole-URL masking, same policy as slack.
        let mut c = ChannelConfig::Discord(DiscordConfig {
            webhook_url: "https://discord.com/api/webhooks/123/tok".into(),
            mention: None,
        });
        assert_eq!(
            c.abuse_url(),
            Some("https://discord.com/api/webhooks/123/tok")
        );
        c.redact_in_place();
        assert!(c.has_redaction_sentinel());
        assert_eq!(c.redacted()["webhook_url"], MASK);
    }

    #[test]
    fn email_is_secretless_and_validates_address_shape() {
        let mut c = ChannelConfig::Email(EmailConfig {
            to: "ops+alerts@example.com".into(),
        });
        assert!(c.validate().is_ok());
        assert_eq!(c.abuse_url(), None);
        assert!(!c.operator_managed());
        // Nothing to mask: the address survives redaction and a round-trip
        // never reads as the sentinel.
        assert_eq!(c.redacted()["to"], "ops+alerts@example.com");
        c.redact_in_place();
        assert!(!c.has_redaction_sentinel());

        let bad = |to: &str| ChannelConfig::Email(EmailConfig { to: to.into() }).validate();
        assert!(bad("").is_err());
        assert!(bad("not-an-email").is_err());
        assert!(bad("a@b").is_err());
        assert!(bad("@example.com").is_err());
        assert!(bad("a b@example.com").is_err());
        assert!(bad("Ops@example.com").is_err());
        assert!(bad("a@.example.com").is_err());
        assert!(bad("a@example.com.").is_err());
        assert!(bad("a@example..com").is_err());
        assert!(bad(&format!("{}@example.com", "x".repeat(65))).is_err());
        assert!(bad(&format!("a@{}.com", "x".repeat(260))).is_err());
        // Role and tagged addresses are deliberately allowed.
        assert!(bad("ops@example.com").is_ok());
        assert!(bad("a+tag@sub.example.co").is_ok());
    }

    fn sample_channel(kind_email: bool, verified: bool) -> NotificationChannel {
        use chrono::Utc;
        NotificationChannel {
            id: Uuid::nil(),
            name: "n".into(),
            kind: if kind_email {
                ChannelKind::Email
            } else {
                ChannelKind::Slack
            },
            config: if kind_email {
                ChannelConfig::Email(EmailConfig {
                    to: "ops@example.com".into(),
                })
            } else {
                ChannelConfig::Slack(SlackConfig {
                    webhook_url: "https://hooks.slack.com/x".into(),
                    mention: None,
                })
            },
            enabled: true,
            disabled_reason: None,
            verified_at: verified.then(Utc::now),
            consecutive_failures: 0,
            failing_since: None,
            last_delivered_at: None,
            auto_bind_tags: Vec::new(),
            write_source: WriteSource::Ui,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn a_tag_rule_needs_one_tag_in_common_not_all_of_them() {
        let mut ch = sample_channel(false, false);
        ch.auto_bind_tags = vec!["db".into(), "cache".into()];
        assert!(ch.auto_binds(&["db".to_string()]));
        assert!(ch.auto_binds(&["eu".to_string(), "cache".to_string()]));
        assert!(!ch.auto_binds(&["web".to_string()]));
        assert!(!ch.auto_binds(&[]));
        // A rule spelled in another case still pages: the operator who typed
        // it would otherwise learn of the mismatch from an outage.
        assert!(ch.auto_binds(&["DB".to_string()]));
        assert!(ch.auto_binds(&["Cache".to_string()]));
        // An empty rule matches nothing, however the monitor is tagged.
        assert!(!sample_channel(false, false).auto_binds(&["db".to_string()]));
    }

    #[test]
    fn a_channel_nothing_can_land_on_does_not_count_as_reaching_someone() {
        assert!(sample_channel(true, true).can_deliver());
        // Unconfirmed address: every send fails by design.
        assert!(!sample_channel(true, false).can_deliver());
        let mut off = sample_channel(true, true);
        off.enabled = false;
        assert!(!off.can_deliver());
    }

    #[test]
    fn awaiting_verification_only_for_unverified_email() {
        assert!(sample_channel(true, false).awaiting_verification());
        assert!(!sample_channel(true, true).awaiting_verification());
        assert!(!sample_channel(false, false).awaiting_verification());
    }

    #[test]
    fn pagerduty_masks_key_and_validates_shape() {
        let mut c = ChannelConfig::PagerDuty(PagerDutyConfig {
            routing_key: "R0123456789abcdef0123456789abcde".into(),
        });
        assert!(c.validate().is_ok());
        assert_eq!(c.abuse_url(), None);
        assert!(!c.operator_managed());
        assert_eq!(c.redacted()["routing_key"], MASK);
        c.redact_in_place();
        assert!(c.has_redaction_sentinel());

        let bad = |key: &str| {
            ChannelConfig::PagerDuty(PagerDutyConfig {
                routing_key: key.into(),
            })
            .validate()
        };
        assert!(bad("").is_err());
        assert!(bad("short").is_err());
        assert!(bad(&"x".repeat(33)).is_err());
        assert!(bad("R0123456789abcdef0123456789abcd!").is_err());
        assert!(bad(&"a".repeat(32)).is_ok());
    }

    #[test]
    fn ntfy_masks_token_and_pins_server_root() {
        let mut c = ChannelConfig::Ntfy(NtfyConfig {
            server_url: "https://ntfy.example.com".into(),
            topic: "uptime_alerts-1".into(),
            access_token: Some("tk_secret".into()),
        });
        assert!(c.validate().is_ok());
        assert_eq!(c.abuse_url(), Some("https://ntfy.example.com"));
        let r = c.redacted();
        // Server + topic are routing, the token is the only secret.
        assert_eq!(r["server_url"], "https://ntfy.example.com");
        assert_eq!(r["topic"], "uptime_alerts-1");
        assert_eq!(r["access_token"], MASK);
        c.redact_in_place();
        assert!(c.has_redaction_sentinel());

        // Token-less config has nothing to mask and never reads as sentinel.
        let mut open = ChannelConfig::Ntfy(NtfyConfig {
            server_url: "https://ntfy.sh".into(),
            topic: "t".into(),
            access_token: None,
        });
        assert!(open.validate().is_ok());
        open.redact_in_place();
        assert!(!open.has_redaction_sentinel());

        let bad = |f: fn(&mut NtfyConfig)| {
            let mut n = NtfyConfig {
                server_url: "https://ntfy.sh".into(),
                topic: "ops".into(),
                access_token: None,
            };
            f(&mut n);
            ChannelConfig::Ntfy(n).validate()
        };
        assert!(bad(|n| n.server_url = "http://ntfy.sh".into()).is_err());
        assert!(bad(|n| n.server_url = "https://ntfy.sh/mytopic".into()).is_err());
        assert!(bad(|n| n.server_url = "https://ntfy.sh/?x=1".into()).is_err());
        assert!(bad(|n| n.server_url = "https://tk:secret@ntfy.sh".into()).is_err());
        assert!(bad(|n| n.server_url = "https://tk@ntfy.sh".into()).is_err());
        assert!(bad(|n| n.topic = "".into()).is_err());
        assert!(bad(|n| n.topic = "has space".into()).is_err());
        assert!(bad(|n| n.topic = "x".repeat(65)).is_err());
        assert!(bad(|n| n.access_token = Some("".into())).is_err());
        assert!(bad(|n| n.access_token = Some("tk x".into())).is_err());
        // Bare server_url keeps the root path after parsing.
        assert!(bad(|n| n.server_url = "https://ntfy.sh".into()).is_ok());

        // Omitted server_url defaults to ntfy.sh on the wire.
        let parsed: ChannelConfig =
            serde_json::from_str(r#"{"type":"ntfy","topic":"ops"}"#).unwrap();
        let ChannelConfig::Ntfy(n) = &parsed else {
            panic!()
        };
        assert_eq!(n.server_url, "https://ntfy.sh");
    }

    #[test]
    fn gotify_masks_token_and_keeps_base_url() {
        let mut c = ChannelConfig::Gotify(GotifyConfig {
            server_url: "https://push.example.com/gotify".into(),
            token: "AbCdEfGhIjKlMnO".into(),
        });
        assert!(c.validate().is_ok());
        assert_eq!(c.abuse_url(), Some("https://push.example.com/gotify"));
        let r = c.redacted();
        assert_eq!(r["server_url"], "https://push.example.com/gotify");
        assert_eq!(r["token"], MASK);
        c.redact_in_place();
        assert!(c.has_redaction_sentinel());

        let bad = |f: fn(&mut GotifyConfig)| {
            let mut g = GotifyConfig {
                server_url: "https://push.example.com".into(),
                token: "AbCdEfGhIjKlMnO".into(),
            };
            f(&mut g);
            ChannelConfig::Gotify(g).validate()
        };
        assert!(bad(|g| g.server_url = "http://push.example.com".into()).is_err());
        assert!(bad(|g| g.server_url = "https://push.example.com/?x=1".into()).is_err());
        assert!(bad(|g| g.server_url = "https://u:p@push.example.com".into()).is_err());
        assert!(bad(|g| g.token = "".into()).is_err());
        assert!(bad(|g| g.token = "tok en".into()).is_err());
        assert!(bad(|g| g.token = "x".repeat(201)).is_err());
        // A subpath install is normal behind a reverse proxy.
        assert!(bad(|g| g.server_url = "https://push.example.com/gotify".into()).is_ok());
    }

    #[test]
    fn mattermost_masks_the_hook_url_and_pins_the_hooks_path() {
        let mut c = ChannelConfig::Mattermost(MattermostConfig {
            webhook_url: "https://mm.example.com/hooks/abc123".into(),
            mention: Some("@here".into()),
        });
        assert!(c.validate().is_ok());
        assert_eq!(c.abuse_url(), Some("https://mm.example.com/hooks/abc123"));
        let r = c.redacted();
        assert_eq!(r["webhook_url"], MASK);
        assert_eq!(r["mention"], "@here");
        c.redact_in_place();
        assert!(c.has_redaction_sentinel());

        let bad = |f: fn(&mut MattermostConfig)| {
            let mut m = MattermostConfig {
                webhook_url: "https://mm.example.com/hooks/abc123".into(),
                mention: None,
            };
            f(&mut m);
            ChannelConfig::Mattermost(m).validate()
        };
        assert!(bad(|m| m.webhook_url = "http://mm.example.com/hooks/abc".into()).is_err());
        assert!(bad(|m| m.webhook_url = "https://mm.example.com/api/v4/posts".into()).is_err());
        assert!(bad(|m| m.webhook_url = "https://mm.example.com/hooks/".into()).is_err());
        assert!(bad(|m| m.mention = Some("@nobody!".into())).is_err());
        assert!(bad(|m| m.mention = Some("Upper".into())).is_err());
        assert!(bad(|m| m.mention = Some("x".repeat(65))).is_err());
        assert!(bad(|m| m.mention = Some("a b c d e f".into())).is_err());
        assert!(bad(|m| m.mention = Some("9lives".into())).is_ok());
        assert!(bad(|m| m.mention = Some("ab".into())).is_ok());
        assert!(bad(|m| m.mention = Some("svc.monitoring.oncall.emea".into())).is_ok());
        assert!(bad(|m| m.webhook_url = "https://co.test/mm/hooks/abc/".into()).is_ok());
        assert!(bad(|m| m.mention = Some("@all, oncall-sre".into())).is_ok());
    }

    #[test]
    fn a_mattermost_ping_is_lowercased_and_a_test_send_drops_the_broadcast() {
        let mut c = ChannelConfig::Mattermost(MattermostConfig {
            webhook_url: " https://mm.example.com/hooks/abc123 ".into(),
            mention: Some("@Channel, OnCall-SRE".into()),
        });
        c.normalize();
        assert!(c.validate().is_ok());
        let ChannelConfig::Mattermost(m) = &c else {
            panic!()
        };
        assert_eq!(m.webhook_url, "https://mm.example.com/hooks/abc123");
        assert_eq!(m.mention_markup().as_deref(), Some("@channel @oncall-sre"));

        let ChannelConfig::Mattermost(m) = &c.quieted_for_test() else {
            panic!()
        };
        assert_eq!(m.mention_markup().as_deref(), Some("@oncall-sre"));
    }

    #[test]
    fn pushover_masks_both_keys_and_validates_shape() {
        let mut c = ChannelConfig::Pushover(PushoverConfig {
            token: "azGDORePK8gMaC0QOYAMyEEuzJnyUi".into(),
            user: "uQiRzpo4DXghDmr9QzzfQu27cmVRsG".into(),
            device: Some("droid2".into()),
            emergency: false,
        });
        assert!(c.validate().is_ok());
        assert_eq!(c.abuse_url(), None);
        let r = c.redacted();
        assert_eq!(r["token"], MASK);
        assert_eq!(r["user"], MASK);
        assert_eq!(r["device"], "droid2");
        c.redact_in_place();
        assert!(c.has_redaction_sentinel());

        let bad = |f: fn(&mut PushoverConfig)| {
            let mut p = PushoverConfig {
                token: "azGDORePK8gMaC0QOYAMyEEuzJnyUi".into(),
                user: "uQiRzpo4DXghDmr9QzzfQu27cmVRsG".into(),
                device: None,
                emergency: false,
            };
            f(&mut p);
            ChannelConfig::Pushover(p).validate()
        };
        assert!(bad(|p| p.token = "short".into()).is_err());
        assert!(bad(|p| p.token = format!("{}!", "x".repeat(29))).is_err());
        assert!(bad(|p| p.user = "".into()).is_err());
        assert!(bad(|p| p.device = Some("".into())).is_err());
        assert!(bad(|p| p.device = Some("x".repeat(26))).is_err());
        assert!(bad(|p| p.device = Some("bad device".into())).is_err());
        assert!(bad(|p| p.device = Some("ok_device-1".into())).is_ok());
    }

    #[test]
    fn sms_masks_only_the_secret_per_provider_and_validates() {
        // Twilio: account_sid is an identifier and survives; auth_token masks.
        let mut twilio = ChannelConfig::Sms(SmsConfig::Twilio {
            to: "+15551234567".into(),
            from: "+15557654321".into(),
            account_sid: "AC0123456789ABCDEF0123456789ABCDEF".into(),
            auth_token: "supersecret".into(),
        });
        assert!(twilio.validate().is_ok());
        assert_eq!(twilio.abuse_url(), None);
        assert!(!twilio.operator_managed());
        let r = twilio.redacted();
        assert_eq!(r["account_sid"], "AC0123456789ABCDEF0123456789ABCDEF");
        assert_eq!(r["to"], "+15551234567");
        assert_eq!(r["auth_token"], MASK);
        assert!(!serde_json::to_string(&r).unwrap().contains("supersecret"));
        twilio.redact_in_place();
        assert!(twilio.has_redaction_sentinel());

        // Telnyx: api_key is the secret; Vonage: api_secret is, api_key isn't.
        let mut telnyx = ChannelConfig::Sms(SmsConfig::Telnyx {
            to: "+15551234567".into(),
            from: "alerts".into(),
            api_key: "KEY_secret".into(),
            messaging_profile_id: None,
        });
        assert!(telnyx.validate().is_ok());
        assert_eq!(telnyx.redacted()["api_key"], MASK);
        telnyx.redact_in_place();
        assert!(telnyx.has_redaction_sentinel());

        let vonage = ChannelConfig::Sms(SmsConfig::Vonage {
            to: "+15551234567".into(),
            from: "Acme".into(),
            api_key: "a1b2c3d4".into(),
            api_secret: "vsecret".into(),
        });
        let rv = vonage.redacted();
        assert_eq!(rv["api_key"], "a1b2c3d4");
        assert_eq!(rv["api_secret"], MASK);

        // Plivo: auth_id is an identifier and survives; auth_token masks.
        let plivo = ChannelConfig::Sms(SmsConfig::Plivo {
            to: "+15551234567".into(),
            from: "+15557654321".into(),
            auth_id: "MAXXXXXXXXXXXXXXXXXX".into(),
            auth_token: "psecret".into(),
        });
        assert!(plivo.validate().is_ok());
        let rp = plivo.redacted();
        assert_eq!(rp["auth_id"], "MAXXXXXXXXXXXXXXXXXX");
        assert_eq!(rp["auth_token"], MASK);

        // Sinch: service_plan_id survives, api_token masks; region defaults to us.
        let parsed: ChannelConfig = serde_json::from_str(
            r#"{"type":"sms","provider":"sinch","to":"+15551234567","from":"Acme","service_plan_id":"abc123","api_token":"ssecret"}"#,
        )
        .unwrap();
        assert!(parsed.validate().is_ok());
        let ChannelConfig::Sms(SmsConfig::Sinch { region, .. }) = &parsed else {
            panic!("expected sinch");
        };
        assert_eq!(region, "us");
        let rs = parsed.redacted();
        assert_eq!(rs["service_plan_id"], "abc123");
        assert_eq!(rs["api_token"], MASK);
        // Unknown region is rejected.
        assert!(
            ChannelConfig::Sms(SmsConfig::Sinch {
                to: "+15551234567".into(),
                from: "Acme".into(),
                service_plan_id: "abc123".into(),
                api_token: "tok".into(),
                region: "moon".into(),
            })
            .validate()
            .is_err()
        );

        let twilio = |f: fn(&mut (String, String, String, String))| {
            let mut t = (
                "+15551234567".to_string(),
                "+15557654321".to_string(),
                "AC0123456789ABCDEF0123456789ABCDEF".to_string(),
                "tok".to_string(),
            );
            f(&mut t);
            ChannelConfig::Sms(SmsConfig::Twilio {
                to: t.0,
                from: t.1,
                account_sid: t.2,
                auth_token: t.3,
            })
            .validate()
        };
        assert!(twilio(|t| t.0 = "15551234567".into()).is_err()); // no +
        assert!(twilio(|t| t.0 = "+12".into()).is_err()); // too short
        assert!(twilio(|t| t.0 = "+1555 123".into()).is_err()); // space
        assert!(twilio(|t| t.1 = "has space".into()).is_err());
        assert!(twilio(|t| t.2 = "AC123".into()).is_err()); // bad sid
        assert!(twilio(|t| t.3 = "".into()).is_err());
        assert!(twilio(|t| t.3 = "tok en".into()).is_err());
    }

    #[test]
    fn mask_matches_canonical_redaction_sentinel() {
        // A redacted config round-tripped through the API must be detectable
        // as the sentinel (re-submitted "***" is rejected). That only holds
        // if this mask stays byte-equal to the canonical one.
        assert_eq!(MASK, crate::api::redaction::REDACTED);
    }

    #[test]
    fn name_validation_bounds() {
        assert!(validate_channel_name("  ").is_err());
        assert!(validate_channel_name(&"x".repeat(MAX_CHANNEL_NAME_LEN + 1)).is_err());
        assert!(validate_channel_name("Ops Slack").is_ok());
    }
}

#[cfg(test)]
mod normalize_tests {
    use super::*;

    /// The console trims in the browser, so these reach the store only over
    /// the API, MCP or Terraform.
    /// An API client clears an optional string by sending it empty. Left as
    /// `Some("")` the save is refused as blank and the old ping stays live,
    /// so there is no way to stop pinging except through the console.
    #[test]
    fn an_emptied_ping_clears_rather_than_failing_the_save() {
        let mut slack = ChannelConfig::Slack(SlackConfig {
            webhook_url: "https://hooks.slack.com/services/T/B/x".into(),
            mention: Some("  ".into()),
        });
        slack.normalize();
        assert!(slack.validate().is_ok());
        let ChannelConfig::Slack(c) = &slack else {
            unreachable!()
        };
        assert_eq!(c.mention, None);

        let mut discord = ChannelConfig::Discord(DiscordConfig {
            webhook_url: "https://discord.com/api/webhooks/1/tok".into(),
            mention: Some(String::new()),
        });
        discord.normalize();
        assert!(discord.validate().is_ok());
        let ChannelConfig::Discord(c) = &discord else {
            unreachable!()
        };
        assert_eq!(c.mention, None);
    }

    #[test]
    fn a_pasted_value_is_cleaned_before_its_shape_is_judged() {
        let mut telegram = ChannelConfig::Telegram(TelegramConfig {
            bot_token: "123456:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw ".into(),
            chat_id: " -1001234567890\n".into(),
        });
        telegram.normalize();
        assert!(telegram.validate().is_ok());
        let ChannelConfig::Telegram(t) = &telegram else {
            unreachable!()
        };
        // Stored raw, this builds a URL ending `...aw%20/sendMessage`, which
        // validates, saves, and 404s on the first alert.
        assert_eq!(t.bot_token, "123456:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw");
        assert_eq!(t.chat_id, "-1001234567890");

        let mut pd = ChannelConfig::PagerDuty(PagerDutyConfig {
            routing_key: "R0ABCDEF1234567890ABCDEF12345678\n".into(),
        });
        assert!(pd.validate().is_err(), "33 chars until it is trimmed");
        pd.normalize();
        assert!(pd.validate().is_ok());
    }

    #[test]
    fn a_gotify_base_url_keeps_the_path_the_install_is_served_under() {
        let mut gotify = ChannelConfig::Gotify(GotifyConfig {
            server_url: " https://push.example.com/gotify/ ".into(),
            token: " AbCdEfGhIjKlMnO ".into(),
        });
        gotify.normalize();
        assert!(gotify.validate().is_ok());
        let ChannelConfig::Gotify(g) = &gotify else {
            unreachable!()
        };
        assert_eq!(g.server_url, "https://push.example.com/gotify");
        assert_eq!(g.token, "AbCdEfGhIjKlMnO");
        assert_eq!(g.publish_url(), "https://push.example.com/gotify/message");

        // A server that really is mounted at /message publishes one level
        // deeper — the path is never guessed away.
        let mut odd = ChannelConfig::Gotify(GotifyConfig {
            server_url: "https://push.example.com/message".into(),
            token: "AbCdEfGhIjKlMnO".into(),
        });
        odd.normalize();
        let ChannelConfig::Gotify(g) = &odd else {
            unreachable!()
        };
        assert_eq!(g.publish_url(), "https://push.example.com/message/message");
    }

    #[test]
    fn an_address_is_stored_the_way_signup_already_stores_one() {
        let mut email = ChannelConfig::Email(EmailConfig {
            to: "  Ops.Team@Example.COM ".into(),
        });
        email.normalize();
        assert!(email.validate().is_ok());
        let ChannelConfig::Email(e) = &email else {
            unreachable!()
        };
        assert_eq!(e.to, "ops.team@example.com");
    }

    #[test]
    fn a_recipient_is_stripped_to_digits_but_a_sender_id_is_left_alone() {
        let mut sms = ChannelConfig::Sms(SmsConfig::Twilio {
            to: "+1 (555) 123-4567".into(),
            from: "ACME-SMS ".into(),
            account_sid: format!("AC{}", "0".repeat(32)),
            auth_token: " tok ".into(),
        });
        sms.normalize();
        assert!(sms.validate().is_ok());
        let ChannelConfig::Sms(s) = &sms else {
            unreachable!()
        };
        assert_eq!(s.to(), "+15551234567");
        // The dash is part of the name it sends from, not punctuation.
        assert_eq!(s.from(), "ACME-SMS");
    }
}
