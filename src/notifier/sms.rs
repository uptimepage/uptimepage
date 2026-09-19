use std::collections::BTreeMap;

use async_trait::async_trait;
use base64::Engine;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::domain::SmsConfig;
use crate::error::{AppError, Result};
use crate::http_outbound::{
    OutboundHttpClient, post_form_with_headers, post_json_capture, post_json_with_headers,
};
use crate::notifier::Notifier;
use crate::notifier::event::IncidentNotice;
use crate::notifier::truncate_chars;

// SMS is billed per 160-char (GSM-7) segment. Cap the body so a long monitor
// name plus the incident link can't silently fan out into an expensive
// multi-part message.
const SMS_MAX_CHARS: usize = 480;

const TELNYX_URL: &str = "https://api.telnyx.com/v2/messages";
const VONAGE_URL: &str = "https://rest.nexmo.com/sms/json";

/// One gateway's resolved send recipe. Built once from the stored config.
enum Sender {
    Twilio {
        send_url: Url,
        basic_auth: String,
        from: String,
    },
    Telnyx {
        send_url: Url,
        api_key: String,
        from: String,
        messaging_profile_id: Option<String>,
    },
    Vonage {
        send_url: Url,
        api_key: String,
        api_secret: String,
        from: String,
    },
    Plivo {
        send_url: Url,
        basic_auth: String,
        from: String,
    },
    Sinch {
        send_url: Url,
        api_token: String,
        from: String,
    },
}

pub struct SmsNotifier {
    client: OutboundHttpClient,
    to: String,
    sender: Sender,
}

#[derive(Serialize)]
struct TelnyxMessage<'a> {
    from: &'a str,
    to: &'a str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    messaging_profile_id: Option<&'a str>,
}

#[derive(Serialize)]
struct VonageMessage<'a> {
    api_key: &'a str,
    api_secret: &'a str,
    from: &'a str,
    to: &'a str,
    text: &'a str,
}

#[derive(Deserialize)]
struct VonageResponse {
    messages: Vec<VonageStatus>,
}

#[derive(Deserialize)]
struct VonageStatus {
    status: String,
    #[serde(rename = "error-text")]
    error_text: Option<String>,
}

#[derive(Serialize)]
struct PlivoMessage<'a> {
    src: &'a str,
    dst: &'a str,
    text: &'a str,
}

#[derive(Serialize)]
struct SinchMessage<'a> {
    from: &'a str,
    to: [&'a str; 1],
    body: &'a str,
}

