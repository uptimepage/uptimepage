//! Token endpoint: `authorization_code` and `refresh_token` grants.
//!
//! Both issue a short-lived access token (the audience-bound `sm_live_` scoped
//! token) plus a rotating refresh token. OAuth 2.1 rotation: each refresh use
//! mints a fresh refresh token in the same family and retires the old one;
//! replaying a retired token is treated as theft and revokes the whole family
//! plus the live access token. The connection's absolute lifetime is fixed at
//! consent — refresh renews the access token, it does not extend the deadline.
//!
//! All grant failures collapse to `invalid_grant` so a prober learns nothing.

use axum::extract::{Form, State};
use axum::http::HeaderMap;
use axum::http::header::CACHE_CONTROL;
use axum::response::{IntoResponse, Response};
use axum::{Json, http::HeaderValue};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Duration, Utc};
use rand::TryRng;
use rand::rngs::SysRng;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::app::AppState;
use crate::auth::api_tokens;
use crate::auth::scope::ScopeSet;
use crate::domain::{OrgId, UserId};
use crate::security::sha256_hex;

use super::error::{OAuthError, OAuthErrorResponse};
use super::pkce;
use super::store;

#[derive(Debug, Deserialize)]
pub struct TokenRequest {
    #[serde(default)]
    grant_type: String,
    #[serde(default)]
    code: String,
    #[serde(default)]
    redirect_uri: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    code_verifier: String,
    #[serde(default)]
    refresh_token: String,
}

pub async fn token(
    State(state): State<AppState>,
    Form(req): Form<TokenRequest>,
) -> Result<Response, OAuthErrorResponse> {
    let pool = state
        .db
        .as_ref()
        .ok_or_else(|| OAuthError::ServerError.with("storage unavailable"))?;
    match req.grant_type.as_str() {
        "authorization_code" => code_grant(&state, pool, req).await,
        "refresh_token" => refresh_grant(&state, pool, req).await,
        _ => Err(OAuthError::UnsupportedGrantType
            .with("only authorization_code and refresh_token are supported")),
    }
}

fn invalid() -> OAuthErrorResponse {
    OAuthError::InvalidGrant.with("the grant is invalid")
}

async fn code_grant(
    state: &AppState,
    pool: &sqlx::PgPool,
    req: TokenRequest,
) -> Result<Response, OAuthErrorResponse> {
    if req.code.is_empty() || req.code_verifier.is_empty() {
        return Err(OAuthError::InvalidRequest.with("code and code_verifier are required"));
    }
    // Single-use: consume atomically, then validate. Every failure → invalid.
    let code = match store::consume_code(pool, &sha256_hex(&req.code)).await {
        Ok(Some(c)) => c,
        Ok(None) => return Err(invalid()),
        Err(e) => {
            tracing::warn!(target: "oauth", error = %e, "consume_code failed");
            return Err(OAuthError::ServerError.with("internal error"));
        }
    };
    if code.expires_at <= Utc::now()
        || code.client_id != req.client_id
        || code.redirect_uri != req.redirect_uri
        || !pkce::verify_s256(&req.code_verifier, &code.code_challenge)
    {
        return Err(invalid());
    }

    issue_tokens(
        state,
        pool,
        Uuid::now_v7(), // new rotation family
        &code.client_id,
        &code.scope,
        &code.resource,
        code.user_id,
        code.org_id,
        code.refresh_expires_at,
    )
    .await
}

