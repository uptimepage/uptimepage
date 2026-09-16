//! `sm_live_` API token issuance, storage and lookup.
//!
//! Format: `sm_live_<43 chars>` where the 43 chars are 32 cryptographically
//! random bytes encoded as base64url (no padding). 51 chars total. The
//! `sm_live_` prefix lets git secret scanners detect leaks; the visible
//! `token_prefix` column stores the first `prefix_visible_chars` characters of
//! the raw token (default 16: `sm_live_` + 8 random base64url chars = 48 bits
//! of prefix entropy).
//!
//! The prefix column is intentionally **not** UNIQUE. Collisions at 48 bits
//! are rare but a UNIQUE constraint would turn the rare event into a 500.
//! Instead the prefix narrows the lookup set and argon2-verify against
//! `token_hash` disambiguates the survivor.

use std::time::Duration as StdDuration;

use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use moka::sync::Cache;
use rand::TryRng;
use rand::rngs::SysRng;
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::scope::ScopeSet;
use crate::auth::session::LAST_USED_DEBOUNCE_SECS;
use crate::auth::token_hash;
use crate::domain::{OrgId, UserId};
use crate::error::Result;
use crate::storage::locks::{advisory_xact_lock, user_lock_key};

/// Public prefix that triggers Bearer authentication. Both halves are checked:
/// `Authorization: Bearer <s>` with `s.starts_with(TOKEN_PREFIX)` routes to
/// the API-token path; everything else falls through to session auth.
pub const TOKEN_PREFIX: &str = "sm_live_";

/// Bytes of randomness in a generated token. 32 bytes → 43 base64url chars
/// after the fixed prefix → 51 char total length.
const TOKEN_RANDOM_BYTES: usize = 32;

/// Minimum acceptable value of `prefix_visible_chars`. Below this, the prefix
/// supplies fewer than 48 bits of entropy and the bounded-row-set guarantee
/// behind "lookup is fast and never 500s on collision" breaks down.
pub const MIN_PREFIX_VISIBLE_CHARS: usize = 16;

/// Outcome of an attempted Bearer lookup. The middleware translates `Invalid`
/// into 401; `Active` into populating `AuthContext::ApiToken`.
#[derive(Debug)]
pub enum LookupOutcome {
    Active(ApiTokenRow),
    /// Live token minted for a different resource (RFC 8707).
    WrongAudience,
    Invalid,
}

#[derive(Debug, Clone)]
pub struct ApiTokenRow {
    pub id: Uuid,
    pub user_id: UserId,
    pub name: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub scopes: ScopeSet,
    /// Org binding: `Some` pins the token to one org; `None` is unbound.
    pub org: Option<OrgId>,
}

/// One row returned by [`list_for_user`]. `token_prefix` is the only piece of
/// the raw token persisted in plaintext — safe to show in UI.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ApiTokenListing {
    pub id: Uuid,
    pub name: String,
    pub token_prefix: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    /// Stored scope strings (canonical `resource:action` forms).
    pub scopes: sqlx::types::Json<Vec<String>>,
    /// Slug of the bound org, or `None` for an unbound (any-member-org) token.
    pub org_slug: Option<String>,
}

/// Output of [`create`]. `token` is the only place the caller can see the raw
/// value — never persisted, never recoverable.
#[derive(Debug, Clone)]
pub struct CreatedToken {
    pub id: Uuid,
    pub name: String,
    pub token: String,
    pub prefix: String,
    pub created_at: DateTime<Utc>,
}

/// In-process "I wrote `last_used_at` for this token recently" set. Mirrors
/// the session debounce shape — one cache per replica.
pub type ApiTokenLastUsedDebounce = Cache<Uuid, DateTime<Utc>>;

/// Build the API-token `last_used_at` debounce cache. Capacity matches the
/// session cache: well above the realistic active-token set.
pub fn build_debounce_cache() -> ApiTokenLastUsedDebounce {
    Cache::builder()
        .max_capacity(100_000)
        .time_to_live(StdDuration::from_secs(LAST_USED_DEBOUNCE_SECS * 4))
        .build()
}

