use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::ChannelKind;
use super::transport::{MASK, TransportConfig, trim_in_place};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PagerDutyConfig {
    /// Events API v2 integration key of a PagerDuty service; a bearer send
    /// capability, so the whole value is treated as a secret.
    pub routing_key: String,
}

impl TransportConfig for PagerDutyConfig {
    const KIND: ChannelKind = ChannelKind::PagerDuty;

    fn redact_in_place(&mut self) {
        self.routing_key = MASK.to_string();
    }

    fn has_redaction_sentinel(&self) -> bool {
        self.routing_key == MASK
    }

    fn normalize(&mut self) {
        trim_in_place(&mut self.routing_key);
    }

    fn validate(&self) -> Result<(), String> {
        let k = &self.routing_key;
        if k.len() != 32 || !k.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(
                "routing_key must be the 32-character Events API v2 integration key of \
                        a PagerDuty service"
                    .into(),
            );
        }
        Ok(())
    }

    fn abuse_url(&self) -> Option<&str> {
        None
    }

    fn quiet_broadcast_mention(&mut self) {}
}
