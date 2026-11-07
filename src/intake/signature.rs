//! Grafana's contact point signature: hex HMAC-SHA256 of the timestamp, a
//! colon and the body, in headers the operator names in Grafana. We publish
//! both names and require both, since the timestamp header is optional on
//! Grafana's side and without it a captured request could be replayed.

use chrono::{DateTime, Utc};
use subtle::ConstantTimeEq;

use crate::security::mac::hmac_sha256_hex;

pub const SIGNATURE_HEADER: &str = "X-Grafana-Alerting-Signature";
pub const TIMESTAMP_HEADER: &str = "X-Grafana-Alerting-Timestamp";
/// How far a signed request's timestamp may sit from our clock.
pub const MAX_SKEW_SECS: i64 = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureError {
    Missing,
    Stale,
    Mismatch,
}

/// Checks a signed request. `timestamp` and `signature` are the raw header
/// values, `None` when absent.
pub fn verify_grafana(
    secret: &str,
    timestamp: Option<&str>,
    signature: Option<&str>,
    body: &[u8],
    now: DateTime<Utc>,
) -> Result<(), SignatureError> {
    let (Some(timestamp), Some(signature)) = (timestamp, signature) else {
        return Err(SignatureError::Missing);
    };
    let ts: i64 = timestamp
        .trim()
        .parse()
        .map_err(|_| SignatureError::Mismatch)?;
    if now.timestamp().abs_diff(ts) > MAX_SKEW_SECS.unsigned_abs() {
        return Err(SignatureError::Stale);
    }
    let expected = hmac_sha256_hex(
        secret.as_bytes(),
        &[timestamp.trim().as_bytes(), b":", body],
    );
    let given = signature.trim().to_ascii_lowercase();
    match bool::from(expected.as_bytes().ct_eq(given.as_bytes())) {
        true => Ok(()),
        false => Err(SignatureError::Mismatch),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize)]
    struct Captured {
        secret: String,
        timestamp: String,
        signature: String,
    }

    fn captured() -> (Captured, &'static [u8], DateTime<Utc>) {
        let c: Captured = serde_json::from_str(include_str!(
            "../../tests/fixtures/alerts/grafana_firing.signature.json"
        ))
        .unwrap();
        let at = DateTime::from_timestamp(c.timestamp.parse().unwrap(), 0).unwrap();
        (
            c,
            include_bytes!("../../tests/fixtures/alerts/grafana_firing.json"),
            at,
        )
    }

    #[test]
    fn a_request_grafana_signed_verifies() {
        let (c, body, at) = captured();
        assert_eq!(
            verify_grafana(&c.secret, Some(&c.timestamp), Some(&c.signature), body, at),
            Ok(())
        );
    }

    #[test]
    fn a_changed_body_wrong_secret_or_old_timestamp_is_refused() {
        let (c, body, at) = captured();
        let mut changed = body.to_vec();
        changed.push(b' ');
        let check = |secret: &str, body: &[u8], now| {
            verify_grafana(secret, Some(&c.timestamp), Some(&c.signature), body, now)
        };
        assert_eq!(
            check(&c.secret, &changed, at),
            Err(SignatureError::Mismatch)
        );
        assert_eq!(
            check("another-secret-entirely", body, at),
            Err(SignatureError::Mismatch)
        );
        let later = at + chrono::Duration::seconds(MAX_SKEW_SECS + 1);
        assert_eq!(check(&c.secret, body, later), Err(SignatureError::Stale));
        assert_eq!(
            verify_grafana(&c.secret, None, Some(&c.signature), body, at),
            Err(SignatureError::Missing),
            "no timestamp means no replay protection"
        );
        assert_eq!(
            verify_grafana(
                &c.secret,
                Some("-9223372036854775808"),
                Some(&c.signature),
                body,
                at
            ),
            Err(SignatureError::Stale),
            "an extreme timestamp is refused, not overflowed"
        );
    }
}
