//! `oauth_states` CRUD: insert before redirect, atomic consume on callback.
//!
//! The `consume` query both deletes and returns in one statement so two
//! concurrent callbacks for the same state cannot both proceed.

use anyhow::Context;
use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{AppError, Result};
use crate::security::token_hash::generate_raw_token;

const STATE_EXPIRY_MINUTES: i64 = 10;

/// SHA-256 hex of the raw state — see [`crate::security::sha256_hex`].
pub fn hash_state(raw: &str) -> String {
    crate::security::sha256_hex(raw)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ConsumedState {
    pub provider: String,
    pub redirect_after: Option<String>,
    pub invitation_id: Option<Uuid>,
    /// Target org of a connect-purpose dance; `None` for login providers.
    pub org_id: Option<Uuid>,
    /// Delegate link a connect dance was started from; possession of that
    /// link replaces the session-membership check on the callback.
    pub link_code_id: Option<Uuid>,
    /// Set only by a link dance: the signed-in user the resulting identity
    /// attaches to. `None` is a login dance, which resolves its own user.
    pub link_user_id: Option<Uuid>,
}

/// What a dance carries beyond its provider. Every field is a purpose marker,
/// so a dance that sets none is a plain sign-in.
#[derive(Debug, Clone, Copy, Default)]
pub struct StateBinding<'a> {
    pub redirect_after: Option<&'a str>,
    pub invitation_id: Option<Uuid>,
    pub org_id: Option<Uuid>,
    pub link_code_id: Option<Uuid>,
    pub link_user_id: Option<Uuid>,
}

/// 32 random bytes, base64url-no-pad. Caller stores via [`insert`].
pub fn generate_state() -> String {
    generate_raw_token()
}

pub async fn insert(
    pool: &PgPool,
    state: &str,
    provider: &str,
    binding: StateBinding<'_>,
) -> Result<DateTime<Utc>> {
    let expires_at = Utc::now() + Duration::minutes(STATE_EXPIRY_MINUTES);
    sqlx::query(
        "INSERT INTO oauth_states \
             (state_hash, provider, redirect_after, invitation_id, org_id, link_code_id, \
              link_user_id, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(hash_state(state))
    .bind(provider)
    .bind(binding.redirect_after)
    .bind(binding.invitation_id)
    .bind(binding.org_id)
    .bind(binding.link_code_id)
    .bind(binding.link_user_id)
    .bind(expires_at)
    .execute(pool)
    .await
    .context("oauth_state::insert")?;
    Ok(expires_at)
}

/// DELETE-and-RETURN. Returns `None` when the state never existed or has
/// expired. The callback maps that to `INVALID_STATE` without distinguishing
/// the two cases (anti-enumeration).
pub async fn consume(pool: &PgPool, state: &str) -> Result<Option<ConsumedState>> {
    sqlx::query_as(
        "DELETE FROM oauth_states \
         WHERE state_hash = $1 AND expires_at > now() \
         RETURNING provider, redirect_after, invitation_id, org_id, link_code_id, link_user_id",
    )
    .bind(hash_state(state))
    .fetch_optional(pool)
    .await
    .map_err(|e| AppError::Other(anyhow::anyhow!("oauth_state::consume: {e}")))
}

/// `expires_at` is 10 minutes from insert and the callback's atomic
/// DELETE-and-RETURN purges consumed rows, so a row only outlives its TTL
/// when the user abandons the dance — without the periodic sweep the table
/// leaks slowly forever.
pub async fn purge_expired(pool: &PgPool) -> sqlx::Result<u64> {
    let res = sqlx::query("DELETE FROM oauth_states WHERE expires_at < now()")
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_state_is_url_safe_nopad() {
        let s = generate_state();
        assert!(!s.contains('+') && !s.contains('/') && !s.contains('='));
        assert!(s.len() >= 40);
    }

    #[test]
    fn generate_state_is_unique_over_many_samples() {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for _ in 0..1024 {
            assert!(seen.insert(generate_state()));
        }
    }

    #[test]
    fn hash_state_is_deterministic_and_hex() {
        let s = "test-state-value";
        let a = hash_state(s);
        assert_eq!(a, hash_state(s));
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hash_state_separates_distinct_inputs() {
        assert_ne!(hash_state("a"), hash_state("b"));
    }
}
