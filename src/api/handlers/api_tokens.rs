//! `/api/v1/me/api-tokens` — list, create, rename, revoke.
//!
//! The raw token is returned exactly once (on create); subsequent reads only
//! ever expose the visible prefix. All four endpoints are browser-session only
//! (`BrowserUser`/`VerifiedBrowserUser`) — an API token must never manage
//! tokens, or a scoped token could mint an unrestricted sibling. Creation also
//! requires a verified email: a compromised unverified account could otherwise
//! exfiltrate via a fresh token without proving mailbox control.

use crate::api::json::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::api::error::{ApiError, codes};
use crate::app::AppState;
use crate::auth::api_tokens as tokens;
use crate::auth::scope::{Scope, ScopeSet};
use crate::domain::{OrgId, UserId};
use crate::error::{AppError, Result};
use crate::storage::orgs::{find_id_by_slug, is_active_member};
use crate::web::{BrowserUser, CurrentOrg, CurrentUser, VerifiedBrowserUser};

/// Max length of a token name. Anything longer is almost certainly an
/// accident (or an attempt to fill the table with junk).
const MAX_NAME_LEN: usize = 80;

/// Ceiling on a token's lifetime — least-privilege nudge toward rotation.
const MAX_EXPIRY_DAYS: i64 = 365;

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewApiTokenRequest {
    pub name: String,
    /// Required, non-empty; each must be a known `resource:action` (or
    /// `full_access`). `*:write` implies `*:read` at check time.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Bind the token to one org by slug; omit for an unbound (any-member-org)
    /// token.
    #[serde(default)]
    pub org_slug: Option<String>,
    /// 1..=365; omit for no expiry.
    #[serde(default)]
    pub expires_in_days: Option<u32>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct NewApiTokenResponse {
    pub id: Uuid,
    pub name: String,
    /// Raw token shown ONCE — clients must store it now or regenerate.
    pub token: String,
    pub prefix: String,
    pub created_at: DateTime<Utc>,
    pub scopes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ApiTokenView {
    pub id: Uuid,
    pub name: String,
    pub prefix: String,
    pub created_at: DateTime<Utc>,
    pub scopes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RenameApiTokenRequest {
    pub name: String,
}

#[utoipa::path(
    post,
    path = "/api/v1/me/api-tokens",
    tag = "api-tokens",
    summary = "Create an API token (browser session only)",
    description = "Returns the raw token exactly once. Session-authenticated \
                   only — a bearer token cannot mint another token.",
    request_body = NewApiTokenRequest,
    responses(
        (status = 201, body = NewApiTokenResponse),
        (status = 400, body = ApiError, description = "TOKEN_NAME_INVALID (empty or >80 chars)"),
        (status = 401, body = ApiError, description = "Not a browser session"),
        (status = 403, body = ApiError,
            description = "EMAIL_NOT_VERIFIED, or bound org missing / caller not a member"),
        (status = 422, body = ApiError,
            description = "INVALID_SCOPES, INVALID_EXPIRY, or the per-user token cap reached"),
    ),
)]
pub async fn create(
    State(state): State<AppState>,
    VerifiedBrowserUser(CurrentUser(user_id)): VerifiedBrowserUser,
    CurrentOrg(org): CurrentOrg,
    Json(req): Json<NewApiTokenRequest>,
) -> Result<(StatusCode, Json<NewApiTokenResponse>)> {
    let name = validate_name(&req.name)?;
    let scopes = parse_scopes(&req.scopes)?;
    let expires_at = resolve_expiry(req.expires_in_days)?;
    let pool = state.require_db()?;

    let bound_slug = req
        .org_slug
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let bound_org = match bound_slug {
        Some(slug) => Some(resolve_bound_org(pool, user_id, slug).await?),
        None => None,
    };

    // Tokens are user-scoped; the cap is read from the active org's account
    // plan, so a user acting in two accounts sees each one's limit rather than a
    // single global number.
    let max_tokens = i64::from(
        state
            .quotas
            .limit_for_org(org)
            .await?
            .max_api_tokens_per_user,
    );
    let prefix_len = state.cfg.auth.api_tokens.prefix_visible_chars as usize;
    let created = tokens::create(
        pool, user_id, name, &scopes, bound_org, expires_at, prefix_len, max_tokens,
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(NewApiTokenResponse {
            id: created.id,
            name: created.name,
            token: created.token,
            prefix: created.prefix,
            created_at: created.created_at,
            scopes: scopes.to_strings(),
            org: bound_slug.map(str::to_owned),
            expires_at,
        }),
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/me/api-tokens",
    tag = "api-tokens",
    summary = "List the caller's API tokens (prefix only)",
    responses(
        (status = 200, body = Vec<ApiTokenView>),
        (status = 401, body = ApiError, description = "Not a browser session"),
    ),
)]
pub async fn list(
    State(state): State<AppState>,
    BrowserUser(CurrentUser(user_id)): BrowserUser,
) -> Result<Json<Vec<ApiTokenView>>> {
    let pool = state.require_db()?;
    let rows = tokens::list_for_user(pool, user_id).await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| ApiTokenView {
                id: r.id,
                name: r.name,
                prefix: r.token_prefix,
                created_at: r.created_at,
                scopes: r.scopes.0,
                org: r.org_slug,
                last_used_at: r.last_used_at,
                expires_at: r.expires_at,
            })
            .collect(),
    ))
}

