use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::ChannelKind;
use super::transport::{TransportConfig, strip_phone_separators};

/// Destination linked through the operator-owned WhatsApp business number.
/// Secretless (delivery uses the operator's Cloud API credentials) and
/// created exclusively by the webhook consume path — a caller-supplied
/// phone would let anyone alert-spam an arbitrary number through our WABA.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct WhatsAppAppConfig {
    /// Sender's phone in international digits, as Meta reports it in
    /// `messages[].from` (e.g. `15551234567`).
    pub phone: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_name: Option<String>,
}

impl TransportConfig for WhatsAppAppConfig {
    const KIND: ChannelKind = ChannelKind::WhatsAppApp;

    fn redact_in_place(&mut self) {}

    fn has_redaction_sentinel(&self) -> bool {
        false
    }

    /// Digits only here, so a pasted number's leading `+` goes too.
    fn normalize(&mut self) {
        let phone = strip_phone_separators(self.phone.trim());
        self.phone = phone.strip_prefix('+').unwrap_or(&phone).to_string();
    }

    fn validate(&self) -> Result<(), String> {
        let p = self.phone.trim();
        if p.is_empty() {
            return Err("phone is required".into());
        }
        if !(5..=20).contains(&p.len()) || !p.bytes().all(|b| b.is_ascii_digit()) {
            return Err("phone must be international-format digits".into());
        }
        Ok(())
    }

    /// Deliveries go to the fixed graph.facebook.com endpoint — no
    /// customer-controlled URL to inspect.
    fn abuse_url(&self) -> Option<&str> {
        None
    }

    fn quiet_broadcast_mention(&mut self) {}

    /// The phone: an inbound `stop` severs every org linked to the number.
    fn lifecycle_ref(&self) -> Option<&str> {
        Some(&self.phone)
    }
}
