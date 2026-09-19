//! Request → org resolution + cookie- or token-backed authentication.
//!
//! Two auth paths feed the same extractor surface:
//!
//! 1. **Session cookie** — `_sm_session` resolved via [`crate::auth::session`].
//!    Carries `active_org_id` and falls back to the user's default org
//! 2. **API token** — `Authorization: Bearer sm_live_…` resolved by the
//!    [`api_token`] middleware ahead of routing. The token carries no active
//!    org, so handlers that need one must read an explicit org slug from the
//!    `X-Uptimepage-Org` header — otherwise [`CurrentOrg`] returns 400
//!    `ORG_REQUIRED`.
//!
//! [`CurrentOrg`] is the only extractor that hands a handler an `OrgId`.
//! Combined with the org-scoped repositories in `src/storage/`, this is what
//! makes "forgetting to scope a query" a compile error rather than a security
//! incident: the repos require an `OrgId`, and the only place to obtain one
//! inside a request is this extractor.

pub mod agent;
pub mod api_token;
pub mod authz;
pub mod csrf;
pub mod operator;

/// `Authorization: Bearer <raw>` → trimmed `<raw>`, or `None` for anything
/// else. Used by both the API-token middleware (to decide whether to look the
/// token up) and the CSRF guard (to know a request is Bearer-authenticated and
/// therefore exempt from the cookie-targeted CSRF rule).
pub fn bearer_from_headers(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::trim)
}

use axum::extract::{FromRef, FromRequestParts, OptionalFromRequestParts};
use axum::http::HeaderName;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Redirect, Response};
use std::convert::Infallible;
use tower_cookies::Cookies;
use uuid::Uuid;

use crate::auth::url::url_encode;

use crate::app::AppState;
use crate::auth::scope::ScopeSet;
use crate::auth::session as session_store;
use crate::domain::{OrgId, UserId};
use crate::error::codes;
use crate::error::{AppError, Result};
use crate::storage::orgs::is_active_member;

/// Custom header used by API-token clients to scope writes/reads to a specific
/// org. Tokens carry no active-org state, so this header (or a future slug
/// path param) is the only way the handler learns which org to read/write.
pub const ORG_HEADER: HeaderName = HeaderName::from_static("x-uptimepage-org");

#[derive(Debug, Clone)]
pub struct User {
    pub id: UserId,
    pub email: String,
}

/// Authenticated session. The cookie-backed extractor populates this from the
/// `sessions` table; tests can short-circuit by stamping one into request
/// extensions ahead of the extractor.
#[derive(Debug, Default, Clone)]
pub struct Session {
    pub user: Option<User>,
    /// Active org selected by the user via the org picker, or set by signup.
    /// `None` means "fall back to my default org" (oldest active membership).
    pub active_org_id: Option<OrgId>,
    /// SHA-256 hash of the cookie value (matches `sessions.id_hash`). Present
    /// iff this Session was constructed by the cookie path. Compared with
    /// `SessionListing.id_hash` to mark "this device" and used by targeted
    /// revoke. Never the raw cookie — that only lives in the request's
    /// `Set-Cookie` header.
    pub session_id_hash: Option<String>,
}

impl Session {
    pub fn user_id(&self) -> Option<UserId> {
        self.user.as_ref().map(|u| u.id)
    }
}

/// Result of authentication, populated either by the API-token middleware
/// (Bearer path) or by the session extractor (cookie path). Lives in request
/// extensions; [`CurrentUser`] and [`CurrentOrg`] read it.
#[derive(Debug, Clone)]
pub enum AuthContext {
    /// Cookie-based browser session. Carries an `active_org_id` that the user
    /// chose via the org picker; falls back to their default org.
    Session {
        user_id: UserId,
        session_id_hash: String,
        active_org_id: Option<OrgId>,
    },
    /// `Authorization: Bearer sm_live_…`. `org` is the token's binding: `Some`
    /// pins it to one org (header optional, must match if present); `None`
    /// leaves the org to the `X-Uptimepage-Org` header (any member org).
    ApiToken {
        user_id: UserId,
        token_id: Uuid,
        scopes: ScopeSet,
        org: Option<OrgId>,
    },
}

impl AuthContext {
    pub fn user_id(&self) -> UserId {
        match self {
            AuthContext::Session { user_id, .. } | AuthContext::ApiToken { user_id, .. } => {
                *user_id
            }
        }
    }
}

