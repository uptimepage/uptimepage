//! `/api/v1/me/*` — current user + session management endpoints.
//!
//! `/me/orgs` and `/me/active-org` already live in `handlers::orgs`; this
//! module owns the bits that are pure session/user surface.

use crate::api::json::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tower_cookies::Cookies;
use utoipa::ToSchema;

use crate::app::AppState;
use crate::auth::session as session_store;
use crate::domain::{AppTheme, TimeFormat, UserId};
use crate::error::{AppError, Result};
use crate::storage::users as users_store;
use crate::web::auth::Session;
use crate::web::theme as theme_cookie;
use crate::web::time_format as time_format_cookie;

#[derive(Debug, Serialize, ToSchema)]
pub struct MeView {
    pub id: UserId,
    pub email: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub email_verified: bool,
}

pub async fn me(State(state): State<AppState>, session: Session) -> Result<Json<MeView>> {
    let user = session.user.as_ref().ok_or(AppError::Unauthorized)?;
    let pool = state.db.as_ref().ok_or(AppError::Unauthorized)?;
    let row: Option<(Option<String>, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT display_name, email_verified_at FROM users WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(user.id.0)
    .fetch_optional(pool)
    .await
    .map_err(|e| AppError::Other(anyhow::anyhow!("me: {e}")))?;
    let (display_name, verified) = row.unwrap_or((None, None));
    Ok(Json(MeView {
        id: user.id,
        email: user.email.clone(),
        display_name,
        email_verified: verified.is_some(),
    }))
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SessionView {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent_hash: Option<String>,
    /// True for the cookie that produced this request — UI marks it
    /// "this device" so users don't revoke themselves by mistake.
    pub is_current: bool,
}

pub async fn list_sessions(
    State(state): State<AppState>,
    session: Session,
) -> Result<Json<Vec<SessionView>>> {
    let user = session.user.as_ref().ok_or(AppError::Unauthorized)?;
    let pool = state.db.as_ref().ok_or(AppError::Unauthorized)?;
    let rows = session_store::list_for_user(pool, user.id).await?;
    let current = session.session_id_hash.as_deref();
    Ok(Json(
        rows.into_iter()
            .map(|r| SessionView {
                is_current: Some(r.id_hash.as_str()) == current,
                id: r.id_hash,
                created_at: r.created_at,
                last_used_at: r.last_used_at,
                expires_at: r.expires_at,
                ip_hash: r.ip_hash,
                user_agent_hash: r.user_agent_hash,
            })
            .collect(),
    ))
}

pub async fn revoke_session(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    let user = session.user.as_ref().ok_or(AppError::Unauthorized)?;
    let pool = state.db.as_ref().ok_or(AppError::Unauthorized)?;
    let removed = session_store::destroy_for_user(pool, user.id, &id).await?;
    if !removed {
        return Err(AppError::not_found(
            "SESSION_NOT_FOUND",
            "session not found",
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ThemeView {
    pub theme: AppTheme,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateThemeRequest {
    pub theme: AppTheme,
}

pub async fn get_theme(State(state): State<AppState>, session: Session) -> Result<Json<ThemeView>> {
    let user = session.user.as_ref().ok_or(AppError::Unauthorized)?;
    let pool = state.db.as_ref().ok_or(AppError::Unauthorized)?;
    let theme = users_store::get_theme(pool, user.id).await?;
    Ok(Json(ThemeView { theme }))
}

pub async fn update_theme(
    State(state): State<AppState>,
    session: Session,
    cookies: Cookies,
    Json(req): Json<UpdateThemeRequest>,
) -> Result<Json<ThemeView>> {
    let user = session.user.as_ref().ok_or(AppError::Unauthorized)?;
    let pool = state.db.as_ref().ok_or(AppError::Unauthorized)?;
    let ok = users_store::set_theme(pool, user.id, req.theme).await?;
    if !ok {
        return Err(AppError::not_found("USER_NOT_FOUND", "user not found"));
    }
    cookies.add(theme_cookie::build_cookie(
        req.theme,
        state.cfg.auth.session.cookie_secure,
    ));
    Ok(Json(ThemeView { theme: req.theme }))
}

#[derive(Debug, Serialize, ToSchema)]
pub struct TimeFormatView {
    pub time_format: TimeFormat,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateTimeFormatRequest {
    pub time_format: TimeFormat,
}

pub async fn get_time_format(
    State(state): State<AppState>,
    session: Session,
) -> Result<Json<TimeFormatView>> {
    let user = session.user.as_ref().ok_or(AppError::Unauthorized)?;
    let pool = state.db.as_ref().ok_or(AppError::Unauthorized)?;
    let time_format = users_store::get_time_format(pool, user.id).await?;
    Ok(Json(TimeFormatView { time_format }))
}

pub async fn update_time_format(
    State(state): State<AppState>,
    session: Session,
    cookies: Cookies,
    Json(req): Json<UpdateTimeFormatRequest>,
) -> Result<Json<TimeFormatView>> {
    let user = session.user.as_ref().ok_or(AppError::Unauthorized)?;
    let pool = state.db.as_ref().ok_or(AppError::Unauthorized)?;
    let ok = users_store::set_time_format(pool, user.id, req.time_format).await?;
    if !ok {
        return Err(AppError::not_found("USER_NOT_FOUND", "user not found"));
    }
    cookies.add(time_format_cookie::build_cookie(
        req.time_format,
        state.cfg.auth.session.cookie_secure,
    ));
    Ok(Json(TimeFormatView {
        time_format: req.time_format,
    }))
}
