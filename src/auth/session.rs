//! DB-backed sessions: row CRUD, cookie helpers, debounced `last_used_at`
//! refresh.
//!
//! The cookie value is 32 OS-random bytes, base64url-no-pad (43 chars). The
//! DB stores the SHA-256 hash of that value in `sessions.id_hash` so a leak
//! of the `sessions` table can't be replayed as a live cookie. Lookup hashes
//! the presented cookie and probes the indexed `id_hash` column — still one
//! PK probe, plus a fixed SHA-256 (a few microseconds, no allocations beyond
//! the hex output). Matches the persistence pattern already used for
//! `api_tokens` and `magic_link_tokens`.
//!
//! `last_used_at` is updated lazily — the in-process `moka` cache remembers
//! when each session was last persisted and skips the UPDATE if `now()` is
//! within the debounce window. Without that, at 100 RPS one session generates
//! 100 writes per second; with the debounce it's at most 1 per minute (one per
//! replica in multi-replica deployments). The cache is keyed by the hash so
//! the raw cookie never lives in process memory longer than one request.

use std::time::Duration as StdDuration;

use anyhow::Context;
use chrono::{DateTime, Duration, Utc};
use moka::sync::Cache;
use rand::TryRng;
use rand::rngs::SysRng;
use sqlx::PgPool;
use tower_cookies::cookie::{Cookie, SameSite};
use uuid::Uuid;

use crate::config::SessionConfig;
use crate::domain::{OrgId, UserId};
use crate::error::Result;

/// Debounce window for `last_used_at` writes — see module docs.
pub const LAST_USED_DEBOUNCE_SECS: u64 = 60;

/// Cap on how many session ids the debounce cache remembers across all
/// replicas of an `AppState`. 100k entries × ~70 bytes-each is bounded enough
/// to live next to the dashboard cache.
const DEBOUNCE_CACHE_CAPACITY: u64 = 100_000;

/// In-process "I already wrote `last_used_at` for this session recently" set,
/// keyed by session id. Cheap to clone — `moka::sync::Cache` is `Arc` inside.
pub type LastUsedDebounce = Cache<String, DateTime<Utc>>;

pub fn build_debounce_cache() -> LastUsedDebounce {
    Cache::builder()
        .max_capacity(DEBOUNCE_CACHE_CAPACITY)
        .time_to_live(StdDuration::from_secs(LAST_USED_DEBOUNCE_SECS * 4))
        .build()
}

/// Identifies one session DB row. `id` holds the SHA-256-hex of the cookie
/// (the value in the `id_hash` column), never the raw cookie. Comparing
/// `Session.session_id` to listings, debounce-cache keys, and `is_current`
/// all use this same hash form.
#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: String,
    pub user_id: UserId,
    pub active_org_id: Option<OrgId>,
    pub last_used_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// Bundle returned by [`create`]. `cookie_token` is the raw 43-char secret
/// that must land in the `Set-Cookie` header — it is the only place a caller
/// ever sees the raw value, since the DB only stores its hash. `row.id` is
/// the matching hash for downstream lookups/touch.
#[derive(Debug, Clone)]
pub struct CreatedSession {
    pub cookie_token: String,
    pub row: SessionRow,
}

/// Outcome of a session lookup with timeout checks applied. The cookie path
/// uses this to decide whether to update `last_used_at` or clear the cookie.
#[derive(Debug)]
pub enum LookupOutcome {
    Active(SessionRow),
    Expired,
    Missing,
}

