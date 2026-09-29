use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::ChannelKind;
use super::transport::{MASK, TransportConfig, require_https, trim_in_place};

fn default_server_url() -> String {
    "https://ntfy.sh".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct NtfyConfig {
    /// Server root (self-hostable); deliveries publish to it via the JSON
    /// endpoint. Customer-controlled, so it rides the abuse deny-list.
    #[serde(default = "default_server_url")]
    pub server_url: String,
    /// Visible routing, like a chat id — but on ntfy.sh an unprotected
    /// topic's name is its only access control.
    pub topic: String,
    /// Optional access token, sent as `Authorization: Bearer`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
}

impl TransportConfig for NtfyConfig {
    const KIND: ChannelKind = ChannelKind::Ntfy;

    fn redact_in_place(&mut self) {
        if self.access_token.is_some() {
            self.access_token = Some(MASK.to_string());
        }
    }

    fn has_redaction_sentinel(&self) -> bool {
        self.access_token.as_deref() == Some(MASK)
    }

    fn normalize(&mut self) {
        trim_in_place(&mut self.server_url);
        trim_in_place(&mut self.topic);
        if let Some(t) = &mut self.access_token {
            trim_in_place(t);
        }
    }

    fn validate(&self) -> Result<(), String> {
        require_https(&self.server_url, "server_url")?;
        // JSON publishes go to the server ROOT with the topic in the body; a
        // pasted topic URL would publish garbage to the wrong place.
        let parsed = url::Url::parse(&self.server_url)
            .map_err(|_| "server_url is not a valid URL".to_string())?;
        if parsed.path() != "/" || parsed.query().is_some() || parsed.fragment().is_some() {
            return Err(
                "server_url must be the server root — put the topic in the topic field".into(),
            );
        }
        // server_url stays visible after redaction, so inline credentials
        // would be echoed back by the API.
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(
                "server_url must not embed credentials — use the access token field".into(),
            );
        }
        if self.topic.is_empty()
            || self.topic.len() > 64
            || !self
                .topic
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err("topic must be 1-64 characters of letters, digits, _ or -".into());
        }
        if let Some(t) = &self.access_token
            && (t.is_empty() || t.chars().any(|c| c.is_ascii_whitespace()))
        {
            return Err("access_token must not be empty or contain whitespace".into());
        }
        Ok(())
    }

    fn abuse_url(&self) -> Option<&str> {
        Some(&self.server_url)
    }

    fn quiet_broadcast_mention(&mut self) {}
}
