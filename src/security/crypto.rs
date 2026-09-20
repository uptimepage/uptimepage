use std::sync::Arc;

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use thiserror::Error;

use crate::config::SecurityConfig;

const VERSION: &str = "v1";

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("invalid KEK: expected 32 bytes after base64 decode, got {0}")]
    InvalidKekLength(usize),
    #[error("invalid KEK base64: {0}")]
    InvalidKekBase64(base64::DecodeError),
    #[error("envelope must start with '{VERSION}:'")]
    BadVersion,
    #[error("envelope must be three colon-delimited parts")]
    BadShape,
    #[error("invalid base64 in envelope: {0}")]
    InvalidBase64(base64::DecodeError),
    #[error("nonce must be 12 bytes, got {0}")]
    InvalidNonceLength(usize),
    #[error("decryption failed (wrong KEK or tampered ciphertext)")]
    DecryptFailed,
    #[error("encryption failed")]
    EncryptFailed,
}

pub struct Cipher {
    aead: Aes256Gcm,
}

impl std::fmt::Debug for Cipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cipher").finish_non_exhaustive()
    }
}

impl Cipher {
    /// `None` means no KEK, so secrets are stored in plaintext. Every store
    /// must be handed the same one or a row sealed by one process fails to
    /// open in another.
    pub fn from_config(cfg: &SecurityConfig) -> crate::error::Result<Option<Arc<Self>>> {
        match cfg.kek() {
            Some(kek) => Ok(Some(Arc::new(Self::from_base64(kek).map_err(|e| {
                crate::error::AppError::Other(anyhow::anyhow!(
                    "invalid credentials_kek_base64: {e}"
                ))
            })?))),
            None => Ok(None),
        }
    }

    pub fn from_base64(s: &str) -> Result<Self, CryptoError> {
        let bytes = decode_base64_flexible(s).map_err(CryptoError::InvalidKekBase64)?;
        if bytes.len() != 32 {
            return Err(CryptoError::InvalidKekLength(bytes.len()));
        }
        let key = Key::<Aes256Gcm>::from_slice(&bytes);
        Ok(Self {
            aead: Aes256Gcm::new(key),
        })
    }

    pub fn encrypt(&self, plaintext: &[u8]) -> Result<String, CryptoError> {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ct = self
            .aead
            .encrypt(&nonce, plaintext)
            .map_err(|_| CryptoError::EncryptFailed)?;
        Ok(format!(
            "{VERSION}:{}:{}",
            URL_SAFE_NO_PAD.encode(nonce),
            URL_SAFE_NO_PAD.encode(&ct),
        ))
    }

    pub fn decrypt(&self, envelope: &str) -> Result<Vec<u8>, CryptoError> {
        let mut parts = envelope.splitn(3, ':');
        let version = parts.next().ok_or(CryptoError::BadShape)?;
        let nonce_b64 = parts.next().ok_or(CryptoError::BadShape)?;
        let ct_b64 = parts.next().ok_or(CryptoError::BadShape)?;
        if version != VERSION {
            return Err(CryptoError::BadVersion);
        }
        let nonce = URL_SAFE_NO_PAD
            .decode(nonce_b64)
            .map_err(CryptoError::InvalidBase64)?;
        let ct = URL_SAFE_NO_PAD
            .decode(ct_b64)
            .map_err(CryptoError::InvalidBase64)?;
        if nonce.len() != 12 {
            return Err(CryptoError::InvalidNonceLength(nonce.len()));
        }
        let nonce = Nonce::from_slice(&nonce);
        self.aead
            .decrypt(nonce, ct.as_ref())
            .map_err(|_| CryptoError::DecryptFailed)
    }
}

/// Accept both URL-safe-no-pad and standard base64 KEKs for operator convenience.
fn decode_base64_flexible(s: &str) -> Result<Vec<u8>, base64::DecodeError> {
    let trimmed = s.trim();
    if let Ok(bytes) = URL_SAFE_NO_PAD.decode(trimmed) {
        return Ok(bytes);
    }
    base64::engine::general_purpose::STANDARD.decode(trimmed)
}

/// True if an envelope string looks like a v1 ciphertext (cheap prefix check).
pub fn is_envelope(s: &str) -> bool {
    s.starts_with("v1:")
}

/// Seal a reversible string for an at-rest column: a Cipher envelope when a KEK
/// is configured, plaintext otherwise (the documented self-host fallback, shared
/// by target credentials, share tokens, and secret variables).
pub fn seal_str(raw: &str, cipher: Option<&Cipher>) -> Result<String, CryptoError> {
    match cipher {
        Some(c) => c.encrypt(raw.as_bytes()),
        None => Ok(raw.to_string()),
    }
}

/// Recover a string sealed by [`seal_str`]. `None` when the value is an envelope
/// but no KEK can open it (key rotated out), so callers treat it as unusable
/// rather than handing back ciphertext.
pub fn open_str(stored: &str, cipher: Option<&Cipher>) -> Option<String> {
    if is_envelope(stored) {
        let bytes = cipher?.decrypt(stored).ok()?;
        String::from_utf8(bytes).ok()
    } else {
        Some(stored.to_string())
    }
}

