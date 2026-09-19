//! Shared HMAC-SHA256 so the webhook signer, WhatsApp verifier, and subscriber
//! unsubscribe token agree on one keyed-MAC convention.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

/// HMAC-SHA256 of `parts` concatenated in order, hex-encoded.
pub fn hmac_sha256_hex(key: &[u8], parts: &[&[u8]]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts a key of any length");
    for part in parts {
        mac.update(part);
    }
    hex::encode(mac.finalize().into_bytes())
}

/// `sha256=<hex>` over `"{timestamp}.{body}"` — the webhook delivery signature
/// shared by the operator notifier and public-subscriber webhooks. The receiver
/// recomputes it and rejects a timestamp outside its freshness window.
pub fn webhook_signature(secret: &str, timestamp: i64, body: &[u8]) -> String {
    let ts = timestamp.to_string();
    format!(
        "sha256={}",
        hmac_sha256_hex(secret.as_bytes(), &[ts.as_bytes(), b".", body])
    )
}