#[utoipa::path(
    patch,
    path = "/api/v1/me/api-tokens/{id}",
    tag = "api-tokens",
    summary = "Rename an API token",
    params(("id" = Uuid, Path)),
    request_body = RenameApiTokenRequest,
    responses(
        (status = 204, description = "Renamed"),
        (status = 400, body = ApiError, description = "TOKEN_NAME_INVALID (empty or >80 chars)"),
        (status = 401, body = ApiError, description = "Not a browser session"),
        (status = 404, body = ApiError, description = "TOKEN_NOT_FOUND"),
    ),
)]
pub async fn rename(
    State(state): State<AppState>,
    BrowserUser(CurrentUser(user_id)): BrowserUser,
    Path(id): Path<Uuid>,
    Json(req): Json<RenameApiTokenRequest>,
) -> Result<StatusCode> {
    let name = validate_name(&req.name)?;
    let pool = state.require_db()?;
    let updated = tokens::rename_for_user(pool, user_id, id, name).await?;
    if !updated {
        return Err(AppError::not_found(
            codes::TOKEN_NOT_FOUND,
            "token not found",
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    delete,
    path = "/api/v1/me/api-tokens/{id}",
    tag = "api-tokens",
    summary = "Revoke an API token",
    params(("id" = Uuid, Path)),
    responses(
        (status = 204, description = "Revoked"),
        (status = 401, body = ApiError, description = "Not a browser session"),
        (status = 404, body = ApiError, description = "TOKEN_NOT_FOUND"),
    ),
)]
pub async fn revoke(
    State(state): State<AppState>,
    BrowserUser(CurrentUser(user_id)): BrowserUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode> {
    let pool = state.require_db()?;
    let removed = tokens::delete_for_user(pool, user_id, id).await?;
    if !removed {
        return Err(AppError::not_found(
            codes::TOKEN_NOT_FOUND,
            "token not found",
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}

fn validate_name(name: &str) -> Result<&str> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(AppError::bad_request_field(
            codes::TOKEN_NAME_INVALID,
            "name must not be empty",
            "name",
        ));
    }
    if trimmed.len() > MAX_NAME_LEN {
        return Err(AppError::bad_request_field(
            codes::TOKEN_NAME_INVALID,
            format!("name must be at most {MAX_NAME_LEN} characters"),
            "name",
        ));
    }
    Ok(trimmed)
}

fn parse_scopes(raw: &[String]) -> Result<ScopeSet> {
    if raw.is_empty() {
        return Err(AppError::unprocessable(
            codes::INVALID_SCOPES,
            "at least one scope is required",
        ));
    }
    if let Some(bad) = raw.iter().find(|s| Scope::parse(s).is_none()) {
        return Err(AppError::unprocessable(
            codes::INVALID_SCOPES,
            format!("unknown scope `{bad}`"),
        ));
    }
    Ok(ScopeSet::from_strs(raw.iter().map(String::as_str)))
}

fn resolve_expiry(days: Option<u32>) -> Result<Option<DateTime<Utc>>> {
    let Some(days) = days else { return Ok(None) };
    if days == 0 || i64::from(days) > MAX_EXPIRY_DAYS {
        return Err(AppError::unprocessable(
            codes::INVALID_EXPIRY,
            format!("expiry must be between 1 and {MAX_EXPIRY_DAYS} days"),
        ));
    }
    Ok(Some(Utc::now() + Duration::days(i64::from(days))))
}

/// Resolve a binding slug to an org the caller actively belongs to. A missing
/// org and a non-membership both 403 (don't reveal which).
async fn resolve_bound_org(pool: &PgPool, user: UserId, slug: &str) -> Result<OrgId> {
    let org = find_id_by_slug(pool, slug)
        .await?
        .ok_or(AppError::Forbidden)?;
    if !is_active_member(pool, user, org).await? {
        return Err(AppError::Forbidden);
    }
    Ok(org)
}
