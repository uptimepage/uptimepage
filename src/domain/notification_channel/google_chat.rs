use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::ChannelKind;
use super::transport::{MASK, TransportConfig, require_provider_webhook, trim_in_place};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct GoogleChatConfig {
    /// Space webhook URL. The `key`/`token` query params are the secret, so
    /// the whole value is masked.
    pub webhook_url: String,
}

impl TransportConfig for GoogleChatConfig {
    const KIND: ChannelKind = ChannelKind::GoogleChat;

    fn redact_in_place(&mut self) {
        self.webhook_url = MASK.to_string();
    }

    fn has_redaction_sentinel(&self) -> bool {
        self.webhook_url == MASK
    }

    fn normalize(&mut self) {
        trim_in_place(&mut self.webhook_url);
    }

    fn validate(&self) -> Result<(), String> {
        require_provider_webhook(
            &self.webhook_url,
            "Google Chat",
            &["chat.googleapis.com"],
            None,
        )
    }

    fn abuse_url(&self) -> Option<&str> {
        Some(&self.webhook_url)
    }

    fn quiet_broadcast_mention(&mut self) {}
}
