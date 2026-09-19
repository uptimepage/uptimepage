//! Operator WhatsApp number (Meta Cloud API): webhook payload model,
//! delivery-signature check, and the free-form reply used inside the 24-hour
//! service window a user opens by messaging us. Alert delivery stays in
//! `crate::notifier::whatsapp` (template sends).

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::Serialize;
use subtle::ConstantTimeEq;
use url::Url;

use crate::error::{AppError, Result};
use crate::http_outbound::{OutboundHttpClient, post_json_with_headers};

pub const WEBHOOK_PATH: &str = "/hooks/whatsapp";
const GRAPH_API_VERSION: &str = "v23.0";

/// `X-Hub-Signature-256: sha256=<hex HMAC-SHA256(app_secret, raw_body)>`.
pub fn signature_matches(app_secret: &str, header: &str, body: &[u8]) -> bool {
    let Some(hex_sig) = header.strip_prefix("sha256=") else {
        return false;
    };
    // Compare lowercased: providers may send upper/mixed-case hex.
    let provided = hex_sig.to_ascii_lowercase();
    let expected = crate::auth::mac::hmac_sha256_hex(app_secret.as_bytes(), &[body]);
    provided.as_bytes().ct_eq(expected.as_bytes()).into()
}

// ── webhook payload (the slice of it we consume) ────────────────────────

#[derive(Debug, Deserialize)]
pub struct Notification {
    #[serde(default)]
    pub entry: Vec<Entry>,
}

#[derive(Debug, Deserialize)]
pub struct Entry {
    #[serde(default)]
    pub changes: Vec<Change>,
}

#[derive(Debug, Deserialize)]
pub struct Change {
    pub value: ChangeValue,
}

#[derive(Debug, Default, Deserialize)]
pub struct ChangeValue {
    pub metadata: Option<Metadata>,
    #[serde(default)]
    pub contacts: Vec<Contact>,
    #[serde(default)]
    pub messages: Vec<Message>,
}

#[derive(Debug, Deserialize)]
pub struct Metadata {
    #[serde(default)]
    pub phone_number_id: String,
}

#[derive(Debug, Deserialize)]
pub struct Contact {
    #[serde(default)]
    pub wa_id: String,
    pub profile: Option<Profile>,
}

#[derive(Debug, Deserialize)]
pub struct Profile {
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Message {
    #[serde(default)]
    pub from: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    pub text: Option<Text>,
}

#[derive(Debug, Deserialize)]
pub struct Text {
    #[serde(default)]
    pub body: String,
}

/// One inbound text message, classified.
#[derive(Debug, PartialEq)]
pub enum InboundAction {
    /// A link-code candidate (any non-`stop` text body).
    Link {
        code: String,
        from: String,
        profile_name: Option<String>,
    },
    /// Meta-mandated opt-out: sever every channel bound to the sender.
    Stop { from: String },
}

/// Flattens a webhook notification into the actions we act on. Only
/// `messages` changes addressed to `our_phone_number_id` count — one Meta
/// app can serve several numbers, and statuses/receipts are ignored.
pub fn classify(n: &Notification, our_phone_number_id: &str) -> Vec<InboundAction> {
    let mut out = Vec::new();
    for entry in &n.entry {
        for change in &entry.changes {
            let v = &change.value;
            if v.metadata
                .as_ref()
                .is_none_or(|m| m.phone_number_id != our_phone_number_id)
            {
                continue;
            }
            for m in &v.messages {
                if m.kind != "text" || m.from.is_empty() {
                    continue;
                }
                let body = m.text.as_ref().map(|t| t.body.trim()).unwrap_or_default();
                if body.is_empty() {
                    continue;
                }
                if body.eq_ignore_ascii_case("stop") {
                    out.push(InboundAction::Stop {
                        from: m.from.clone(),
                    });
                    continue;
                }
                let profile_name = v
                    .contacts
                    .iter()
                    .find(|c| c.wa_id == m.from)
                    .and_then(|c| c.profile.as_ref())
                    .and_then(|p| p.name.clone())
                    .map(|n| n.trim().to_string())
                    .filter(|n| !n.is_empty());
                out.push(InboundAction::Link {
                    code: body.to_string(),
                    from: m.from.clone(),
                    profile_name,
                });
            }
        }
    }
    out
}

// ── free-form reply (inside the user-opened 24 h window) ────────────────

#[derive(Serialize)]
struct TextSend<'a> {
    messaging_product: &'static str,
    to: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    text: TextBody<'a>,
}

