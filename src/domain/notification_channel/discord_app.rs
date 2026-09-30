use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::discord::{
    mention_targets, require_discord_webhook, snowflake, validate_ping, without_broadcast_ping,
};
use super::mention::cleared_when_empty;
use super::transport::{MASK, TransportConfig, kept, secret_kept, trim_in_place};
use super::{ChannelKind, DiscordMention};

/// Discord channel connected through our own "Add to Discord" app. The
/// webhook belongs to the app, so a press on a button in its alerts reaches
/// us, which a pasted webhook's never does. Only the connect flow creates one:
/// a caller-supplied webhook id would point our presses at a webhook the
/// alerts never go through. The ping is the one part people edit; an edit
/// that tries to move the connection is refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DiscordAppConfig {
    /// Webhook the install minted. The path carries the webhook token, so the
    /// whole value is treated as a secret. An edit may leave it out or send it
    /// back masked.
    #[serde(default)]
    pub webhook_url: String,
    /// Discord's id for that webhook, which every press on its alerts
    /// carries. An edit may leave it out or send it back as read.
    #[serde(default)]
    pub webhook_id: String,
    /// Who to ping on an alert, as on a pasted Discord webhook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mention: Option<String>,
}

impl DiscordAppConfig {
    pub fn mention_targets(&self) -> Option<DiscordMention> {
        mention_targets(self.mention.as_deref())
    }

    /// This config as an edit to `stored`: its webhook, with this ping.
    /// `None` when the edit names another webhook.
    pub(super) fn edited_on(&self, stored: &Self) -> Option<Self> {
        let keeps_connection =
            secret_kept(&self.webhook_url) && kept(&self.webhook_id, &stored.webhook_id);
        keeps_connection.then(|| Self {
            mention: self.mention.clone(),
            ..stored.clone()
        })
    }
}

impl TransportConfig for DiscordAppConfig {
    const KIND: ChannelKind = ChannelKind::DiscordApp;

    fn redact_in_place(&mut self) {
        self.webhook_url = MASK.to_string();
    }

    fn has_redaction_sentinel(&self) -> bool {
        self.webhook_url == MASK
    }

    fn normalize(&mut self) {
        trim_in_place(&mut self.webhook_url);
        trim_in_place(&mut self.webhook_id);
        self.mention = cleared_when_empty(self.mention.take());
    }

    fn validate(&self) -> Result<(), String> {
        require_discord_webhook(&self.webhook_url)?;
        if !snowflake(&self.webhook_id) {
            return Err("webhook_id must be a Discord webhook id".into());
        }
        // The segment after `webhooks` in the path, not anything that looks
        // like it in a query.
        let names_it = url::Url::parse(&self.webhook_url)
            .ok()
            .and_then(|u| {
                let mut path = u.path_segments()?;
                path.find(|s| *s == "webhooks")?;
                path.next().map(|id| id == self.webhook_id)
            })
            .unwrap_or(false);
        if !names_it {
            return Err("webhook_id must name the webhook in webhook_url".into());
        }
        validate_ping(self.mention.as_deref())
    }

    fn quiet_broadcast_mention(&mut self) {
        self.mention = without_broadcast_ping(self.mention.as_deref());
    }

    fn abuse_url(&self) -> Option<&str> {
        Some(&self.webhook_url)
    }

    /// The webhook id: a press names the webhook that posted the alert, never
    /// the org.
    fn lifecycle_ref(&self) -> Option<&str> {
        Some(&self.webhook_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOOK: &str = "112233445566778899";

    fn cfg() -> DiscordAppConfig {
        DiscordAppConfig {
            webhook_url: format!("https://discord.com/api/webhooks/{HOOK}/tok"),
            webhook_id: HOOK.into(),
            mention: None,
        }
    }

    #[test]
    fn the_id_must_be_the_webhooks_own() {
        assert!(cfg().validate().is_ok());
        let mut c = cfg();
        c.webhook_id = "998877665544332211".into();
        assert!(c.validate().is_err());
        c.webhook_id = "#alerts".into();
        assert!(c.validate().is_err());
        let mut c = cfg();
        c.webhook_url = format!("https://evil.example/api/webhooks/{HOOK}/tok");
        assert!(c.validate().is_err());
        c.webhook_url = format!("https://discord.com/api/webhooks/999/tok?x=/webhooks/{HOOK}/");
        assert!(c.validate().is_err());
    }

    #[test]
    fn an_edit_takes_only_the_ping_and_keeps_the_connection() {
        let ping = Some("&123456789012345678".to_string());
        let ping_only = DiscordAppConfig {
            webhook_url: String::new(),
            webhook_id: String::new(),
            mention: ping.clone(),
        };
        let read_back = DiscordAppConfig {
            webhook_url: MASK.into(),
            webhook_id: HOOK.into(),
            mention: ping.clone(),
        };
        for edit in [ping_only, read_back] {
            let edited = edit.edited_on(&cfg()).expect("keeps the connection");
            assert_eq!(
                edited,
                DiscordAppConfig {
                    mention: ping.clone(),
                    ..cfg()
                }
            );
            assert!(edited.validate().is_ok());
        }
    }

    #[test]
    fn an_edit_that_moves_the_webhook_is_refused() {
        let mut read_back = cfg();
        read_back.redact_in_place();
        assert!(read_back.edited_on(&cfg()).is_some());
        let mut edit = read_back.clone();
        edit.webhook_id = "998877665544332211".into();
        assert_eq!(edit.edited_on(&cfg()), None);
        let mut edit = read_back;
        edit.webhook_url = "https://discord.com/api/webhooks/112233445566778899/other".into();
        assert_eq!(edit.edited_on(&cfg()), None);
        assert_eq!(
            cfg().edited_on(&cfg()),
            None,
            "the stored secret is never compared"
        );
    }

    #[test]
    fn the_webhook_is_the_secret_and_its_id_is_not() {
        let mut c = cfg();
        c.redact_in_place();
        assert!(c.has_redaction_sentinel());
        assert_eq!(c.lifecycle_ref(), Some(HOOK));
    }

    #[test]
    fn a_ping_follows_the_pasted_webhooks_rules() {
        let mut c = cfg();
        c.mention = Some("@sre".into());
        assert!(c.validate().is_err());
        c.mention = Some("@here &123456789012345678".into());
        assert!(c.validate().is_ok());
        c.quiet_broadcast_mention();
        assert_eq!(
            c.mention_targets().unwrap().markup,
            "<@&123456789012345678>"
        );
    }
}