impl SmsNotifier {
    pub fn new(client: OutboundHttpClient, cfg: &SmsConfig) -> Result<Self> {
        let to = cfg.to().to_string();
        let sender = match cfg {
            SmsConfig::Twilio {
                account_sid,
                auth_token,
                from,
                ..
            } => {
                // account_sid is validated `AC` + 32 hex on create, so the
                // path is URL-safe; parsing guards against a stored value that
                // somehow bypassed it.
                let send_url = format!(
                    "https://api.twilio.com/2010-04-01/Accounts/{account_sid}/Messages.json"
                )
                .parse::<Url>()
                .map_err(|e| {
                    AppError::bad_request(
                        crate::error::codes::INVALID_CONFIG,
                        format!("twilio account_sid is not URL-safe: {e}"),
                    )
                })?;
                let basic_auth = base64::engine::general_purpose::STANDARD
                    .encode(format!("{account_sid}:{auth_token}"));
                Sender::Twilio {
                    send_url,
                    basic_auth,
                    from: from.clone(),
                }
            }
            SmsConfig::Telnyx {
                api_key,
                from,
                messaging_profile_id,
                ..
            } => Sender::Telnyx {
                send_url: TELNYX_URL.parse().expect("static telnyx URL parses"),
                api_key: api_key.clone(),
                from: from.clone(),
                messaging_profile_id: messaging_profile_id.clone(),
            },
            SmsConfig::Vonage {
                api_key,
                api_secret,
                from,
                ..
            } => Sender::Vonage {
                send_url: VONAGE_URL.parse().expect("static vonage URL parses"),
                api_key: api_key.clone(),
                api_secret: api_secret.clone(),
                from: from.clone(),
            },
            SmsConfig::Plivo {
                auth_id,
                auth_token,
                from,
                ..
            } => {
                // auth_id is validated alphanumeric on create, so the path is
                // URL-safe; parsing guards a value that bypassed it.
                let send_url = format!("https://api.plivo.com/v1/Account/{auth_id}/Message/")
                    .parse::<Url>()
                    .map_err(|e| {
                        AppError::bad_request(
                            crate::error::codes::INVALID_CONFIG,
                            format!("plivo auth_id is not URL-safe: {e}"),
                        )
                    })?;
                let basic_auth = base64::engine::general_purpose::STANDARD
                    .encode(format!("{auth_id}:{auth_token}"));
                Sender::Plivo {
                    send_url,
                    basic_auth,
                    from: from.clone(),
                }
            }
            SmsConfig::Sinch {
                service_plan_id,
                api_token,
                from,
                region,
                ..
            } => {
                // region is validated against the known set, service_plan_id
                // alphanumeric — both URL-safe; parsing is the guard.
                let send_url =
                    format!("https://{region}.sms.api.sinch.com/xms/v1/{service_plan_id}/batches")
                        .parse::<Url>()
                        .map_err(|e| {
                            AppError::bad_request(
                                crate::error::codes::INVALID_CONFIG,
                                format!("sinch service plan / region is not URL-safe: {e}"),
                            )
                        })?;
                Sender::Sinch {
                    send_url,
                    api_token: api_token.clone(),
                    from: from.clone(),
                }
            }
        };
        Ok(Self { client, to, sender })
    }
}

