use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::ChannelKind;
use super::transport::{MASK, TransportConfig, require_https, trim_in_place};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct WebhookConfig {
    pub url: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Optional HMAC-SHA256 signing key. When set, each delivery carries an
    /// `X-Uptimepage-Signature` + `X-Uptimepage-Timestamp` the receiver can
    /// verify; absent leaves the POST unsigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

impl TransportConfig for WebhookConfig {
    const KIND: ChannelKind = ChannelKind::Webhook;

    /// The URL can carry a token (…/hooks/T/B/secret) and header values are
    /// routinely `Authorization`, so both are masked along with the key.
    fn redact_in_place(&mut self) {
        self.url = MASK.to_string();
        for v in self.headers.values_mut() {
            *v = MASK.to_string();
        }
        if self.secret.is_some() {
            self.secret = Some(MASK.to_string());
        }
    }

    fn has_redaction_sentinel(&self) -> bool {
        self.url == MASK
            || self.headers.values().any(|v| v == MASK)
            || self.secret.as_deref() == Some(MASK)
    }

    /// The URL only: the secret and the headers are set identically on the
    /// receiving end, where trimming ours would break the match.
    fn normalize(&mut self) {
        trim_in_place(&mut self.url);
    }

    fn validate(&self) -> Result<(), String> {
        require_https(&self.url, "url")?;
        // A short signing key undermines the HMAC; require a meaningful
        // length when one is set (empty/absent stays unsigned).
        if let Some(s) = &self.secret
            && s.len() < 16
        {
            return Err("secret must be at least 16 characters".into());
        }
        Ok(())
    }

    fn abuse_url(&self) -> Option<&str> {
        Some(&self.url)
    }

    fn quiet_broadcast_mention(&mut self) {}
}
