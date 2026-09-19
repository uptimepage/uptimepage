//! Named, process-generated secrets persisted across restarts.

use anyhow::Context;
use sqlx::PgPool;

use crate::error::{AppError, Result};
use crate::security::token_hash::generate_raw_token;
use crate::security::{Cipher, is_envelope};

/// Return the secret named `name`, generating and persisting a fresh one on
/// first call. The generate races harmlessly: the unique `name` plus
/// `ON CONFLICT DO NOTHING` means only the first writer's value lands, and the
/// follow-up SELECT always returns the winner, which is then opened.
pub async fn ensure_secret(pool: &PgPool, cipher: Option<&Cipher>, name: &str) -> Result<String> {
    let stored = seal(&generate_raw_token(), cipher)?;
    sqlx::query("INSERT INTO app_secrets (name, secret) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(name)
        .bind(&stored)
        .execute(pool)
        .await
        .context("app_secrets::ensure_secret insert")?;
    let stored: String = sqlx::query_scalar("SELECT secret FROM app_secrets WHERE name = $1")
        .bind(name)
        .fetch_one(pool)
        .await
        .context("app_secrets::ensure_secret select")?;
    open(&stored, cipher)
}

/// Cipher envelope when a KEK is configured, plaintext otherwise (same fallback
/// as target credentials).
fn seal(raw: &str, cipher: Option<&Cipher>) -> Result<String> {
    match cipher {
        Some(c) => c
            .encrypt(raw.as_bytes())
            .map_err(|e| AppError::Other(anyhow::anyhow!("app secret encryption failed: {e}"))),
        None => Ok(raw.to_string()),
    }
}

/// A sealed envelope needs the KEK; if it is gone the secret is unrecoverable
/// and boot fails loudly rather than minting HMACs under a wrong key, which
/// would silently void every link already issued under the original.
fn open(stored: &str, cipher: Option<&Cipher>) -> Result<String> {
    if is_envelope(stored) {
        let c = cipher.ok_or_else(|| {
            AppError::Other(anyhow::anyhow!("app secret is sealed but no KEK set"))
        })?;
        let bytes = c
            .decrypt(stored)
            .map_err(|e| AppError::Other(anyhow::anyhow!("app secret decryption failed: {e}")))?;
        String::from_utf8(bytes)
            .map_err(|e| AppError::Other(anyhow::anyhow!("app secret not utf8: {e}")))
    } else {
        Ok(stored.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn cipher() -> Cipher {
        Cipher::from_base64(&base64::engine::general_purpose::STANDARD.encode([7u8; 32])).unwrap()
    }

    #[test]
    fn seal_open_round_trip() {
        let c = cipher();
        let env = seal("raw-secret", Some(&c)).unwrap();
        assert!(is_envelope(&env));
        assert_eq!(open(&env, Some(&c)).unwrap(), "raw-secret");
    }

    #[test]
    fn no_kek_stores_plaintext() {
        let stored = seal("raw-secret", None).unwrap();
        assert_eq!(stored, "raw-secret");
        assert_eq!(open(&stored, None).unwrap(), "raw-secret");
    }

    #[test]
    fn plaintext_opens_without_kek() {
        assert_eq!(open("raw-secret", Some(&cipher())).unwrap(), "raw-secret");
    }

    #[test]
    fn sealed_without_kek_errors() {
        let env = seal("raw-secret", Some(&cipher())).unwrap();
        assert!(open(&env, None).is_err());
    }
}
