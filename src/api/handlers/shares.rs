//! Operator API for a monitor's share links (`/api/v1/targets/{id}/shares`).
//!
//! Minting a share is a monitor action, so it gates on member-level
//! `TargetsWrite` (not owner-only). `POST` mints, `GET` lists, `DELETE`
//! revokes. Both `POST` and `GET` return the raw `token` so the owner can
//! re-copy the `/m/{token}` link (stored encrypted at rest). The public read
//! surface those tokens unlock lives in `web::views::share`.

use crate::api::json::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::Utc;
use uuid::Uuid;

use crate::api::ApiError;
use crate::api::error::codes;
use crate::app::AppState;
use crate::domain::{MonitorShare, MonitorShareId, NewMonitorShare};
use crate::error::{AppError, Result};
use crate::storage::CreateShareOutcome;
use crate::web::{Authorized, CurrentUser, TargetsWrite};

/// Maximum share-label length (mirrors the `monitor_shares_label_length` CHECK).
const LABEL_MAX: usize = 80;

fn target_not_found() -> AppError {
    AppError::not_found(codes::TARGET_NOT_FOUND, "monitor not found")
}

fn share_not_found() -> AppError {
    AppError::not_found(codes::SHARE_NOT_FOUND, "share link not found")
}

/// Trim the label to `None` when blank; reject when over the length cap.
fn clean_label(label: Option<String>) -> Result<Option<String>> {
    let Some(raw) = label else { return Ok(None) };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.chars().count() > LABEL_MAX {
        return Err(AppError::bad_request_field(
            codes::SHARE_LABEL_INVALID,
            format!("label must be at most {LABEL_MAX} characters"),
            "label",
        ));
    }
    Ok(Some(trimmed.to_string()))
}

#[utoipa::path(
    post,
    path = "/api/v1/targets/{id}/shares",
    tag = "shares",
    summary = "Create a read-only share link for a monitor",
    description = "Mints a capability link (`/m/{token}`) that renders this \
                   monitor's read-only detail view to anyone with it, no account. \
                   Credentials in the check config are redacted. The returned \
                   `token` builds the URL as `/m/{token}`; it stays re-copyable \
                   via the list endpoint. Optional `expires_at`; omit for a link \
                   that never expires.",
    params(("id" = Uuid, Path, description = "Target id")),
    request_body = NewMonitorShare,
    responses(
        (status = 201, body = MonitorShare),
        (status = 400, description = "Invalid label or expiry in the past", body = ApiError),
        (status = 404, description = "Monitor not found", body = ApiError),
        (status = 422, description = "Plan limit reached: max_share_links_per_monitor or max_shared_monitors", body = ApiError),
    ),
)]
pub async fn create_share(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<TargetsWrite>,
    CurrentUser(user): CurrentUser,
    Path(target_id): Path<Uuid>,
    Json(req): Json<NewMonitorShare>,
) -> Result<(StatusCode, Json<MonitorShare>)> {
    if let Some(expires_at) = req.expires_at
        && expires_at <= Utc::now()
    {
        return Err(AppError::bad_request_field(
            codes::INVALID_EXPIRY,
            "expires_at must be in the future",
            "expires_at",
        ));
    }
    let new = NewMonitorShare {
        label: clean_label(req.label)?,
        expires_at: req.expires_at,
    };
    let plan = state.quotas.limit_for_org(org).await?;
    let outcome = state
        .monitor_share_store
        .create(
            org,
            target_id,
            new,
            Some(user),
            Some(i64::from(plan.max_share_links_per_monitor)),
            Some(i64::from(plan.max_shared_monitors)),
        )
        .await?;
    // A cap hit is recorded to quota_events (like every other quota block) so
    // the usage/abuse view sees share-cap blocks too.
    let blocked = |quota: &'static str, limit: i64| {
        state
            .quotas
            .record_block(org, Some(user), quota, limit, limit);
        AppError::quota_exceeded(quota, limit, limit, plan.id.clone())
    };
    match outcome {
        CreateShareOutcome::Created(c) => Ok((StatusCode::CREATED, Json(c.share))),
        CreateShareOutcome::TargetNotFound => Err(target_not_found()),
        CreateShareOutcome::PerMonitorLimit => Err(blocked(
            "max_share_links_per_monitor",
            i64::from(plan.max_share_links_per_monitor),
        )),
        CreateShareOutcome::OrgMonitorLimit => Err(blocked(
            "max_shared_monitors",
            i64::from(plan.max_shared_monitors),
        )),
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/targets/{id}/shares",
    tag = "shares",
    summary = "List a monitor's live share links",
    description = "Returns non-revoked shares, each with its `token` so the link \
                   can be re-copied. The token is `null` only when it was stored \
                   encrypted and no KEK is configured to decrypt it.",
    params(("id" = Uuid, Path, description = "Target id")),
    responses(
        (status = 200, body = [MonitorShare]),
        (status = 404, description = "Monitor not found", body = ApiError),
    ),
)]
pub async fn list_shares(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<TargetsWrite>,
    Path(target_id): Path<Uuid>,
) -> Result<Json<Vec<MonitorShare>>> {
    // Resolve the monitor first so a foreign/unknown id 404s instead of
    // returning an empty list (no populated-vs-empty oracle).
    state
        .target_store
        .get(org, target_id)
        .await?
        .ok_or_else(target_not_found)?;
    let shares = state
        .monitor_share_store
        .list_for_target(org, target_id)
        .await?;
    Ok(Json(shares))
}

#[utoipa::path(
    delete,
    path = "/api/v1/targets/{id}/shares/{share_id}",
    tag = "shares",
    summary = "Revoke a share link",
    description = "Revoked immediately; the capability URL 404s on the next request.",
    params(
        ("id" = Uuid, Path, description = "Target id"),
        ("share_id" = Uuid, Path, description = "Share id"),
    ),
    responses(
        (status = 204, description = "Revoked"),
        (status = 404, description = "Share not found or already revoked", body = ApiError),
    ),
)]
pub async fn revoke_share(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<TargetsWrite>,
    CurrentUser(user): CurrentUser,
    Path((target_id, share_id)): Path<(Uuid, MonitorShareId)>,
) -> Result<StatusCode> {
    if state
        .monitor_share_store
        .revoke(org, target_id, share_id, Some(user))
        .await?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(share_not_found())
    }
}
