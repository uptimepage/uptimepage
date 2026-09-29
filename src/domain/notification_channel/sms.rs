use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::ChannelKind;
use super::transport::{MASK, TransportConfig, strip_phone_separators, trim_in_place};

/// BYO-token SMS. One channel kind, many gateways: each variant carries that
/// gateway's own credentials, so a future operator-billed pool is an
/// additive variant rather than a new kind. `provider` is the wire tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum SmsConfig {
    Twilio {
        to: String,
        from: String,
        /// Account SID — an identifier, not a secret.
        account_sid: String,
        auth_token: String,
    },
    Telnyx {
        to: String,
        from: String,
        /// Bearer key — the secret.
        api_key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        messaging_profile_id: Option<String>,
    },
    Vonage {
        to: String,
        from: String,
        /// Public account key — an identifier, not a secret.
        api_key: String,
        api_secret: String,
    },
    Plivo {
        to: String,
        from: String,
        /// Auth ID — an identifier, not a secret.
        auth_id: String,
        auth_token: String,
    },
    Sinch {
        to: String,
        from: String,
        /// Service plan id — an identifier, not a secret.
        service_plan_id: String,
        api_token: String,
        /// Sinch serves region-specific clusters; the wrong one fails auth.
        #[serde(default = "default_sinch_region")]
        region: String,
    },
}

fn default_sinch_region() -> String {
    "us".to_string()
}

/// Sinch REST regions (each a distinct host); keep in step with the notifier.
pub const SINCH_REGIONS: &[&str] = &["us", "eu", "au", "br", "ca"];

/// `+` followed by 8–15 digits (E.164 max is 15 significant digits).
fn is_e164(s: &str) -> bool {
    let Some(rest) = s.strip_prefix('+') else {
        return false;
    };
    (8..=15).contains(&rest.len()) && rest.bytes().all(|b| b.is_ascii_digit())
}

/// Sender: an E.164 number, an alphanumeric sender id, or a messaging-service
/// id (Twilio `MG…`). Gateways diverge on the exact rules; this keeps out
/// whitespace and metacharacters without rejecting valid sender ids.
fn is_sender_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 34
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.' | b'_'))
}

/// Non-empty, no whitespace — the shape every gateway token shares.
fn is_token(s: &str) -> bool {
    !s.is_empty() && !s.chars().any(char::is_whitespace)
}

