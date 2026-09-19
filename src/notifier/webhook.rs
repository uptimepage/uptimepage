use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::Utc;
use url::Url;

use crate::error::{AppError, Result};
use crate::http_outbound::{OutboundHttpClient, post_bytes_with_headers};
use crate::notifier::Notifier;
use crate::notifier::event::IncidentNotice;

/// Header carrying the signing timestamp (unix seconds) and the signature.
const TIMESTAMP_HEADER: &str = "X-Uptimepage-Timestamp";
const SIGNATURE_HEADER: &str = "X-Uptimepage-Signature";

pub struct WebhookNotifier {
    client: OutboundHttpClient,
    url: Url,
    headers: BTreeMap<String, String>,
    /// HMAC-SHA256 signing key; `None` sends the payload unsigned.
    secret: Option<String>,
}

impl WebhookNotifier {
    pub fn new(
        client: OutboundHttpClient,
        url: Url,
        headers: BTreeMap<String, String>,
        secret: Option<String>,
    ) -> Self {
        Self {
            client,
            url,
            headers,
            secret,
        }
    }

    /// Signature over `"{timestamp}.{body}"`, hex-encoded. To verify: read the
    /// `X-Uptimepage-Timestamp` header, recompute `HMAC-SHA256(secret,
    /// timestamp + "." + raw_request_body)`, and compare in constant time
    /// against the hex after `sha256=`. The timestamp is bound into the digest
    /// for replay protection — the receiver must reject a timestamp outside a
    /// freshness window (e.g. ±5 min) or the binding buys nothing.
    fn sign(secret: &str, timestamp: i64, body: &[u8]) -> String {
        crate::security::mac::webhook_signature(secret, timestamp, body)
    }
}

#[async_trait]
impl Notifier for WebhookNotifier {
    async fn notify_incident(&self, notice: &IncidentNotice) -> Result<()> {
        let payload =
            serde_json::to_vec(notice).map_err(|e| AppError::Other(anyhow::anyhow!("{e}")))?;
        let mut headers = self.headers.clone();
        if let Some(secret) = &self.secret {
            let ts = Utc::now().timestamp();
            // Operator headers can't shadow the signature: insert ours last.
            headers.insert(TIMESTAMP_HEADER.to_string(), ts.to_string());
            headers.insert(
                SIGNATURE_HEADER.to_string(),
                Self::sign(secret, ts, &payload),
            );
        }
        post_bytes_with_headers(&self.client, &self.url, payload, &headers).await
    }
}

#[cfg(test)]
mod tests {
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::Sha256;

    use super::*;

    #[test]
    fn signature_is_stable_and_keyed() {
        let body = br#"{"incident":"x"}"#;
        let a = WebhookNotifier::sign("0123456789abcdef", 1_700_000_000, body);
        let b = WebhookNotifier::sign("0123456789abcdef", 1_700_000_000, body);
        assert_eq!(a, b, "same key + timestamp + body → same signature");
        assert!(a.starts_with("sha256="));

        // A different key, timestamp, or body all change the signature.
        assert_ne!(
            a,
            WebhookNotifier::sign("fedcba9876543210", 1_700_000_000, body)
        );
        assert_ne!(
            a,
            WebhookNotifier::sign("0123456789abcdef", 1_700_000_001, body)
        );
        assert_ne!(
            a,
            WebhookNotifier::sign("0123456789abcdef", 1_700_000_000, b"{}")
        );
    }

    #[test]
    fn signature_matches_a_hand_computed_vector() {
        // Independently recompute HMAC-SHA256("key", "1700000000.{}") to prove
        // the scheme matches the documented verification recipe.
        let sig = WebhookNotifier::sign("supersecretkey16", 1_700_000_000, b"{}");
        let mut mac = Hmac::<Sha256>::new_from_slice(b"supersecretkey16").unwrap();
        mac.update(b"1700000000.{}");
        let expected = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        assert_eq!(sig, expected);
    }
}
