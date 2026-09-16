//! Postgres persistence for OAuth clients + single-use authorization codes.

use anyhow::Context;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::domain::{OrgId, UserId};
use crate::error::Result;

/// A dynamically-registered (RFC 7591) public client. The caller already holds
/// the `client_id` it looked up by, so only the validation-relevant fields are
/// returned.
#[derive(Debug, Clone)]
pub struct OAuthClient {
    pub client_name: Option<String>,
    pub redirect_uris: Vec<String>,
}

pub async fn insert_client(
    pool: &PgPool,
    client_id: &str,
    client_name: Option<&str>,
    redirect_uris: &[String],
) -> Result<()> {
    sqlx::query(
        "INSERT INTO oauth_clients (client_id, client_name, redirect_uris) \
         VALUES ($1, $2, $3)",
    )
    .bind(client_id)
    .bind(client_name)
    .bind(sqlx::types::Json(redirect_uris))
    .execute(pool)
    .await
    .context("oauth::insert_client")?;
    Ok(())
}

/// Restarts the sweep window of a still-unapproved client; called when its
/// consent screen renders.
pub async fn touch_client(pool: &PgPool, client_id: &str) -> Result<()> {
    sqlx::query(
        "UPDATE oauth_clients SET last_seen_at = now() \
         WHERE client_id = $1 AND last_authorized_at IS NULL",
    )
    .bind(client_id)
    .execute(pool)
    .await
    .context("oauth::touch_client")?;
    Ok(())
}

pub async fn get_client(pool: &PgPool, client_id: &str) -> Result<Option<OAuthClient>> {
    let row: Option<(Option<String>, sqlx::types::Json<Vec<String>>)> =
        sqlx::query_as("SELECT client_name, redirect_uris FROM oauth_clients WHERE client_id = $1")
            .bind(client_id)
            .fetch_optional(pool)
            .await
            .context("oauth::get_client")?;
    Ok(row.map(|(client_name, uris)| OAuthClient {
        client_name,
        redirect_uris: uris.0,
    }))
}

/// A pending authorization code's bound parameters. The code itself is never
/// stored — only its SHA-256.
#[derive(Debug, Clone)]
pub struct AuthCode {
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub scope: String,
    pub resource: String,
    pub user_id: UserId,
    pub org_id: OrgId,
    pub expires_at: DateTime<Utc>,
    /// Connection lifetime: the refresh token (and connection) expires here.
    pub refresh_expires_at: DateTime<Utc>,
}

/// Issues the code and marks the client approved in one statement. `false`
/// means the client is gone (swept mid-consent).
pub async fn insert_code(pool: &PgPool, code_hash: &str, c: &AuthCode) -> Result<bool> {
    let res = sqlx::query(
        "WITH client AS (\
           UPDATE oauth_clients SET last_authorized_at = now() \
           WHERE client_id = $2 RETURNING client_id\
         ) \
         INSERT INTO oauth_authorization_codes \
           (code_hash, client_id, redirect_uri, code_challenge, scope, resource, \
            user_id, org_id, expires_at, refresh_expires_at) \
         SELECT $1, client_id, $3, $4, $5, $6, $7, $8, $9, $10 FROM client",
    )
    .bind(code_hash)
    .bind(&c.client_id)
    .bind(&c.redirect_uri)
    .bind(&c.code_challenge)
    .bind(&c.scope)
    .bind(&c.resource)
    .bind(c.user_id.0)
    .bind(c.org_id.0)
    .bind(c.expires_at)
    .bind(c.refresh_expires_at)
    .execute(pool)
    .await
    .context("oauth::insert_code")?;
    Ok(res.rows_affected() == 1)
}

/// Atomically consume a code (DELETE-RETURNING) so it can never replay, even
/// under two concurrent token requests. Returns `None` if unknown/already used.
/// The caller still checks `expires_at`.
type CodeRow = (
    String,
    String,
    String,
    String,
    String,
    uuid::Uuid,
    uuid::Uuid,
    DateTime<Utc>,
    DateTime<Utc>,
);