/// Generate a fresh raw token. 32 random bytes → 43 chars base64url → prefixed
/// with `sm_live_`.
pub fn generate_raw() -> String {
    let mut bytes = [0u8; TOKEN_RANDOM_BYTES];
    SysRng
        .try_fill_bytes(&mut bytes)
        .expect("SysRng must succeed for api-token generation");
    format!("{TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes))
}

/// Slice the visible prefix per `prefix_visible_chars`. Panics in debug if
/// the configured value is shorter than [`MIN_PREFIX_VISIBLE_CHARS`] — startup
/// asserts the bound, but the slice would otherwise silently produce a too-
/// short prefix.
fn slice_prefix(raw: &str, prefix_visible_chars: usize) -> &str {
    debug_assert!(
        prefix_visible_chars >= MIN_PREFIX_VISIBLE_CHARS,
        "prefix_visible_chars must be >= {MIN_PREFIX_VISIBLE_CHARS}"
    );
    let n = prefix_visible_chars.min(raw.len());
    &raw[..n]
}

/// Count of *live* tokens owned by `user`. Excludes expired rows because
/// they can't authenticate (`lookup_by_raw` skips them) and shouldn't
/// occupy the per-user quota — without this filter a power user who
/// rotates monthly hits the cap on year-old expired remnants. The cap in
/// `create` uses the same filter so the read here matches the gate there.
pub async fn count_for_user(pool: &PgPool, user: UserId) -> Result<u32> {
    let (n,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM api_tokens \
         WHERE user_id = $1 AND oauth_client_id IS NULL \
           AND (expires_at IS NULL OR expires_at > now())",
    )
    .bind(user.0)
    .fetch_one(pool)
    .await
    .context("api_tokens::count_for_user")?;
    Ok(u32::try_from(n).unwrap_or(u32::MAX))
}

/// Insert a freshly-issued token. Returns the raw token string — the only
/// place the caller can ever see it. The persisted row holds the argon2id
/// hash plus the visible prefix.
/// `max_tokens` is the plan's `max_api_tokens_per_user`. A per-user
/// advisory lock serialises concurrent creates for the same user so the
/// count + INSERT cannot race (the same standard as the owner-org and
/// invitation caps — not check-then-act). Argon2 runs only *after* the
/// cheap cap reject so a blocked abuse path doesn't pay ~150 ms of CPU.
#[allow(clippy::too_many_arguments)]
pub async fn create(
    pool: &PgPool,
    user: UserId,
    name: &str,
    scopes: &ScopeSet,
    org: Option<OrgId>,
    expires_at: Option<DateTime<Utc>>,
    prefix_visible_chars: usize,
    max_tokens: i64,
) -> Result<CreatedToken> {
    let raw = generate_raw();
    let prefix = slice_prefix(&raw, prefix_visible_chars).to_string();

    let mut tx = pool.begin().await.context("api_tokens::create: begin")?;
    advisory_xact_lock(&mut *tx, &user_lock_key(user))
        .await
        .context("api_tokens::create: advisory lock")?;

    let (current,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM api_tokens \
         WHERE user_id = $1 AND oauth_client_id IS NULL \
           AND (expires_at IS NULL OR expires_at > now())",
    )
    .bind(user.0)
    .fetch_one(&mut *tx)
    .await
    .context("api_tokens::create: count")?;
    if current + 1 > max_tokens {
        tx.rollback().await.ok();
        return Err(crate::error::AppError::quota_exceeded(
            "max_api_tokens_per_user",
            current,
            max_tokens,
            "free",
        ));
    }

    let hash = token_hash::hash(&raw)?;
    let (id, created_at): (Uuid, DateTime<Utc>) = sqlx::query_as(
        "INSERT INTO api_tokens \
           (user_id, name, token_hash, token_prefix, scopes, org_id, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         RETURNING id, created_at",
    )
    .bind(user.0)
    .bind(name)
    .bind(&hash)
    .bind(&prefix)
    .bind(sqlx::types::Json(scopes.to_strings()))
    .bind(org.map(|o| o.0))
    .bind(expires_at)
    .fetch_one(&mut *tx)
    .await
    .context("api_tokens::create")?;
    tx.commit().await.context("api_tokens::create: commit")?;
    Ok(CreatedToken {
        id,
        name: name.to_string(),
        token: raw,
        prefix,
        created_at,
    })
}

/// Mint an OAuth connector access token: an org-bound, expiring, audience-
/// stamped (`sm_live_`) token issued by the OAuth token endpoint behind user
/// consent. Distinct from [`create`]:
///  - It does **not** count against the manual per-user token cap — OAuth
///    tokens are a separate class (`oauth_client_id IS NOT NULL`), kept bounded
///    instead by revoking the prior token for the same
///    `(user, org, client, audience)` before inserting, so a re-consent is
///    idempotent rather than additive.
///  - `audience` (RFC 8707) and `oauth_client_id` are stamped so the resource
///    server can reject tokens minted for any other resource.
///
/// The revoke + insert run under the same per-user advisory lock so two
/// concurrent consents for one client can't both survive.
#[allow(clippy::too_many_arguments)]
pub async fn mint_oauth(
    pool: &PgPool,
    user: UserId,
    org: OrgId,
    name: &str,
    scopes: &ScopeSet,
    expires_at: DateTime<Utc>,
    prefix_visible_chars: usize,
    audience: &str,
    oauth_client_id: &str,
) -> Result<CreatedToken> {
    let raw = generate_raw();
    let prefix = slice_prefix(&raw, prefix_visible_chars).to_string();
    let hash = token_hash::hash(&raw)?;

    let mut tx = pool
        .begin()
        .await
        .context("api_tokens::mint_oauth: begin")?;
    advisory_xact_lock(&mut *tx, &user_lock_key(user))
        .await
        .context("api_tokens::mint_oauth: advisory lock")?;

    // Idempotent re-consent: drop the previous connector token for this exact
    // (user, org, client, audience) so the user never accumulates stale grants.
    sqlx::query(
        "DELETE FROM api_tokens \
         WHERE user_id = $1 AND org_id = $2 AND oauth_client_id = $3 AND audience = $4",
    )
    .bind(user.0)
    .bind(org.0)
    .bind(oauth_client_id)
    .bind(audience)
    .execute(&mut *tx)
    .await
    .context("api_tokens::mint_oauth: revoke prior")?;

    let (id, created_at): (Uuid, DateTime<Utc>) = sqlx::query_as(
        "INSERT INTO api_tokens \
           (user_id, name, token_hash, token_prefix, scopes, org_id, expires_at, \
            audience, oauth_client_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
         RETURNING id, created_at",
    )
    .bind(user.0)
    .bind(name)
    .bind(&hash)
    .bind(&prefix)
    .bind(sqlx::types::Json(scopes.to_strings()))
    .bind(org.0)
    .bind(expires_at)
    .bind(audience)
    .bind(oauth_client_id)
    .fetch_one(&mut *tx)
    .await
    .context("api_tokens::mint_oauth: insert")?;
    tx.commit()
        .await
        .context("api_tokens::mint_oauth: commit")?;
    Ok(CreatedToken {
        id,
        name: name.to_string(),
        token: raw,
        prefix,
        created_at,
    })
}

/// Revoke the connector's OAuth access token(s) for one
/// `(user, org, client, audience)`. Called when a refresh-token replay is
/// detected (token theft) so the leaked access token dies alongside the refresh
/// family. Safe no-op when none exist.
pub async fn revoke_oauth(
    pool: &PgPool,
    user: UserId,
    org: OrgId,
    oauth_client_id: &str,
    audience: &str,
) -> Result<()> {
    sqlx::query(
        "DELETE FROM api_tokens \
         WHERE user_id = $1 AND org_id = $2 AND oauth_client_id = $3 AND audience = $4",
    )
    .bind(user.0)
    .bind(org.0)
    .bind(oauth_client_id)
    .bind(audience)
    .execute(pool)
    .await
    .context("api_tokens::revoke_oauth")?;
    Ok(())
}

/// List the caller's tokens. Prefix-only; the raw token can never be
/// reconstructed from this output.
pub async fn list_for_user(pool: &PgPool, user: UserId) -> Result<Vec<ApiTokenListing>> {
    let rows: Vec<ApiTokenListing> = sqlx::query_as(
        "SELECT t.id, t.name, t.token_prefix, t.created_at, t.last_used_at, \
                t.expires_at, t.scopes, o.slug::text AS org_slug \
         FROM api_tokens t \
         LEFT JOIN organizations o ON o.id = t.org_id \
         WHERE t.user_id = $1 \
         ORDER BY t.created_at DESC",
    )
    .bind(user.0)
    .fetch_all(pool)
    .await
    .context("api_tokens::list_for_user")?;
    Ok(rows)
}

/// Look up a presented token. Strips the `sm_live_` prefix-check, derives the
/// visible prefix per config, fetches candidate rows (bounded by the
/// per-user token cap), and argon2-verifies each. Verification is
/// constant-time; the loop runs at most that many iterations.
///
/// `resource` is the RFC 8707 audience the caller serves. `None` serves no
/// OAuth resource, so a token minted for one is refused; `Some` accepts an
/// unbound token or one bound to exactly that resource. Every consumer states
/// this, so none can honour an MCP token by omission.
pub async fn lookup_by_raw(
    pool: &PgPool,
    raw: &str,
    prefix_visible_chars: usize,
    resource: Option<&str>,
) -> Result<LookupOutcome> {
    if !raw.starts_with(TOKEN_PREFIX) {
        return Ok(LookupOutcome::Invalid);
    }
    let prefix = slice_prefix(raw, prefix_visible_chars);

    type CandidateRow = (
        Uuid,
        Uuid,
        String,
        String,
        Option<DateTime<Utc>>,
        sqlx::types::Json<Vec<String>>,
        Option<Uuid>,
        Option<String>,
    );
    let rows: Vec<CandidateRow> = sqlx::query_as(
        "SELECT id, user_id, name, token_hash, expires_at, scopes, org_id, audience \
         FROM api_tokens WHERE token_prefix = $1",
    )
    .bind(prefix)
    .fetch_all(pool)
    .await
    .context("api_tokens::lookup_by_raw")?;

    let now = Utc::now();
    for (id, user_id, name, hash, expires_at, scopes, org_id, audience) in rows {
        if expires_at.is_some_and(|e| e <= now) {
            continue;
        }
        if token_hash::verify(raw, &hash) {
            if !audience_accepted(audience.as_deref(), resource) {
                return Ok(LookupOutcome::WrongAudience);
            }
            return Ok(LookupOutcome::Active(ApiTokenRow {
                id,
                user_id: UserId(user_id),
                name,
                expires_at,
                scopes: ScopeSet::from_strs(scopes.0.iter().map(String::as_str)),
                org: org_id.map(OrgId),
            }));
        }
    }
    Ok(LookupOutcome::Invalid)
}

fn audience_accepted(audience: Option<&str>, resource: Option<&str>) -> bool {
    match (audience, resource) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(aud), Some(res)) => {
            !res.is_empty() && aud.trim_end_matches('/') == res.trim_end_matches('/')
        }
    }
}