#[derive(Serialize)]
struct TextBody<'a> {
    body: &'a str,
}

/// One free-form text to `to`. Only valid inside the service window the
/// user just opened by messaging us — link confirmations qualify, alerts
/// never use this path.
pub async fn send_text(
    http: &OutboundHttpClient,
    access_token: &str,
    phone_number_id: &str,
    to: &str,
    body: &str,
) -> Result<()> {
    let url = format!("https://graph.facebook.com/{GRAPH_API_VERSION}/{phone_number_id}/messages")
        .parse::<Url>()
        .map_err(|e| {
            AppError::bad_request(
                crate::error::codes::INVALID_CONFIG,
                format!("whatsapp phone_number_id is not URL-safe: {e}"),
            )
        })?;
    let headers = BTreeMap::from([(
        "Authorization".to_string(),
        format!("Bearer {access_token}"),
    )]);
    post_json_with_headers(
        http,
        &url,
        &TextSend {
            messaging_product: "whatsapp",
            to,
            kind: "text",
            text: TextBody { body },
        },
        &headers,
    )
    .await
}

#[cfg(test)]
mod tests {
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::Sha256;

    use super::*;

    #[test]
    fn signature_round_trips_and_rejects_tampering() {
        let body = br#"{"object":"whatsapp_business_account"}"#;
        let mut mac = Hmac::<Sha256>::new_from_slice(b"app-secret").unwrap();
        mac.update(body);
        let hex_sig = hex::encode(mac.finalize().into_bytes());
        let header = format!("sha256={hex_sig}");
        assert!(signature_matches("app-secret", &header, body));
        // Hex case is not significant — a provider may send upper/mixed case.
        assert!(signature_matches(
            "app-secret",
            &format!("sha256={}", hex_sig.to_uppercase()),
            body
        ));
        assert!(!signature_matches("app-secret", &header, b"tampered"));
        assert!(!signature_matches("other-secret", &header, body));
        assert!(!signature_matches("app-secret", "sha256=zz", body));
        assert!(!signature_matches("app-secret", "md5=abcd", body));
        assert!(!signature_matches("app-secret", "", body));
    }

    fn sample(phone_number_id: &str, kind: &str, body: &str) -> Notification {
        serde_json::from_value(serde_json::json!({
            "object": "whatsapp_business_account",
            "entry": [{ "id": "1", "changes": [{ "field": "messages", "value": {
                "messaging_product": "whatsapp",
                "metadata": { "display_phone_number": "15550001111", "phone_number_id": phone_number_id },
                "contacts": [{ "wa_id": "15551234567", "profile": { "name": "Jane" } }],
                "messages": [{ "from": "15551234567", "id": "wamid.X", "timestamp": "0",
                               "type": kind, "text": { "body": body } }]
            }}]}]
        }))
        .unwrap()
    }

    #[test]
    fn classify_links_stops_and_filters() {
        let n = sample("42", "text", "  ABC123  ");
        assert_eq!(
            classify(&n, "42"),
            vec![InboundAction::Link {
                code: "ABC123".into(),
                from: "15551234567".into(),
                profile_name: Some("Jane".into()),
            }]
        );
        assert_eq!(
            classify(&sample("42", "text", "Stop"), "42"),
            vec![InboundAction::Stop {
                from: "15551234567".into()
            }]
        );
        // Foreign number, non-text kinds, and empty bodies are ignored.
        assert!(classify(&sample("42", "text", "x"), "43").is_empty());
        assert!(classify(&sample("42", "image", "x"), "42").is_empty());
        assert!(classify(&sample("42", "text", "   "), "42").is_empty());
    }

    #[test]
    fn statuses_only_payload_is_ignored() {
        let n: Notification = serde_json::from_value(serde_json::json!({
            "object": "whatsapp_business_account",
            "entry": [{ "id": "1", "changes": [{ "field": "messages", "value": {
                "metadata": { "phone_number_id": "42" },
                "statuses": [{ "id": "wamid.X", "status": "delivered" }]
            }}]}]
        }))
        .unwrap();
        assert!(classify(&n, "42").is_empty());
    }
}