impl<S> FromRequestParts<S> for Session
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        if let Some(injected) = parts.extensions.get::<Session>().cloned() {
            return Ok(injected);
        }

        let app_state = AppState::from_ref(state);
        let Some(pool) = app_state.db.as_ref() else {
            return Ok(Session::default());
        };
        let Some((cookies, cookie_val)) = session_cookie(parts, &app_state) else {
            return Ok(Session::default());
        };
        let cookie_name = app_state.cfg.auth.session.cookie_name.as_str();

        match session_store::lookup(pool, &app_state.cfg.auth.session, &cookie_val).await {
            Ok(session_store::LookupOutcome::Active(row)) => {
                if let Err(err) = session_store::touch_last_used_debounced(
                    pool,
                    &app_state.session_debounce,
                    &row.id,
                )
                .await
                {
                    tracing::warn!(error = %err, "touch_last_used_debounced failed");
                }
                let user = load_user(pool, row.user_id).await;
                Ok(Session {
                    user,
                    active_org_id: row.active_org_id,
                    session_id_hash: Some(row.id),
                })
            }
            Ok(session_store::LookupOutcome::Expired) => {
                cookies.remove(
                    tower_cookies::Cookie::build((cookie_name.to_string(), String::new()))
                        .path("/")
                        .build(),
                );
                Ok(Session::default())
            }
            Ok(session_store::LookupOutcome::Missing) => Ok(Session::default()),
            Err(err) => {
                tracing::warn!(error = %err, "session lookup failed");
                Ok(Session::default())
            }
        }
    }
}

/// A signed-in user whose account is soft-deleted and inside its grace window.
/// Every other extractor treats them as signed out ([`load_user`] filters
/// `deleted_at IS NULL`), so this is the only door a pending-deletion session
/// opens: the restore choice and the restore itself.
#[derive(Debug, Clone)]
pub struct PendingDeletionUser {
    pub user_id: UserId,
    pub deleted_at: chrono::DateTime<chrono::Utc>,
    /// `sessions.id_hash` for this request — the restore stamps the
    /// just-untombstoned org onto it, since the session was minted while every
    /// org of theirs was still hidden.
    pub session_id_hash: String,
}

impl<S> FromRequestParts<S> for PendingDeletionUser
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self> {
        let app_state = AppState::from_ref(state);
        let pool = app_state.require_db()?;
        let (_, cookie_val) = session_cookie(parts, &app_state).ok_or(AppError::Unauthorized)?;
        let row = match session_store::lookup(pool, &app_state.cfg.auth.session, &cookie_val).await
        {
            Ok(session_store::LookupOutcome::Active(row)) => row,
            _ => return Err(AppError::Unauthorized),
        };
        // Not `load_user`: that hides deleted accounts, this finds them.
        let deleted_at: Option<(chrono::DateTime<chrono::Utc>,)> =
            sqlx::query_as("SELECT deleted_at FROM users WHERE id = $1 AND deleted_at IS NOT NULL")
                .bind(row.user_id.0)
                .fetch_optional(pool)
                .await
                .map_err(|e| AppError::Other(anyhow::anyhow!("pending-deletion lookup: {e}")))?;
        let (deleted_at,) = deleted_at.ok_or(AppError::Unauthorized)?;
        Ok(PendingDeletionUser {
            user_id: row.user_id,
            deleted_at,
            session_id_hash: row.id,
        })
    }
}

/// Browser-facing gate for the restore page: no pending deletion sends the
/// visitor to `/login` rather than rendering [`AppError`]'s JSON envelope in
/// their tab. Same split as [`CurrentUser`] and [`AuthedBrowser`].
impl<S> OptionalFromRequestParts<S> for PendingDeletionUser
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &S,
    ) -> Result<Option<Self>, Self::Rejection> {
        Ok(
            <PendingDeletionUser as FromRequestParts<S>>::from_request_parts(parts, state)
                .await
                .ok(),
        )
    }
}

/// The request's session cookie jar and raw cookie value, if it carries one.
fn session_cookie(parts: &Parts, app_state: &AppState) -> Option<(Cookies, String)> {
    let cookies = parts.extensions.get::<Cookies>().cloned()?;
    let value = cookies
        .get(app_state.cfg.auth.session.cookie_name.as_str())
        .map(|c| c.value().to_string())?;
    Some((cookies, value))
}

async fn load_user(pool: &sqlx::PgPool, user_id: UserId) -> Option<User> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT email::text FROM users WHERE id = $1 AND deleted_at IS NULL")
            .bind(user_id.0)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    row.map(|(email,)| User { id: user_id, email })
}

/// Caller identity extracted from either a session cookie or an API token.
/// Returns 401 if neither path produced a user. Handlers that need both the
/// active org and the caller use [`CurrentOrg`] and [`CurrentUser`] together.
#[derive(Debug, Clone, Copy)]
pub struct CurrentUser(pub UserId);

impl<S> FromRequestParts<S> for CurrentUser
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self> {
        if let Some(ctx) = parts.extensions.get::<AuthContext>() {
            return Ok(CurrentUser(ctx.user_id()));
        }
        let session = Session::from_request_parts(parts, state)
            .await
            .expect("Session extractor is infallible");
        session
            .user_id()
            .map(CurrentUser)
            .ok_or(AppError::Unauthorized)
    }
}

/// Org id for the current request. Constructed by the extractor; never by
/// hand. Wrapping `OrgId` in a separate newtype keeps the "this came from the
/// request" provenance visible at the type level.
#[derive(Debug, Clone, Copy)]
pub struct CurrentOrg(pub OrgId);

