//! API-token Bearer middleware + the session-only auth extractors.
//!
//! The middleware runs ahead of the route handler. If the request carries
//! `Authorization: Bearer sm_live_…`, it looks the token up (argon2-verify
//! against rows matching `token_prefix`), inserts an
//! [`AuthContext::ApiToken`] into request extensions, and lazily updates
//! `api_tokens.last_used_at`. Anything else falls through — the session
//! extractor handles cookies later.
//!
//! Invalid tokens (no row, all rows failed verify) short-circuit with 401
//! `INVALID_TOKEN`; a token minted for the MCP resource with 401
//! `TOKEN_AUDIENCE`. A missing or non-Bearer `Authorization` header is **not**
//! an error — the cookie path may still succeed.
//!
//! [`BrowserUser`] / [`VerifiedBrowserUser`] gate account-administration
//! endpoints to a browser session: they read the cookie, never `AuthContext`,
//! so an API token is rejected regardless of its scopes.

use axum::extract::{FromRef, FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::{Json, http::StatusCode};
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::api::error::{ApiError, ApiErrorBody, codes};
use crate::app::AppState;
use crate::auth::api_tokens;
use crate::domain::UserId;
use crate::error::{AppError, Result};

use super::{AuthContext, CurrentUser, Session, bearer_from_headers};

/// Middleware entry point. Mount on the API router so it runs ahead of any
/// `FromRequestParts` impl that reads `AuthContext`.
pub async fn middleware(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let Some(raw) = bearer_from_headers(req.headers()) else {
        return next.run(req).await;
    };
    if !raw.starts_with(api_tokens::TOKEN_PREFIX) {
        // Some other Bearer scheme — leave it for the session path / future
        // flows to interpret. Don't 401 here.
        return next.run(req).await;
    }

    let Some(pool) = state.db.as_ref() else {
        return unauthorized();
    };
    let prefix_len = state.cfg.auth.api_tokens.prefix_visible_chars as usize;
    match api_tokens::lookup_by_raw(pool, raw, prefix_len, None).await {
        Ok(api_tokens::LookupOutcome::Active(row)) => {
            let token_id = row.id;
            req.extensions_mut().insert(AuthContext::ApiToken {
                user_id: row.user_id,
                token_id,
                scopes: row.scopes,
                org: row.org,
            });
            // last_used_at debounce: most requests hit the cache and skip
            // the UPDATE entirely. Only spawn when the cache says a write is
            // due — saves a task allocation per Bearer request on the >99%
            // no-op path.
            if api_tokens::should_touch(&state.api_token_debounce, token_id) {
                let cache = state.api_token_debounce.clone();
                let pool = pool.clone();
                tokio::spawn(async move {
                    if let Err(err) =
                        api_tokens::touch_last_used_debounced(&pool, &cache, token_id).await
                    {
                        tracing::warn!(
                            error = %err,
                            "api_tokens::touch_last_used_debounced failed",
                        );
                    }
                });
            }
            next.run(req).await
        }
        Ok(api_tokens::LookupOutcome::Invalid) => unauthorized(),
        Ok(api_tokens::LookupOutcome::WrongAudience) => wrong_audience(),
        Err(err) => {
            tracing::warn!(error = %err, "api token lookup failed");
            unauthorized()
        }
    }
}

fn unauthorized() -> Response {
    reject(codes::INVALID_TOKEN, "API token is invalid or expired")
}

fn wrong_audience() -> Response {
    reject(
        codes::TOKEN_AUDIENCE,
        "this token was issued to an MCP connector and is only accepted at the MCP endpoint",
    )
}

fn reject(code: &'static str, message: &str) -> Response {
    let body = ApiError {
        error: ApiErrorBody::new(code, message),
    };
    (StatusCode::UNAUTHORIZED, Json(body)).into_response()
}

async fn ensure_email_verified(pool: &PgPool, user_id: UserId) -> Result<()> {
    let row: Option<(Option<DateTime<Utc>>,)> =
        sqlx::query_as("SELECT email_verified_at FROM users WHERE id = $1")
            .bind(user_id.0)
            .fetch_optional(pool)
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("verify lookup: {e}")))?;
    if row.and_then(|(v,)| v).is_some() {
        Ok(())
    } else {
        Err(AppError::forbidden_code(
            codes::EMAIL_NOT_VERIFIED,
            "this action requires a verified email",
        ))
    }
}

/// Session-authenticated user for account-administration endpoints that API
/// tokens must never reach — managing the tokens themselves. A Bearer-only
/// request carries no session cookie, so it 401s here instead of letting a
/// scoped token mint or revoke siblings.
#[derive(Debug, Clone, Copy)]
pub struct BrowserUser(pub CurrentUser);

impl<S> FromRequestParts<S> for BrowserUser
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self> {
        let session = Session::from_request_parts(parts, state)
            .await
            .expect("Session extractor is infallible");
        session
            .user_id()
            .map(|id| BrowserUser(CurrentUser(id)))
            .ok_or_else(|| {
                if matches!(
                    parts.extensions.get::<AuthContext>(),
                    Some(AuthContext::ApiToken { .. })
                ) {
                    AppError::SessionRequired
                } else {
                    AppError::Unauthorized
                }
            })
    }
}

/// [`BrowserUser`] plus the verified-email gate. Required by `POST
/// /api/v1/me/api-tokens` (a compromised unverified account could otherwise
/// exfiltrate via a fresh token) and `POST /api/v1/orgs/{id}/invitations` (the
/// inviter's address is shown to recipients).
#[derive(Debug, Clone, Copy)]
pub struct VerifiedBrowserUser(pub CurrentUser);

impl<S> FromRequestParts<S> for VerifiedBrowserUser
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self> {
        let app_state = AppState::from_ref(state);
        let BrowserUser(user) = BrowserUser::from_request_parts(parts, state).await?;
        let pool = app_state.db.as_ref().ok_or(AppError::Unauthorized)?;
        ensure_email_verified(pool, user.0).await?;
        Ok(Self(user))
    }
}
