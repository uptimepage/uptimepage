//! Argon2id password-hash helpers shared by the API-token and the
//! invitation/magic-link token flows.
//!
//! Single owner of these primitives. If argon2 params ever need tuning
//! (memory cost, parallelism), one place changes — the two flows can't
//! silently drift.

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::TryRng;
use rand::rngs::SysRng;
use sha2::{Digest, Sha256};

use crate::error::{AppError, Result};

/// Visible prefix length used by invitation and magic-link token tables.
/// 16 base64url chars = 96 bits of entropy — large enough that a single
/// prefix narrows lookups to ~1 row, so argon2 verify runs once per request
/// instead of once per live row (the DoS-amplification trap).
pub const TOKEN_PREFIX_LEN: usize = 16;

/// First [`TOKEN_PREFIX_LEN`] chars of a raw token (or the whole string if
/// shorter — only happens in tests with handcrafted inputs). Used to populate
/// the `token_prefix` column at INSERT and to narrow the SELECT at consume.
pub fn slice_prefix(raw: &str) -> &str {
    let n = TOKEN_PREFIX_LEN.min(raw.len());
    &raw[..n]
}

/// 32 cryptographically random bytes, base64url-no-pad encoded (43 chars).
/// Shared by invitation and magic-link token issuance — same entropy budget,
/// same encoding, same expectation that only the argon2id hash ever lands in
/// the database.
pub fn generate_raw_token() -> String {
    let mut bytes = [0u8; 32];
    SysRng
        .try_fill_bytes(&mut bytes)
        .expect("SysRng must succeed for token generation");
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Hash a token with argon2id (default params). Returns the encoded PHC
/// string suitable for direct INSERT into a `token_hash` column.
pub fn hash(raw: &str) -> Result<String> {
    Ok(Argon2::default()
        .hash_password(raw.as_bytes())
        .map_err(|e| AppError::Other(anyhow::anyhow!("argon2 hash: {e}")))?
        .to_string())
}

/// Constant-time verify of a presented token against the encoded PHC string.
pub fn verify(raw: &str, encoded: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(encoded) else {
        return false;
    };
    Argon2::default()
        .verify_password(raw.as_bytes(), &parsed)
        .is_ok()
}

/// SHA-256 hex of any string. Used wherever the schema stores a hashed
/// lookup key for a high-entropy random token (session cookies, OAuth state)
/// so a table or query-log leak yields hashes instead of replayable tokens.
/// The inputs are already 256 bits of unguessable entropy — argon2 would
/// burn CPU per lookup for no extra protection.
pub fn sha256_hex(raw: &str) -> String {
    let digest = Sha256::digest(raw.as_bytes());
    hex::encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let raw = "sm_live_abc123_some_random_token_value_here";
        let h = hash(raw).unwrap();
        assert!(verify(raw, &h));
        assert!(!verify("other", &h));
    }

    #[test]
    fn slice_prefix_returns_first_token_prefix_len_chars() {
        let t = generate_raw_token();
        assert_eq!(slice_prefix(&t).len(), TOKEN_PREFIX_LEN);
    }

    #[test]
    fn slice_prefix_shorter_than_len_returns_whole_string() {
        assert_eq!(slice_prefix("short"), "short");
    }
}
