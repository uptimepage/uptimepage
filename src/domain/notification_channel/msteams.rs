use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::ChannelKind;
use super::transport::{MASK, TransportConfig, require_provider_webhook, trim_in_place};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct MsTeamsConfig {
    /// Teams Workflows webhook URL. The query carries the signature, so the
    /// whole value is treated as a secret.
    pub webhook_url: String,
}

impl TransportConfig for MsTeamsConfig {
    const KIND: ChannelKind = ChannelKind::MsTeams;

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
        // Workflows hosts are env-specific (`prod-XX.<region>.logic.azure.com`).
        require_provider_webhook(
            &self.webhook_url,
            "Teams",
            &["logic.azure.com", "powerplatform.com"],
            None,
        )
    }

    fn abuse_url(&self) -> Option<&str> {
        Some(&self.webhook_url)
    }

    fn quiet_broadcast_mention(&mut self) {}
}
