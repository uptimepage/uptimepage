use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::ChannelKind;
use super::transport::TransportConfig;

pub const MAX_EMAIL_LEN: usize = 254;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct EmailConfig {
    /// Recipient address. One per channel; a second address is a second
    /// channel. Not a secret — it stays visible so the UI can show where
    /// alerts go.
    pub to: String,
}

impl TransportConfig for EmailConfig {
    const KIND: ChannelKind = ChannelKind::Email;

    fn redact_in_place(&mut self) {}

    fn has_redaction_sentinel(&self) -> bool {
        false
    }

    /// Lowercased like signup already stores an owner address, so the two
    /// paths stop disagreeing about one mailbox.
    fn normalize(&mut self) {
        self.to = self.to.trim().to_ascii_lowercase();
    }

    fn validate(&self) -> Result<(), String> {
        let a = &self.to;
        if a.is_empty() {
            return Err("to address is required".into());
        }
        if a.len() > MAX_EMAIL_LEN {
            return Err(format!(
                "to address must be at most {MAX_EMAIL_LEN} characters"
            ));
        }
        if a.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("to address must not contain whitespace".into());
        }
        if a.chars().any(|c| c.is_ascii_uppercase()) {
            return Err("to address must be lowercase".into());
        }
        let Some((local, domain)) = a.split_once('@') else {
            return Err("to address is not a valid email".into());
        };
        if local.is_empty() || local.len() > 64 || domain.contains('@') {
            return Err("to address is not a valid email".into());
        }
        if !domain.contains('.')
            || domain.contains("..")
            || domain.starts_with('.')
            || domain.ends_with('.')
        {
            return Err("to address is not a valid email".into());
        }
        Ok(())
    }

    fn abuse_url(&self) -> Option<&str> {
        None
    }

    fn quiet_broadcast_mention(&mut self) {}

    /// The address itself: a provider bounce/complaint must find and
    /// disable every channel pointed at it.
    fn lifecycle_ref(&self) -> Option<&str> {
        Some(&self.to)
    }
}
