use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::ChannelKind;
use super::transport::{TransportConfig, trim_in_place};

/// Destination linked through the central operator-owned Telegram bot.
/// Secretless (delivery uses the operator token) and created exclusively by
/// the webhook consume path — a caller-supplied chat_id would let anyone
/// alert-spam an arbitrary chat through our bot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TelegramAppConfig {
    pub chat_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_title: Option<String>,
}

impl TransportConfig for TelegramAppConfig {
    const KIND: ChannelKind = ChannelKind::TelegramApp;

    fn redact_in_place(&mut self) {}

    fn has_redaction_sentinel(&self) -> bool {
        false
    }

    fn normalize(&mut self) {
        trim_in_place(&mut self.chat_id);
    }

    fn validate(&self) -> Result<(), String> {
        if self.chat_id.trim().is_empty() {
            return Err("chat_id is required".into());
        }
        if self.chat_id.parse::<i64>().is_err() {
            return Err("chat_id must be a Telegram numeric chat id".into());
        }
        Ok(())
    }

    /// Deliveries go to the fixed api.telegram.org endpoint — no
    /// customer-controlled URL to inspect.
    fn abuse_url(&self) -> Option<&str> {
        None
    }

    fn quiet_broadcast_mention(&mut self) {}

    /// The chat id: a kick/stop on the Telegram side severs every org
    /// linked to the chat.
    fn lifecycle_ref(&self) -> Option<&str> {
        Some(&self.chat_id)
    }
}