/// JSON key under which a sealed value is stored at rest. Single owner so the
/// targets-credential path and the notification-channel path can never drift
/// to different sentinels (a comment used to be the only thing keeping them
/// equal).
pub const ENC_KEY: &str = "$enc";

/// Wrap a v1 envelope as the canonical sealed JSON object `{"$enc": env}`.
pub fn wrap_envelope(env: String) -> serde_json::Value {
    serde_json::Value::Object(
        [(ENC_KEY.to_string(), serde_json::Value::String(env))]
            .into_iter()
            .collect(),
    )
}

/// The envelope string out of a sealed value, gated on the v1 prefix so a
/// plaintext object that merely has an `$enc` key isn't misread as sealed.
pub fn envelope_str(v: &serde_json::Value) -> Option<&str> {
    v.as_object()?
        .get(ENC_KEY)?
        .as_str()
        .filter(|s| is_envelope(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kek() -> String {
        base64::engine::general_purpose::STANDARD.encode([7u8; 32])
    }

    #[test]
    fn round_trip() {
        let c = Cipher::from_base64(&kek()).unwrap();
        let envelope = c.encrypt(b"hello world").unwrap();
        assert!(envelope.starts_with("v1:"));
        let out = c.decrypt(&envelope).unwrap();
        assert_eq!(out, b"hello world");
    }

    #[test]
    fn empty_plaintext_round_trip() {
        let c = Cipher::from_base64(&kek()).unwrap();
        let envelope = c.encrypt(b"").unwrap();
        assert_eq!(c.decrypt(&envelope).unwrap(), b"");
    }

    #[test]
    fn unique_envelopes_due_to_random_nonce() {
        let c = Cipher::from_base64(&kek()).unwrap();
        let a = c.encrypt(b"x").unwrap();
        let b = c.encrypt(b"x").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let c = Cipher::from_base64(&kek()).unwrap();
        let envelope = c.encrypt(b"secret").unwrap();
        // Flip a byte mid-ciphertext. Avoid the trailing base64 character whose
        // padding bits the strict decoder validates separately.
        let mut bytes = envelope.into_bytes();
        let last_colon = bytes.iter().rposition(|&b| b == b':').unwrap();
        let mid = last_colon + (bytes.len() - last_colon) / 2;
        bytes[mid] = if bytes[mid] == b'A' { b'B' } else { b'A' };
        let envelope = String::from_utf8(bytes).unwrap();
        assert!(matches!(
            c.decrypt(&envelope),
            Err(CryptoError::DecryptFailed)
        ));
    }

    #[test]
    fn wrong_kek_fails() {
        let a = Cipher::from_base64(&kek()).unwrap();
        let b = Cipher::from_base64(&base64::engine::general_purpose::STANDARD.encode([9u8; 32]))
            .unwrap();
        let envelope = a.encrypt(b"secret").unwrap();
        assert!(matches!(
            b.decrypt(&envelope),
            Err(CryptoError::DecryptFailed)
        ));
    }

    #[test]
    fn malformed_envelope_no_version() {
        let c = Cipher::from_base64(&kek()).unwrap();
        assert!(matches!(
            c.decrypt("not-an-envelope"),
            Err(CryptoError::BadShape)
        ));
    }

    #[test]
    fn malformed_envelope_wrong_version() {
        let c = Cipher::from_base64(&kek()).unwrap();
        assert!(matches!(
            c.decrypt("v2:aa:bb"),
            Err(CryptoError::BadVersion)
        ));
    }

    #[test]
    fn malformed_envelope_bad_base64() {
        let c = Cipher::from_base64(&kek()).unwrap();
        assert!(matches!(
            c.decrypt("v1:!!!:???"),
            Err(CryptoError::InvalidBase64(_))
        ));
    }

    #[test]
    fn malformed_envelope_short_nonce() {
        let c = Cipher::from_base64(&kek()).unwrap();
        let short = URL_SAFE_NO_PAD.encode([0u8; 8]);
        let envelope = format!("v1:{short}:{short}");
        assert!(matches!(
            c.decrypt(&envelope),
            Err(CryptoError::InvalidNonceLength(8))
        ));
    }

    #[test]
    fn kek_wrong_length() {
        let short = base64::engine::general_purpose::STANDARD.encode([0u8; 16]);
        assert!(matches!(
            Cipher::from_base64(&short),
            Err(CryptoError::InvalidKekLength(16))
        ));
    }

    #[test]
    fn kek_url_safe_base64_accepted() {
        let raw = [3u8; 32];
        let url_safe = URL_SAFE_NO_PAD.encode(raw);
        assert!(Cipher::from_base64(&url_safe).is_ok());
    }

    #[test]
    fn kek_malformed_base64() {
        assert!(matches!(
            Cipher::from_base64("!!!not-base64!!!"),
            Err(CryptoError::InvalidKekBase64(_))
        ));
    }

    #[test]
    fn is_envelope_detection() {
        assert!(is_envelope("v1:abc:def"));
        assert!(!is_envelope("plaintext-password"));
        assert!(!is_envelope("v2:abc:def"));
    }
}
