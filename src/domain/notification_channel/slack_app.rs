use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::ChannelKind;
use super::mention::cleared_when_empty;
use super::slack::{id_like, mention_markup, validate_ping, without_broadcast_ping};
use super::transport::{MASK, TransportConfig, require_https, trim_in_place};

/// Slack channel connected through our own "Add to Slack" app. A press on a
/// button in its alerts reaches us, which a pasted webhook's never does, so
/// only the connect flow may create one: a caller-supplied channel id would
/// point our presses at a channel the webhook does not post to. The ping is
/// the one part people edit; an edit keeps the connection the flow stored,
/// whatever the body says about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SlackAppConfig {
    /// Incoming-webhook URL the install minted. The path carries the
    /// workspace token, so the whole value is treated as a secret. Kept from
    /// the connection on edit.
    #[serde(default)]
    pub webhook_url: String,
    /// The channel picked at install, e.g. `#ops-alerts`. Kept from the
    /// connection on edit.
    #[serde(default)]
    pub channel: String,
    /// Slack's id for that channel, which every press on its alerts carries.
    /// Kept from the connection on edit.
    #[serde(default)]
    pub channel_id: String,
    /// The workspace the app was installed into, when Slack names one: an
    /// install across an Enterprise Grid org has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    /// Who to ping on an alert, as on a pasted Slack webhook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mention: Option<String>,
}

impl SlackAppConfig {
    pub fn mention_markup(&self) -> Option<String> {
        mention_markup(self.mention.as_deref())
    }

    /// This config as an edit to `stored`: its connection, with this ping.
    pub(super) fn edited_on(&self, stored: &Self) -> Self {
        Self {
            mention: self.mention.clone(),
            ..stored.clone()
        }
    }
}

impl TransportConfig for SlackAppConfig {
    const KIND: ChannelKind = ChannelKind::SlackApp;

    fn redact_in_place(&mut self) {
        self.webhook_url = MASK.to_string();
    }

    fn has_redaction_sentinel(&self) -> bool {
        self.webhook_url == MASK
    }

    fn normalize(&mut self) {
        trim_in_place(&mut self.webhook_url);
        trim_in_place(&mut self.channel_id);
        if let Some(team) = &mut self.team_id {
            trim_in_place(team);
        }
        self.mention = cleared_when_empty(self.mention.take());
    }

    fn validate(&self) -> Result<(), String> {
        require_https(&self.webhook_url, "webhook_url")?;
        if !id_like(&self.channel_id) {
            return Err("channel_id must be a Slack channel id".into());
        }
        if self.team_id.as_deref().is_some_and(|t| !id_like(t)) {
            return Err("team_id must be a Slack workspace id".into());
        }
        validate_ping(self.mention.as_deref())
    }

    fn abuse_url(&self) -> Option<&str> {
        Some(&self.webhook_url)
    }

    fn quiet_broadcast_mention(&mut self) {
        self.mention = without_broadcast_ping(self.mention.as_deref());
    }

    /// The channel id: a press names the channel it came from, never the org.
    fn lifecycle_ref(&self) -> Option<&str> {
        Some(&self.channel_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SlackAppConfig {
        SlackAppConfig {
            webhook_url: "https://hooks.slack.com/services/T0AB12CD3/B0AB12CD3/x".into(),
            channel: "#ops".into(),
            channel_id: "C0AB12CD3".into(),
            team_id: Some("T0AB12CD3".into()),
            mention: None,
        }
    }

    #[test]
    fn only_slack_ids_pass() {
        assert!(cfg().validate().is_ok());
        let mut c = cfg();
        c.channel_id = "#ops".into();
        assert!(c.validate().is_err());
        let mut c = cfg();
        c.team_id = Some("#acme".into());
        assert!(c.validate().is_err());
        c.team_id = None;
        assert!(
            c.validate().is_ok(),
            "an org-wide install names no workspace"
        );
    }

    #[test]
    fn the_webhook_is_the_secret_and_the_channel_is_not() {
        let mut c = cfg();
        c.redact_in_place();
        assert!(c.has_redaction_sentinel());
        assert_eq!(c.channel_id, "C0AB12CD3");
        assert_eq!(c.lifecycle_ref(), Some("C0AB12CD3"));
    }

    #[test]
    fn an_edit_keeps_the_connection_and_takes_only_the_ping() {
        let edit = SlackAppConfig {
            webhook_url: MASK.into(),
            channel: "#elsewhere".into(),
            channel_id: "C0EVIL0001".into(),
            team_id: None,
            mention: Some("S01ABC234".into()),
        }
        .edited_on(&cfg());
        assert_eq!(edit.webhook_url, cfg().webhook_url);
        assert_eq!(edit.channel, "#ops");
        assert_eq!(edit.channel_id, "C0AB12CD3");
        assert_eq!(edit.team_id.as_deref(), Some("T0AB12CD3"));
        assert_eq!(edit.mention.as_deref(), Some("S01ABC234"));
        assert!(edit.validate().is_ok());
    }

    #[test]
    fn a_ping_follows_the_pasted_webhooks_rules() {
        let mut c = cfg();
        c.mention = Some("@sre".into());
        assert!(c.validate().is_err());
        c.mention = Some("@here S01ABC234".into());
        assert!(c.validate().is_ok());
        c.quiet_broadcast_mention();
        assert_eq!(c.mention_markup().as_deref(), Some("<!subteam^S01ABC234>"));
    }
}