/// Synchronous check: would a `touch_last_used_debounced` call actually
/// issue an UPDATE right now? Used by the Bearer middleware to skip the
/// `tokio::spawn` on the no-op path (which is >99% of requests).
pub fn should_touch(cache: &ApiTokenLastUsedDebounce, token_id: Uuid) -> bool {
    match cache.get(&token_id) {
        None => true,
        Some(last) => {
            Utc::now().signed_duration_since(last)
                >= chrono::Duration::seconds(LAST_USED_DEBOUNCE_SECS as i64)
        }
    }
}

/// Lazy `last_used_at` bump. Debounced same as sessions — at most one UPDATE
/// per token per replica per [`LAST_USED_DEBOUNCE_SECS`]. Callers can call
/// [`should_touch`] first to elide the async hop entirely.
pub async fn touch_last_used_debounced(
    pool: &PgPool,
    cache: &ApiTokenLastUsedDebounce,
    token_id: Uuid,
) -> Result<()> {
    if !should_touch(cache, token_id) {
        return Ok(());
    }
    sqlx::query("UPDATE api_tokens SET last_used_at = now() WHERE id = $1")
        .bind(token_id)
        .execute(pool)
        .await
        .context("api_tokens::touch_last_used_debounced")?;
    cache.insert(token_id, Utc::now());
    Ok(())
}