/// 32 cryptographically random bytes, base64url-no-pad. Matches the spec's
/// cookie format (43 ASCII chars, no padding to keep cookie value parsing
/// trivial).
pub fn generate_session_id() -> String {
    let mut bytes = [0u8; 32];
    SysRng
        .try_fill_bytes(&mut bytes)
        .expect("SysRng must succeed for session id");
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// SHA-256 hex of the raw cookie value — see [`crate::security::sha256_hex`].
pub fn hash_session_id(raw: &str) -> String {
    crate::security::sha256_hex(raw)
}

/// INSERT a fresh session row. `expires_at` is `now() + absolute_timeout_days`.
/// Returns the raw `cookie_token` (the only place a caller ever sees it) plus
/// the `SessionRow` keyed by its hash — handlers Set-Cookie the raw value and
/// keep the row for any downstream lookup/touch on the same request.
pub async fn create(
    pool: &PgPool,
    cfg: &SessionConfig,
    user: UserId,
    active_org_id: Option<OrgId>,
    ip_hash: Option<&str>,
    user_agent_hash: Option<&str>,
) -> Result<CreatedSession> {
    let cookie_token = generate_session_id();
    let id_hash = hash_session_id(&cookie_token);
    let expires_at = Utc::now() + Duration::days(i64::from(cfg.absolute_timeout_days));
    let row: (DateTime<Utc>, DateTime<Utc>) = sqlx::query_as(
        "INSERT INTO sessions \
            (id_hash, user_id, active_org_id, expires_at, ip_hash, user_agent_hash) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         RETURNING last_used_at, expires_at",
    )
    .bind(&id_hash)
    .bind(user.0)
    .bind(active_org_id.map(|o| o.0))
    .bind(expires_at)
    .bind(ip_hash)
    .bind(user_agent_hash)
    .fetch_one(pool)
    .await
    .context("session::create")?;
    Ok(CreatedSession {
        cookie_token,
        row: SessionRow {
            id: id_hash,
            user_id: user,
            active_org_id,
            last_used_at: row.0,
            expires_at: row.1,
        },
    })
}

/// Look up a session and apply idle + absolute timeout. Expired rows are
/// deleted before returning so a leaked cookie cannot be reanimated by clock
/// drift on the way out.
type LookupRow = (Uuid, Option<Uuid>, DateTime<Utc>, DateTime<Utc>);

pub async fn lookup(
    pool: &PgPool,
    cfg: &SessionConfig,
    cookie_token: &str,
) -> Result<LookupOutcome> {
    let id_hash = hash_session_id(cookie_token);
    let row: Option<LookupRow> = sqlx::query_as(
        "SELECT user_id, active_org_id, last_used_at, expires_at \
         FROM sessions WHERE id_hash = $1",
    )
    .bind(&id_hash)
    .fetch_optional(pool)
    .await
    .context("session::lookup")?;

    let Some((user_id, active_org_id, last_used_at, expires_at)) = row else {
        return Ok(LookupOutcome::Missing);
    };

    let now = Utc::now();
    let idle_limit = Duration::days(i64::from(cfg.idle_timeout_days));
    if expires_at <= now || now.signed_duration_since(last_used_at) > idle_limit {
        destroy_by_hash(pool, &id_hash).await?;
        return Ok(LookupOutcome::Expired);
    }

    Ok(LookupOutcome::Active(SessionRow {
        id: id_hash,
        user_id: UserId(user_id),
        active_org_id: active_org_id.map(OrgId),
        last_used_at,
        expires_at,
    }))
}

/// Updates `last_used_at` to `now()` no more than once per
/// [`LAST_USED_DEBOUNCE_SECS`] per session per replica. `id_hash` is the
/// hash form (the `SessionRow.id` returned by [`lookup`]); the raw cookie
/// must not enter the debounce cache.
pub async fn touch_last_used_debounced(
    pool: &PgPool,
    cache: &LastUsedDebounce,
    id_hash: &str,
) -> Result<()> {
    let now = Utc::now();
    if let Some(last) = cache.get(id_hash)
        && now.signed_duration_since(last) < Duration::seconds(LAST_USED_DEBOUNCE_SECS as i64)
    {
        return Ok(());
    }
    // Server-side guard on the users.last_seen_at leg: an active user with
    // N tabs across M replicas would otherwise dirty the same users heap row
    // N*M times per debounce window. Skip the second UPDATE when the column
    // is already fresh enough — last_seen_at resolution is coarse anyway.
    sqlx::query(
        "WITH bumped AS ( \
             UPDATE sessions SET last_used_at = now() WHERE id_hash = $1 RETURNING user_id \
         ) \
         UPDATE users SET last_seen_at = now() \
         WHERE id IN (SELECT user_id FROM bumped) \
           AND (last_seen_at IS NULL OR last_seen_at < now() - interval '60 seconds')",
    )
    .bind(id_hash)
    .execute(pool)
    .await
    .context("session::touch_last_used_debounced")?;
    cache.insert(id_hash.to_string(), now);
    Ok(())
}

/// Rotate the session's active org in place (invitation accept while signed
/// in). Keyed by the stored hash the extractor already holds — never the raw
/// cookie.
pub async fn set_active_org_by_hash(
    pool: &PgPool,
    id_hash: &str,
    org: crate::domain::OrgId,
) -> Result<bool> {
    let res = sqlx::query(
        "UPDATE sessions SET active_org_id = $2 WHERE id_hash = $1 AND expires_at > now()",
    )
    .bind(id_hash)
    .bind(org.0)
    .execute(pool)
    .await
    .context("session::set_active_org_by_hash")?;
    Ok(res.rows_affected() > 0)
}

/// Stamp `org` onto every live session of `user` that has no active org.
///
/// A sign-in during the deletion grace window resolves no org (theirs is
/// tombstoned) and `active_org_id` is fixed at insert time, so those sessions
/// would 401 on `CurrentOrg` forever. The restore repairs all of them, not just
/// the one that clicked: the user may have signed in on another device first.
pub async fn adopt_org_for_orphan_sessions(
    pool: &PgPool,
    user: UserId,
    org: crate::domain::OrgId,
) -> Result<u64> {
    let res = sqlx::query(
        "UPDATE sessions SET active_org_id = $2 \
         WHERE user_id = $1 AND active_org_id IS NULL AND expires_at > now()",
    )
    .bind(user.0)
    .bind(org.0)
    .execute(pool)
    .await
    .context("session::adopt_org_for_orphan_sessions")?;
    Ok(res.rows_affected())
}

/// Logout / pre-login destroy. Takes the raw cookie value the caller already
/// holds (from the inbound request) and hashes it before the DELETE.
pub async fn destroy(pool: &PgPool, cookie_token: &str) -> Result<u64> {
    destroy_by_hash(pool, &hash_session_id(cookie_token)).await
}

async fn destroy_by_hash(pool: &PgPool, id_hash: &str) -> Result<u64> {
    let res = sqlx::query("DELETE FROM sessions WHERE id_hash = $1")
        .bind(id_hash)
        .execute(pool)
        .await
        .context("session::destroy")?;
    Ok(res.rows_affected())
}

/// "Logout everywhere" — drops every session for the user.
pub async fn destroy_all_for_user(pool: &PgPool, user: UserId) -> Result<u64> {
    let res = sqlx::query("DELETE FROM sessions WHERE user_id = $1")
        .bind(user.0)
        .execute(pool)
        .await
        .context("session::destroy_all_for_user")?;
    Ok(res.rows_affected())
}

/// Everything except the caller's own. A removed credential is not what an
/// attacker holds; the session it opened is, and that outlives it otherwise.
pub async fn destroy_others_for_user(
    pool: &PgPool,
    user: UserId,
    keep_id_hash: &str,
) -> Result<u64> {
    let res = sqlx::query("DELETE FROM sessions WHERE user_id = $1 AND id_hash <> $2")
        .bind(user.0)
        .bind(keep_id_hash)
        .execute(pool)
        .await
        .context("session::destroy_others_for_user")?;
    Ok(res.rows_affected())
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SessionListing {
    /// SHA-256 hex of the cookie value. Safe to expose to the owner — the
    /// pre-image is the unguessable 256-bit cookie. Used as the public handle
    /// for the "revoke this session" form.
    pub id_hash: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub ip_hash: Option<String>,
    pub user_agent_hash: Option<String>,
}

pub async fn list_for_user(pool: &PgPool, user: UserId) -> Result<Vec<SessionListing>> {
    let rows: Vec<SessionListing> = sqlx::query_as(
        "SELECT id_hash, created_at, last_used_at, expires_at, ip_hash, user_agent_hash \
         FROM sessions WHERE user_id = $1 ORDER BY last_used_at DESC",
    )
    .bind(user.0)
    .fetch_all(pool)
    .await
    .context("session::list_for_user")?;
    Ok(rows)
}

/// Targeted revoke — succeeds only when `(id_hash, user_id)` match, so a user
/// can never destroy another user's session. `id_hash` is the value the
/// listing UI handed back (the `SessionListing.id` field), never a raw
/// cookie.
pub async fn destroy_for_user(pool: &PgPool, user: UserId, id_hash: &str) -> Result<bool> {
    let res = sqlx::query("DELETE FROM sessions WHERE id_hash = $1 AND user_id = $2")
        .bind(id_hash)
        .bind(user.0)
        .execute(pool)
        .await
        .context("session::destroy_for_user")?;
    Ok(res.rows_affected() > 0)
}

/// Builds the `Set-Cookie` header value for a session id.
pub fn build_cookie(cfg: &SessionConfig, value: String) -> Cookie<'static> {
    let mut c = Cookie::new(cfg.cookie_name.clone(), value);
    c.set_http_only(true);
    c.set_secure(cfg.cookie_secure);
    c.set_same_site(SameSite::Lax);
    c.set_path("/");
    if !cfg.cookie_domain.is_empty() {
        c.set_domain(cfg.cookie_domain.clone());
    }
    c.set_max_age(tower_cookies::cookie::time::Duration::days(i64::from(
        cfg.absolute_timeout_days,
    )));
    c
}

/// Cookie that clears `_sm_session` on the client. Used by /auth/logout and
/// whenever lookup returns Expired.
pub fn clear_cookie(cfg: &SessionConfig) -> Cookie<'static> {
    let mut c = Cookie::new(cfg.cookie_name.clone(), "");
    c.set_http_only(true);
    c.set_secure(cfg.cookie_secure);
    c.set_same_site(SameSite::Lax);
    c.set_path("/");
    if !cfg.cookie_domain.is_empty() {
        c.set_domain(cfg.cookie_domain.clone());
    }
    c.set_max_age(tower_cookies::cookie::time::Duration::seconds(0));
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_is_base64url_nopad_43_chars() {
        let id = generate_session_id();
        assert_eq!(id.len(), 43);
        assert!(!id.contains('=') && !id.contains('+') && !id.contains('/'));
    }

    #[test]
    fn build_cookie_marks_httponly_samesite_lax() {
        let cfg = SessionConfig::default();
        let c = build_cookie(&cfg, "tok".into());
        assert_eq!(c.name(), "_sm_session");
        assert_eq!(c.value(), "tok");
        assert_eq!(c.http_only(), Some(true));
        assert_eq!(c.same_site(), Some(SameSite::Lax));
    }

    #[test]
    fn clear_cookie_zeroes_max_age() {
        let cfg = SessionConfig::default();
        let c = clear_cookie(&cfg);
        assert_eq!(c.value(), "");
        assert_eq!(
            c.max_age(),
            Some(tower_cookies::cookie::time::Duration::seconds(0))
        );
    }

    #[test]
    fn hash_session_id_is_deterministic_lowercase_hex_64() {
        let a = hash_session_id("cookie-token");
        let b = hash_session_id("cookie-token");
        assert_eq!(a, b, "deterministic");
        assert_eq!(a.len(), 64, "sha256 hex is 64 chars");
        assert!(
            a.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "lowercase hex"
        );
        let c = hash_session_id("different");
        assert_ne!(a, c, "different input → different hash");
    }
}
