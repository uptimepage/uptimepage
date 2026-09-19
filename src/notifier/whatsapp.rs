use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::Serialize;
use url::Url;

use crate::domain::WhatsAppConfig;
use crate::error::{AppError, Result};
use crate::http_outbound::{OutboundHttpClient, post_json_with_headers};
use crate::notifier::Notifier;
use crate::notifier::event::IncidentNotice;

const GRAPH_API_VERSION: &str = "v23.0";

/// WhatsApp Business Cloud API sender. The alert text rides as the single
/// body parameter of a pre-approved template — the only delivery mode that
/// works outside the 24-hour service window (free-form sends out of window
/// are accepted by the API and dropped asynchronously, which an alerting
/// channel can't tolerate).
pub struct WhatsAppNotifier {
    client: OutboundHttpClient,
    send_url: Url,
    access_token: String,
    to: String,
    template_name: String,
    language_code: String,
}

#[derive(Serialize)]
struct SendMessage<'a> {
    messaging_product: &'static str,
    to: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    template: Template<'a>,
}

#[derive(Serialize)]
struct Template<'a> {
    name: &'a str,
    language: Language<'a>,
    components: [Component<'a>; 1],
}

#[derive(Serialize)]
struct Language<'a> {
    code: &'a str,
}

#[derive(Serialize)]
struct Component<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    parameters: [Parameter<'a>; 1],
}

#[derive(Serialize)]
struct Parameter<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
}

impl WhatsAppNotifier {
    pub fn new(client: OutboundHttpClient, cfg: &WhatsAppConfig) -> Result<Self> {
        // Host is fixed; only the numeric phone-number id (validated
        // digits-only on channel create) varies. Parsing guards against an
        // id with URL metacharacters reaching the path.
        let send_url = format!(
            "https://graph.facebook.com/{GRAPH_API_VERSION}/{}/messages",
            cfg.phone_number_id
        )
        .parse::<Url>()
        .map_err(|e| {
            AppError::bad_request(
                crate::error::codes::INVALID_CONFIG,
                format!("whatsapp phone_number_id is not URL-safe: {e}"),
            )
        })?;
        Ok(Self {
            client,
            send_url,
            access_token: cfg.access_token.clone(),
            to: cfg.to.clone(),
            template_name: cfg.template_name.clone(),
            language_code: cfg.language_code.clone().unwrap_or_else(|| "en".into()),
        })
    }

    fn message<'a>(&'a self, text: &'a str) -> SendMessage<'a> {
        SendMessage {
            messaging_product: "whatsapp",
            to: &self.to,
            kind: "template",
            template: Template {
                name: &self.template_name,
                language: Language {
                    code: &self.language_code,
                },
                components: [Component {
                    kind: "body",
                    parameters: [Parameter { kind: "text", text }],
                }],
            },
        }
    }
}

#[async_trait]
impl Notifier for WhatsAppNotifier {
    async fn notify_incident(&self, notice: &IncidentNotice) -> Result<()> {
        // Template body parameters must be a single line — the API rejects
        // newlines/tabs and runs of spaces — so the multi-line plain text
        // (link, region breakdown) collapses to one.
        let text = notice
            .plain_text()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let headers = BTreeMap::from([(
            "Authorization".to_string(),
            format!("Bearer {}", self.access_token),
        )]);
        post_json_with_headers(&self.client, &self.send_url, &self.message(&text), &headers).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notifier() -> WhatsAppNotifier {
        WhatsAppNotifier::new(
            crate::http_outbound::build_outbound_client(
                crate::security::SsrfGuard::relaxed_for_tests(),
            ),
            &WhatsAppConfig {
                access_token: "tok".into(),
                phone_number_id: "12345".into(),
                to: "15551234567".into(),
                template_name: "uptime_alert".into(),
                language_code: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn message_matches_cloud_api_wire_shape() {
        let n = notifier();
        assert_eq!(
            n.send_url.as_str(),
            "https://graph.facebook.com/v23.0/12345/messages"
        );
        let v = serde_json::to_value(n.message("api-prod — major incident OPEN")).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "messaging_product": "whatsapp",
                "to": "15551234567",
                "type": "template",
                "template": {
                    "name": "uptime_alert",
                    "language": { "code": "en" },
                    "components": [{
                        "type": "body",
                        "parameters": [{ "type": "text", "text": "api-prod — major incident OPEN" }]
                    }]
                }
            })
        );
    }
}