async fn refresh_grant(
    state: &AppState,
    pool: &sqlx::PgPool,
    req: TokenRequest,
) -> Result<Response, OAuthErrorResponse> {
    if req.refresh_token.is_empty() || req.client_id.is_empty() {
        return Err(OAuthError::InvalidRequest.with("refresh_token and client_id are required"));
    }
    let hash = sha256_hex(&req.refresh_token);
    let rt = match store::get_refresh_token(pool, &hash).await {
        Ok(Some(rt)) => rt,
        Ok(None) => return Err(invalid()),
        Err(e) => {
            tracing::warn!(target: "oauth", error = %e, "get_refresh_token failed");
            return Err(OAuthError::ServerError.with("internal error"));
        }
    };

    // The refresh token is bound to its client; a mismatch is invalid.
    if rt.client_id != req.client_id {
        return Err(invalid());
    }

    // Replay of an already-rotated token ⇒ theft. Burn the whole family and the
    // live access token so the thief and the victim both lose access; the user
    // must reconnect.
    if rt.used_at.is_some() {
        tracing::warn!(
            target: "oauth",
            family = %rt.family_id,
            "refresh token replay detected; revoking family"
        );
        revoke_family(pool, &rt).await;
        return Err(invalid());
    }
    if rt.expires_at <= Utc::now() {
        return Err(invalid());
    }

    // Rotate: flip this token to used. If we lost the CAS, another concurrent
    // refresh already rotated it — that winner issued the next token, so this
    // one is simply invalid (not necessarily theft; don't nuke the family).
    match store::mark_refresh_used(pool, &hash).await {
        Ok(true) => {}
        Ok(false) => return Err(invalid()),
        Err(e) => {
            tracing::warn!(target: "oauth", error = %e, "mark_refresh_used failed");
            return Err(OAuthError::ServerError.with("internal error"));
        }
    }

    // New tokens, same family, same absolute connection deadline + scope
    // (refresh can never widen scope, cross orgs, or change audience).
    issue_tokens(
        state,
        pool,
        rt.family_id,
        &rt.client_id,
        &rt.scope,
        &rt.resource,
        rt.user_id,
        rt.org_id,
        rt.expires_at,
    )
    .await
}

/// Burn a refresh family + its access token (theft response).
async fn revoke_family(pool: &sqlx::PgPool, rt: &store::RefreshToken) {
    if let Err(e) = store::delete_refresh_family(pool, rt.family_id).await {
        tracing::warn!(target: "oauth", error = %e, "delete_refresh_family failed");
    }
    if let Err(e) =
        api_tokens::revoke_oauth(pool, rt.user_id, rt.org_id, &rt.client_id, &rt.resource).await
    {
        tracing::warn!(target: "oauth", error = %e, "revoke_oauth failed");
    }
}

/// Mint a fresh access token + refresh token and build the token response.
#[allow(clippy::too_many_arguments)]
async fn issue_tokens(
    state: &AppState,
    pool: &sqlx::PgPool,
    family_id: Uuid,
    client_id: &str,
    scope: &str,
    resource: &str,
    user: UserId,
    org: OrgId,
    refresh_expires_at: DateTime<Utc>,
) -> Result<Response, OAuthErrorResponse> {
    let scopes = ScopeSet::from_strs(scope.split_whitespace());
    let prefix_len = state.cfg.auth.api_tokens.prefix_visible_chars as usize;
    let access_expires_at =
        Utc::now() + Duration::seconds(i64::from(state.cfg.mcp.access_token_ttl_secs));

    let minted = api_tokens::mint_oauth(
        pool,
        user,
        org,
        "MCP connector",
        &scopes,
        access_expires_at,
        prefix_len,
        resource,
        client_id,
    )
    .await
    .map_err(|e| {
        tracing::warn!(target: "oauth", error = %e, "mint_oauth failed");
        OAuthError::ServerError.with("could not issue token")
    })?;

    let mut bytes = [0u8; 32];
    SysRng
        .try_fill_bytes(&mut bytes)
        .expect("SysRng must succeed for refresh token");
    let refresh = URL_SAFE_NO_PAD.encode(bytes);
    store::insert_refresh_token(
        pool,
        &sha256_hex(&refresh),
        family_id,
        client_id,
        scope,
        resource,
        user,
        org,
        refresh_expires_at,
    )
    .await
    .map_err(|e| {
        tracing::warn!(target: "oauth", error = %e, "insert_refresh_token failed");
        OAuthError::ServerError.with("could not issue token")
    })?;

    let expires_in = (access_expires_at - Utc::now()).num_seconds().max(0);
    let mut headers = HeaderMap::new();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok((
        headers,
        Json(json!({
            "access_token": minted.token,
            "token_type": "Bearer",
            "expires_in": expires_in,
            "refresh_token": refresh,
            "scope": scope,
        })),
    )
        .into_response())
}