pub async fn consume_code(pool: &PgPool, code_hash: &str) -> Result<Option<AuthCode>> {
    let row: Option<CodeRow> = sqlx::query_as(
        "DELETE FROM oauth_authorization_codes WHERE code_hash = $1 \
         RETURNING client_id, redirect_uri, code_challenge, scope, resource, \
                   user_id, org_id, expires_at, refresh_expires_at",
    )
    .bind(code_hash)
    .fetch_optional(pool)
    .await
    .context("oauth::consume_code")?;
    Ok(row.map(
        |(
            client_id,
            redirect_uri,
            code_challenge,
            scope,
            resource,
            user_id,
            org_id,
            expires_at,
            refresh_expires_at,
        )| AuthCode {
            client_id,
            redirect_uri,
            code_challenge,
            scope,
            resource,
            user_id: UserId(user_id),
            org_id: OrgId(org_id),
            expires_at,
            refresh_expires_at,
        },
    ))
}

/// A stored refresh token's bound parameters (minus the raw token).
#[derive(Debug, Clone)]
pub struct RefreshToken {
    pub family_id: uuid::Uuid,
    pub client_id: String,
    pub scope: String,
    pub resource: String,
    pub user_id: UserId,
    pub org_id: OrgId,
    pub used_at: Option<DateTime<Utc>>,
    pub expires_at: DateTime<Utc>,
}

/// Insert a refresh token (current, unused).
#[allow(clippy::too_many_arguments)]
pub async fn insert_refresh_token(
    pool: &PgPool,
    token_hash: &str,
    family_id: uuid::Uuid,
    client_id: &str,
    scope: &str,
    resource: &str,
    user_id: UserId,
    org_id: OrgId,
    expires_at: DateTime<Utc>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO oauth_refresh_tokens \
           (token_hash, family_id, client_id, scope, resource, user_id, org_id, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(token_hash)
    .bind(family_id)
    .bind(client_id)
    .bind(scope)
    .bind(resource)
    .bind(user_id.0)
    .bind(org_id.0)
    .bind(expires_at)
    .execute(pool)
    .await
    .context("oauth::insert_refresh_token")?;
    Ok(())
}

/// Look up a refresh token by hash (does not consume). Returns its bound
/// parameters incl. `used_at` (for replay detection).
type RefreshRow = (
    uuid::Uuid,
    String,
    String,
    String,
    uuid::Uuid,
    uuid::Uuid,
    Option<DateTime<Utc>>,
    DateTime<Utc>,
);

pub async fn get_refresh_token(pool: &PgPool, token_hash: &str) -> Result<Option<RefreshToken>> {
    let row: Option<RefreshRow> = sqlx::query_as(
        "SELECT family_id, client_id, scope, resource, user_id, org_id, used_at, expires_at \
         FROM oauth_refresh_tokens WHERE token_hash = $1",
    )
    .bind(token_hash)
    .fetch_optional(pool)
    .await
    .context("oauth::get_refresh_token")?;
    Ok(row.map(
        |(family_id, client_id, scope, resource, user_id, org_id, used_at, expires_at)| {
            RefreshToken {
                family_id,
                client_id,
                scope,
                resource,
                user_id: UserId(user_id),
                org_id: OrgId(org_id),
                used_at,
                expires_at,
            }
        },
    ))
}

/// Atomically mark a refresh token rotated. Returns `true` iff this call is the
/// one that flipped it from unused→used — the CAS that makes rotation safe under
/// concurrent refreshes (only one winner issues the next token).
pub async fn mark_refresh_used(pool: &PgPool, token_hash: &str) -> Result<bool> {
    let res = sqlx::query(
        "UPDATE oauth_refresh_tokens SET used_at = now() \
         WHERE token_hash = $1 AND used_at IS NULL",
    )
    .bind(token_hash)
    .execute(pool)
    .await
    .context("oauth::mark_refresh_used")?;
    Ok(res.rows_affected() == 1)
}

/// Revoke an entire rotation family (replay/theft response, or normal cleanup).
pub async fn delete_refresh_family(pool: &PgPool, family_id: uuid::Uuid) -> Result<()> {
    sqlx::query("DELETE FROM oauth_refresh_tokens WHERE family_id = $1")
        .bind(family_id)
        .execute(pool)
        .await
        .context("oauth::delete_refresh_family")?;
    Ok(())
}