fn is_twilio_sid(s: &str) -> bool {
    s.len() == 34 && s.starts_with("AC") && s[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// Non-empty and alphanumeric — for ids that ride in a request path (Plivo
/// auth id, Sinch service plan id), so they can't smuggle URL metacharacters.
fn is_path_id(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

impl SmsConfig {
    pub fn to(&self) -> &str {
        match self {
            Self::Twilio { to, .. }
            | Self::Telnyx { to, .. }
            | Self::Vonage { to, .. }
            | Self::Plivo { to, .. }
            | Self::Sinch { to, .. } => to,
        }
    }

    pub fn from(&self) -> &str {
        match self {
            Self::Twilio { from, .. }
            | Self::Telnyx { from, .. }
            | Self::Vonage { from, .. }
            | Self::Plivo { from, .. }
            | Self::Sinch { from, .. } => from,
        }
    }
}

impl TransportConfig for SmsConfig {
    const KIND: ChannelKind = ChannelKind::Sms;

    fn redact_in_place(&mut self) {
        match self {
            Self::Twilio { auth_token, .. } | Self::Plivo { auth_token, .. } => {
                *auth_token = MASK.to_string()
            }
            Self::Telnyx { api_key, .. } => *api_key = MASK.to_string(),
            Self::Vonage { api_secret, .. } => *api_secret = MASK.to_string(),
            Self::Sinch { api_token, .. } => *api_token = MASK.to_string(),
        }
    }

    fn has_redaction_sentinel(&self) -> bool {
        match self {
            Self::Twilio { auth_token, .. } | Self::Plivo { auth_token, .. } => auth_token == MASK,
            Self::Telnyx { api_key, .. } => api_key == MASK,
            Self::Vonage { api_secret, .. } => api_secret == MASK,
            Self::Sinch { api_token, .. } => api_token == MASK,
        }
    }

    /// The sender is left alone: an alphanumeric sender id may carry a dash,
    /// and stripping it would send from the wrong name.
    fn normalize(&mut self) {
        match self {
            Self::Twilio {
                to,
                from,
                account_sid,
                auth_token,
            } => {
                for f in [&mut *from, &mut *account_sid, &mut *auth_token] {
                    trim_in_place(f);
                }
                *to = strip_phone_separators(to.trim());
            }
            Self::Telnyx {
                to,
                from,
                api_key,
                messaging_profile_id,
            } => {
                for f in [&mut *from, &mut *api_key] {
                    trim_in_place(f);
                }
                if let Some(id) = messaging_profile_id {
                    trim_in_place(id);
                }
                *to = strip_phone_separators(to.trim());
            }
            Self::Vonage {
                to,
                from,
                api_key,
                api_secret,
            } => {
                for f in [&mut *from, &mut *api_key, &mut *api_secret] {
                    trim_in_place(f);
                }
                *to = strip_phone_separators(to.trim());
            }
            Self::Plivo {
                to,
                from,
                auth_id,
                auth_token,
            } => {
                for f in [&mut *from, &mut *auth_id, &mut *auth_token] {
                    trim_in_place(f);
                }
                *to = strip_phone_separators(to.trim());
            }
            Self::Sinch {
                to,
                from,
                service_plan_id,
                api_token,
                region,
            } => {
                for f in [
                    &mut *from,
                    &mut *service_plan_id,
                    &mut *api_token,
                    &mut *region,
                ] {
                    trim_in_place(f);
                }
                *to = strip_phone_separators(to.trim());
            }
        }
    }

    fn validate(&self) -> Result<(), String> {
        if !is_e164(self.to()) {
            return Err("recipient must be in E.164 format, e.g. +15551234567".into());
        }
        if !is_sender_id(self.from()) {
            return Err(
                "sender must be an E.164 number, sender id, or messaging-service id".into(),
            );
        }
        match self {
            Self::Twilio {
                account_sid,
                auth_token,
                ..
            } => {
                if !is_twilio_sid(account_sid) {
                    return Err("account_sid must be a Twilio Account SID (AC + 32 hex)".into());
                }
                if !is_token(auth_token) {
                    return Err("auth_token is required".into());
                }
            }
            Self::Telnyx {
                api_key,
                messaging_profile_id,
                ..
            } => {
                if !is_token(api_key) {
                    return Err("api_key is required".into());
                }
                if let Some(p) = messaging_profile_id
                    && !is_token(p)
                {
                    return Err("messaging_profile_id must not be blank or contain spaces".into());
                }
            }
            Self::Vonage {
                api_key,
                api_secret,
                ..
            } => {
                if !is_token(api_key) {
                    return Err("api_key is required".into());
                }
                if !is_token(api_secret) {
                    return Err("api_secret is required".into());
                }
            }
            Self::Plivo {
                auth_id,
                auth_token,
                ..
            } => {
                if !is_path_id(auth_id) {
                    return Err("auth_id must be the alphanumeric Plivo Auth ID".into());
                }
                if !is_token(auth_token) {
                    return Err("auth_token is required".into());
                }
            }
            Self::Sinch {
                service_plan_id,
                api_token,
                region,
                ..
            } => {
                if !is_path_id(service_plan_id) {
                    return Err(
                        "service_plan_id must be the alphanumeric Sinch service plan id".into(),
                    );
                }
                if !is_token(api_token) {
                    return Err("api_token is required".into());
                }
                if !SINCH_REGIONS.contains(&region.as_str()) {
                    return Err("region must be one of us, eu, au, br, ca".into());
                }
            }
        }
        Ok(())
    }

    fn abuse_url(&self) -> Option<&str> {
        None
    }

    fn quiet_broadcast_mention(&mut self) {}
}