impl<S> FromRequestParts<S> for CurrentOrg
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self> {
        let app_state = AppState::from_ref(state);

        // API-token path: an unbound token MUST surface the org via
        // `X-Uptimepage-Org` (no fallback — a missing/unknown header is a 400).
        // A bound token implies its org; a header, if present, must match it.
        // Either way `is_active_member` re-checks (defense vs revoked access).
        if let Some(AuthContext::ApiToken { user_id, org, .. }) =
            parts.extensions.get::<AuthContext>()
        {
            let pool = app_state.require_db()?;
            let user_id = *user_id;
            let org = match *org {
                Some(bound) => {
                    if let Some(hdr) = optional_org_from_header(parts, pool).await?
                        && hdr != bound
                    {
                        return Err(AppError::forbidden_code(
                            codes::ORG_HEADER_MISMATCH,
                            "X-Uptimepage-Org names a different org than this token is bound to",
                        ));
                    }
                    bound
                }
                None => explicit_org_from_header(parts, pool).await?,
            };
            if !is_active_member(pool, user_id, org).await? {
                return Err(AppError::Forbidden);
            }
            return Ok(CurrentOrg(org));
        }

        let session = Session::from_request_parts(parts, state)
            .await
            .expect("Session extractor is infallible");

        let user_id = session.user_id().ok_or(AppError::Unauthorized)?;
        // Session must carry an active org. OAuth callback + signup stamp it
        // on creation, so a healthy session always has it. A missing one is
        // an unauthorised request, not a silent fallback to some default.
        let active = session.active_org_id.ok_or(AppError::Unauthorized)?;

        // Test-only fixture path: in-memory stores carry no `organizations`
        // table, so the membership check has nothing to verify against. The
        // session was injected through `Extension<Session>` and IS the test's
        // assertion. In production `db` is always `Some`, so this branch
        // never fires there.
        let Some(pool) = app_state.db.as_ref() else {
            return Ok(CurrentOrg(active));
        };
        if !is_active_member(pool, user_id, active).await? {
            return Err(AppError::Forbidden);
        }
        Ok(CurrentOrg(active))
    }
}

/// Redirect an unauthenticated visitor to `/login`, preserving where they
/// were headed. `next` is the raw path; it is URL-encoded here so call
/// sites never hand-roll `%2F` literals. Lives with the auth extractors
/// (not in the view layer) so the gate and the views can both reach it
/// without inverting the layering.
pub(crate) fn login_redirect(next: &str) -> Redirect {
    Redirect::to(&format!("/login?redirect_after={}", url_encode(next)))
}

/// Operator-UI gate for the server-rendered HTML pages. Resolves the cookie
/// [`Session`]; an unauthenticated browser is redirected to `/login` instead
/// of being served operator data — unlike [`CurrentUser`], whose 401 JSON
/// envelope is wrong for a page navigation. Carries no payload: adding it as
/// a handler parameter *is* the auth boundary, the same type-as-boundary rule
/// the API extractors follow (see the module docs).
#[derive(Debug, Clone, Copy)]
pub struct AuthedBrowser;

impl<S> FromRequestParts<S> for AuthedBrowser
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &S,
    ) -> std::result::Result<Self, Self::Rejection> {
        let session = Session::from_request_parts(parts, state)
            .await
            .expect("Session extractor is infallible");
        if session.user_id().is_some() {
            Ok(AuthedBrowser)
        } else {
            Err(login_redirect(parts.uri.path()).into_response())
        }
    }
}

/// Resolve `X-Uptimepage-Org` to an org id by slug when the header is present.
/// `Ok(None)` = header absent; a present-but-malformed or unknown value is
/// `ORG_HEADER_INVALID`. Used directly by bound tokens (header optional) and by
/// [`explicit_org_from_header`] for unbound tokens (header required).
async fn optional_org_from_header(parts: &Parts, pool: &sqlx::PgPool) -> Result<Option<OrgId>> {
    let Some(val) = parts.headers.get(&ORG_HEADER) else {
        return Ok(None);
    };
    let raw = val
        .to_str()
        .map_err(|_| {
            AppError::bad_request(codes::ORG_HEADER_INVALID, "X-Uptimepage-Org is not UTF-8")
        })?
        .trim();
    if raw.is_empty() {
        return Err(AppError::bad_request(
            codes::ORG_HEADER_INVALID,
            "X-Uptimepage-Org is empty",
        ));
    }
    crate::storage::orgs::find_id_by_slug(pool, raw)
        .await?
        .ok_or_else(|| {
            AppError::bad_request(
                codes::ORG_HEADER_INVALID,
                "no organization matches X-Uptimepage-Org",
            )
        })
        .map(Some)
}

/// `X-Uptimepage-Org` required: resolves the header or fails with
/// `ORG_REQUIRED`. The unbound-token path — there is no org to fall back to.
async fn explicit_org_from_header(parts: &Parts, pool: &sqlx::PgPool) -> Result<OrgId> {
    optional_org_from_header(parts, pool).await?.ok_or_else(|| {
        AppError::bad_request(
            codes::ORG_REQUIRED,
            "API tokens must scope to an org via the X-Uptimepage-Org header",
        )
    })
}
