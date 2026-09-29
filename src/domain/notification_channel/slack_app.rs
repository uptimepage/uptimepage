use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::ChannelKind;
use super::slack::id_like;
use super::transport::{MASK, TransportConfig, require_https, trim_in_place};

/// Slack channel connected through our own "Add to Slack" app. A press on a
/// button in its alerts reaches us, which a pasted webhook's never does, so
/// only the connect flow may create one: a caller-supplied channel id would
/// point our presses at a channel the webhook does not post to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SlackAppConfig {
    /// Incoming-webhook URL the install minted. The path carries the
    /// workspace token, so the whole value is treated as a secret.
    pub webhook_url: String,
    /// The channel picked at install, e.g. `#ops-alerts`.
    pub channel: String,
    /// Slack's id for that channel, which every press on its alerts carries.
    pub channel_id: String,
    /// The workspace the app was installed into, when Slack names one: an
    /// install across an Enterprise Grid org has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
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
    }

    fn validate(&self) -> Result<(), String> {
        require_https(&self.webhook_url, "webhook_url")?;
        if !id_like(&self.channel_id) {
            return Err("channel_id must be a Slack channel id".into());
        }
        if self.team_id.as_deref().is_some_and(|t| !id_like(t)) {
            return Err("team_id must be a Slack workspace id".into());
        }
        Ok(())
    }

    fn abuse_url(&self) -> Option<&str> {
        Some(&self.webhook_url)
    }

    fn quiet_broadcast_mention(&mut self) {}

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
}