#[async_trait]
impl Notifier for SmsNotifier {
    async fn notify_incident(&self, notice: &IncidentNotice) -> Result<()> {
        let text = truncate_chars(&notice.plain_text(), SMS_MAX_CHARS);
        match &self.sender {
            Sender::Twilio {
                send_url,
                basic_auth,
                from,
            } => {
                let body = url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("To", &self.to)
                    .append_pair("From", from)
                    .append_pair("Body", &text)
                    .finish();
                let headers =
                    BTreeMap::from([("Authorization".to_string(), format!("Basic {basic_auth}"))]);
                post_form_with_headers(&self.client, send_url, body.into_bytes(), &headers).await
            }
            Sender::Telnyx {
                send_url,
                api_key,
                from,
                messaging_profile_id,
            } => {
                let msg = TelnyxMessage {
                    from,
                    to: &self.to,
                    text: &text,
                    messaging_profile_id: messaging_profile_id.as_deref(),
                };
                let headers =
                    BTreeMap::from([("Authorization".to_string(), format!("Bearer {api_key}"))]);
                post_json_with_headers(&self.client, send_url, &msg, &headers).await
            }
            Sender::Vonage {
                send_url,
                api_key,
                api_secret,
                from,
            } => {
                // Vonage wants the recipient as digits with no leading `+`,
                // and ACKs failures with HTTP 200 + a non-zero status string,
                // so the body must be inspected rather than trusting the code.
                let msg = VonageMessage {
                    api_key,
                    api_secret,
                    from,
                    to: self.to.trim_start_matches('+'),
                    text: &text,
                };
                let resp: VonageResponse = post_json_capture(&self.client, send_url, &msg).await?;
                let first = resp.messages.into_iter().next().ok_or_else(|| {
                    AppError::Other(anyhow::anyhow!("vonage returned no message status"))
                })?;
                if first.status != "0" {
                    return Err(AppError::Other(anyhow::anyhow!(
                        "vonage rejected the message (status {}): {}",
                        first.status,
                        first.error_text.unwrap_or_default()
                    )));
                }
                Ok(())
            }
            Sender::Plivo {
                send_url,
                basic_auth,
                from,
            } => {
                // Plivo wants the destination as digits with no leading `+`.
                let msg = PlivoMessage {
                    src: from,
                    dst: self.to.trim_start_matches('+'),
                    text: &text,
                };
                let headers =
                    BTreeMap::from([("Authorization".to_string(), format!("Basic {basic_auth}"))]);
                post_json_with_headers(&self.client, send_url, &msg, &headers).await
            }
            Sender::Sinch {
                send_url,
                api_token,
                from,
            } => {
                let msg = SinchMessage {
                    from,
                    to: [&self.to],
                    body: &text,
                };
                let headers =
                    BTreeMap::from([("Authorization".to_string(), format!("Bearer {api_token}"))]);
                post_json_with_headers(&self.client, send_url, &msg, &headers).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> OutboundHttpClient {
        crate::http_outbound::build_outbound_client(crate::security::SsrfGuard::relaxed_for_tests())
    }

    #[test]
    fn twilio_builds_account_scoped_url_and_basic_auth() {
        let n = SmsNotifier::new(
            client(),
            &SmsConfig::Twilio {
                to: "+15551234567".into(),
                from: "+15557654321".into(),
                account_sid: "AC0123456789ABCDEF0123456789ABCDEF".into(),
                auth_token: "tok".into(),
            },
        )
        .unwrap();
        let Sender::Twilio {
            send_url,
            basic_auth,
            ..
        } = &n.sender
        else {
            panic!("expected twilio sender");
        };
        assert_eq!(
            send_url.as_str(),
            "https://api.twilio.com/2010-04-01/Accounts/AC0123456789ABCDEF0123456789ABCDEF/Messages.json"
        );
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(basic_auth)
            .unwrap();
        assert_eq!(
            String::from_utf8(decoded).unwrap(),
            "AC0123456789ABCDEF0123456789ABCDEF:tok"
        );
    }

    #[test]
    fn plivo_builds_account_scoped_url_and_basic_auth() {
        let n = SmsNotifier::new(
            client(),
            &SmsConfig::Plivo {
                to: "+15551234567".into(),
                from: "+15557654321".into(),
                auth_id: "MAXXXXXXXXXXXXXXXXXX".into(),
                auth_token: "tok".into(),
            },
        )
        .unwrap();
        let Sender::Plivo {
            send_url,
            basic_auth,
            ..
        } = &n.sender
        else {
            panic!("expected plivo sender");
        };
        assert_eq!(
            send_url.as_str(),
            "https://api.plivo.com/v1/Account/MAXXXXXXXXXXXXXXXXXX/Message/"
        );
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(basic_auth)
            .unwrap();
        assert_eq!(
            String::from_utf8(decoded).unwrap(),
            "MAXXXXXXXXXXXXXXXXXX:tok"
        );
    }

    #[test]
    fn sinch_region_picks_the_cluster_host() {
        let url = |region: &str| {
            let n = SmsNotifier::new(
                client(),
                &SmsConfig::Sinch {
                    to: "+15551234567".into(),
                    from: "Acme".into(),
                    service_plan_id: "abc123".into(),
                    api_token: "tok".into(),
                    region: region.into(),
                },
            )
            .unwrap();
            let Sender::Sinch { send_url, .. } = &n.sender else {
                panic!("expected sinch sender");
            };
            send_url.as_str().to_string()
        };
        assert_eq!(
            url("us"),
            "https://us.sms.api.sinch.com/xms/v1/abc123/batches"
        );
        assert_eq!(
            url("eu"),
            "https://eu.sms.api.sinch.com/xms/v1/abc123/batches"
        );
    }

    #[test]
    fn vonage_strips_leading_plus_from_recipient() {
        let n = SmsNotifier::new(
            client(),
            &SmsConfig::Vonage {
                to: "+15551234567".into(),
                from: "Acme".into(),
                api_key: "a1b2c3d4".into(),
                api_secret: "sek".into(),
            },
        )
        .unwrap();
        let msg = VonageMessage {
            api_key: "a1b2c3d4",
            api_secret: "sek",
            from: "Acme",
            to: n.to.trim_start_matches('+'),
            text: "hi",
        };
        assert_eq!(serde_json::to_value(&msg).unwrap()["to"], "15551234567");
    }
}
