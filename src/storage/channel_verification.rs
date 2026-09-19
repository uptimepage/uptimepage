//! Single-use verification tokens for `email` notification channels.
//!
//! Tokens are 32 random bytes (43-char base64url); only the SHA-256 is
//! stored, mirroring `channel_link_codes.code_hash`. Presenting the raw
//! token at the public verify endpoint proves inbox ownership.

use anyhow::Context;
use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::OrgId;
use crate::error::Result;
use crate::security::sha256_hex;
use crate::security::token_hash::generate_raw_token;

pub const VERIFICATION_TTL_HOURS: u32 = 24;
/// Mints per channel per 24 h — bounds verification-mail spam to one inbox.
pub const PER_CHANNEL_DAILY_CAP: i64 = 3;
/// Mints per org per 24 h — bounds an org fanning out across many addresses.
pub const PER_ORG_DAILY_CAP: i64 = 20;
/// Mints per recipient address per 24 h, across every org and channel —
/// re-creating channels can't reset the budget for one victim inbox.
pub const PER_ADDRESS_DAILY_CAP: i64 = 3;

pub enum MintOutcome {
    /// Raw token — embedded in the verify URL, never persisted.
    Created {
        token: String,
    },
    LimitReached,
}

#[derive(Debug, sqlx::FromRow)]
pub struct ConsumedToken {
    pub org_id: Uuid,
    pub channel_id: Uuid,
    pub email: String,
}

/// Mint a token for `channel_id`, enforcing both daily caps in the same
/// statement so concurrent mints can't overshoot.
pub async fn mint(pool: &PgPool, org: OrgId, channel_id: Uuid, email: &str) -> Result<MintOutcome> {
    let raw = generate_raw_token();
    let expires_at = Utc::now() + Duration::hours(i64::from(VERIFICATION_TTL_HOURS));
    let inserted: Option<(Uuid,)> = sqlx::query_as(
        "INSERT INTO channel_verification_tokens (org_id, channel_id, email, token_hash, expires_at)
         SELECT $1, $2, $3, $4, $5
         WHERE (SELECT count(*) FROM channel_verification_tokens
                WHERE channel_id = $2 AND created_at > now() - interval '24 hours') < $6
           AND (SELECT count(*) FROM channel_verification_tokens
                WHERE org_id = $1 AND created_at > now() - interval '24 hours') < $7
           AND (SELECT count(*) FROM channel_verification_tokens /* SAFE: cross-org on purpose — the cap protects the recipient inbox, not a tenant */
                WHERE email = $3 AND created_at > now() - interval '24 hours') < $8
         RETURNING id",
    )
    .bind(org.0)
    .bind(channel_id)
    .bind(email)
    .bind(sha256_hex(&raw))
    .bind(expires_at)
    .bind(PER_CHANNEL_DAILY_CAP)
    .bind(PER_ORG_DAILY_CAP)
    .bind(PER_ADDRESS_DAILY_CAP)
    .fetch_optional(pool)
    .await
    .context("channel_verification::mint")?;
    Ok(match inserted {
        Some(_) => MintOutcome::Created { token: raw },
        None => MintOutcome::LimitReached,
    })
}

/// Atomically consume `raw_token`. `None` for any of: unknown, expired,
/// already used — callers surface one generic invalid-link page.
pub async fn consume(pool: &PgPool, raw_token: &str) -> Result<Option<ConsumedToken>> {
    let row: Option<ConsumedToken> = sqlx::query_as(
        "UPDATE channel_verification_tokens
         SET used_at = now()
         WHERE token_hash = $1 AND used_at IS NULL AND expires_at > now()
         RETURNING org_id, channel_id, email",
    )
    .bind(sha256_hex(raw_token))
    .fetch_optional(pool)
    .await
    .context("channel_verification::consume")?;
    Ok(row)
}

/// Periodic cleanup: expired rows, and used rows older than 7 days.
pub async fn purge_old(pool: &PgPool) -> sqlx::Result<u64> {
    let res = sqlx::query(
        "DELETE FROM channel_verification_tokens
         WHERE expires_at < now()
            OR (used_at IS NOT NULL AND used_at < now() - INTERVAL '7 days')",
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Most recent mint for a channel (test/introspection helper).
pub async fn latest_expiry(pool: &PgPool, channel_id: Uuid) -> Result<Option<DateTime<Utc>>> {
    let row: Option<(DateTime<Utc>,)> = sqlx::query_as(
        "SELECT expires_at FROM channel_verification_tokens
         WHERE channel_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(channel_id)
    .fetch_optional(pool)
    .await
    .context("channel_verification::latest_expiry")?;
    Ok(row.map(|(t,)| t))
}
