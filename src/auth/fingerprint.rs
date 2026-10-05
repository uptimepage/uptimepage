//! Hashed PII for sessions and the login-audit log.
//!
//! `auth.fingerprint_salt` is mixed into a SHA-256 of the IP or user-agent so
//! that "this IP hit us 50 times in 5 minutes" stays answerable without keeping
//! raw IPs around. The salt is **load-bearing for detection continuity** —
//! rotating it produces different digests for the same source, silently
//! splitting anomaly-windows across the rotation point. `ensure_fingerprint_salt`
//! is the startup guard against accidental rotation.

use anyhow::Context;
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use crate::error::{AppError, Result};

/// Env override that lets an operator confirm an intentional rotation. Without
/// it the boot fails when the configured salt is unknown.
pub const ROTATION_OVERRIDE_ENV: &str = "UPTIMEPAGE_AUTH_ACCEPT_SALT_ROTATION";

/// Result code surfaced when the configured salt disagrees with history and
/// the override env var is absent.
pub const SALT_ROTATED_CODE: &str = "FINGERPRINT_SALT_ROTATED";

/// SHA-256(salt || value) → lowercase hex. Returns `None` when `value` is empty
/// so handlers can skip writing meaningless rows for missing UA / unknown IP.
pub fn hash_fingerprint(salt: &str, value: &str) -> Option<String> {
    if value.is_empty() {
        return None;
    }
    let mut h = Sha256::new();
    h.update(salt.as_bytes());
    h.update(b"|");
    h.update(value.as_bytes());
    Some(hex_encode(&h.finalize()))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn salt_digest(salt: &str) -> String {
    let mut h = Sha256::new();
    h.update(salt.as_bytes());
    hex_encode(&h.finalize())
}

/// Refuses to boot if the configured salt does not match the most-recent row
/// in `auth_salt_history`, unless `accept_rotation` (the operator set
/// [`ROTATION_OVERRIDE_ENV`]). Returns Ok(true) when a new history row was
/// inserted (first boot, or accepted rotation).
pub async fn ensure_fingerprint_salt(
    pool: &PgPool,
    salt: &str,
    accept_rotation: bool,
) -> Result<bool> {
    if salt.is_empty() {
        return Err(AppError::Other(anyhow::anyhow!(
            "auth.fingerprint_salt is empty — refusing to boot. Generate one with `openssl rand -base64 32`"
        )));
    }
    let digest = salt_digest(salt);

    let known: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM auth_salt_history WHERE salt_sha256 = $1)")
            .bind(&digest)
            .fetch_one(pool)
            .await
            .context("ensure_fingerprint_salt: lookup")?;

    if known {
        return Ok(false);
    }

    let any_row: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM auth_salt_history)")
        .fetch_one(pool)
        .await
        .context("ensure_fingerprint_salt: empty-check")?;

    if any_row && !accept_rotation {
        return Err(AppError::Other(anyhow::anyhow!(
            "{SALT_ROTATED_CODE}: configured auth.fingerprint_salt is not in \
             auth_salt_history. Set {ROTATION_OVERRIDE_ENV}=1 to accept the \
             rotation (anomaly-detection windows will reset)."
        )));
    }

    sqlx::query("INSERT INTO auth_salt_history (salt_sha256) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind(&digest)
        .execute(pool)
        .await
        .context("ensure_fingerprint_salt: insert")?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_value_yields_none() {
        assert!(hash_fingerprint("salt", "").is_none());
    }

    #[test]
    fn salt_changes_digest() {
        let a = hash_fingerprint("salt-a", "1.2.3.4").unwrap();
        let b = hash_fingerprint("salt-b", "1.2.3.4").unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn deterministic() {
        let a = hash_fingerprint("salt", "ua/1.0").unwrap();
        let b = hash_fingerprint("salt", "ua/1.0").unwrap();
        assert_eq!(a, b);
    }
}
