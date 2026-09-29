use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::ChannelKind;
use super::transport::{MASK, TransportConfig, strip_phone_separators, trim_in_place};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct WhatsAppConfig {
    /// WhatsApp Business Cloud API access token (secret).
    pub access_token: String,
    /// The business phone-number id messages are sent from.
    pub phone_number_id: String,
    /// Recipient phone number in international format, e.g. `15551234567`.
    pub to: String,
    /// Pre-approved template that carries the alert text as its single body
    /// parameter. Required: free-form text only delivers inside the
    /// 24-hour service window, and out-of-window sends are accepted by the
    /// API and dropped asynchronously — an alerting channel can't be
    /// allowed to fail silently like that.
    pub template_name: String,
    /// Template language code (e.g. `en`, `en_US`). Defaults to `en`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language_code: Option<String>,
}

impl TransportConfig for WhatsAppConfig {
    const KIND: ChannelKind = ChannelKind::WhatsApp;

    fn redact_in_place(&mut self) {
        self.access_token = MASK.to_string();
    }

    fn has_redaction_sentinel(&self) -> bool {
        self.access_token == MASK
    }

    fn normalize(&mut self) {
        trim_in_place(&mut self.access_token);
        trim_in_place(&mut self.phone_number_id);
        trim_in_place(&mut self.template_name);
        if let Some(l) = &mut self.language_code {
            trim_in_place(l);
        }
        self.to = strip_phone_separators(self.to.trim());
    }

    /// Judged on the normalized value, but still strictly: what survives here
    /// is sent to the API verbatim, so stray whitespace inside a token has to
    /// fail now rather than at the first delivery.
    fn validate(&self) -> Result<(), String> {
        if self.access_token.is_empty() || !self.access_token.chars().all(|c| c.is_ascii_graphic())
        {
            return Err(
                "access_token is required and must not contain spaces or line breaks".into(),
            );
        }
        if self.phone_number_id.is_empty()
            || !self.phone_number_id.chars().all(|c| c.is_ascii_digit())
        {
            return Err("phone_number_id must be the numeric Cloud API id".into());
        }
        let digits = self.to.strip_prefix('+').unwrap_or(&self.to);
        if !(6..=15).contains(&digits.len()) || !digits.chars().all(|c| c.is_ascii_digit()) {
            return Err("to must be a phone number in international format".into());
        }
        if self.template_name.is_empty()
            || !self
                .template_name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            return Err("template_name is required (lowercase letters, digits, and _ only)".into());
        }
        if let Some(l) = &self.language_code
            && (l.is_empty() || !l.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        {
            return Err("language_code must be a code like en or en_US".into());
        }
        Ok(())
    }

    /// Deliveries go to the fixed graph.facebook.com endpoint — no
    /// customer-controlled URL to inspect.
    fn abuse_url(&self) -> Option<&str> {
        None
    }

    fn quiet_broadcast_mention(&mut self) {}
}