/// Rename the caller's token. Match `user_id` to prevent cross-user rewrites
/// (defence in depth — the handler already extracts the caller).
pub async fn rename_for_user(
    pool: &PgPool,
    user: UserId,
    token_id: Uuid,
    new_name: &str,
) -> Result<bool> {
    let res = sqlx::query("UPDATE api_tokens SET name = $3 WHERE id = $1 AND user_id = $2")
        .bind(token_id)
        .bind(user.0)
        .bind(new_name)
        .execute(pool)
        .await
        .context("api_tokens::rename_for_user")?;
    Ok(res.rows_affected() > 0)
}

/// Targeted revoke. Bound to `user_id` so a user can never destroy another
/// user's token even if they brute-force the id.
pub async fn delete_for_user(pool: &PgPool, user: UserId, token_id: Uuid) -> Result<bool> {
    let res = sqlx::query("DELETE FROM api_tokens WHERE id = $1 AND user_id = $2")
        .bind(token_id)
        .bind(user.0)
        .execute(pool)
        .await
        .context("api_tokens::delete_for_user")?;
    Ok(res.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audience_accepted_refuses_every_mismatch() {
        let mcp = "https://mcp.example/mcp";
        assert!(audience_accepted(None, None));
        assert!(audience_accepted(None, Some(mcp)));
        assert!(audience_accepted(Some(mcp), Some(mcp)));
        assert!(audience_accepted(
            Some("https://mcp.example/mcp/"),
            Some(mcp)
        ));
        assert!(!audience_accepted(Some(mcp), None));
        assert!(!audience_accepted(Some(mcp), Some("")));
        assert!(!audience_accepted(
            Some("https://other.example/mcp"),
            Some(mcp)
        ));
    }

    #[test]
    fn generate_raw_is_sm_live_prefixed_51_chars() {
        let raw = generate_raw();
        assert!(raw.starts_with(TOKEN_PREFIX));
        assert_eq!(raw.len(), TOKEN_PREFIX.len() + 43);
        assert!(!raw.contains('=') && !raw.contains('+') && !raw.contains('/'));
    }

    #[test]
    fn slice_prefix_returns_first_n_chars() {
        let raw = "sm_live_abcdefghijklmnop";
        assert_eq!(slice_prefix(raw, 16), "sm_live_abcdefgh");
    }
}
