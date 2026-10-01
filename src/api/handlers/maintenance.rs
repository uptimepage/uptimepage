//! Operator endpoints for maintenance window CRUD.
//!
//! Standard `ApiError` envelope. Mounted under `/api/v1/maintenance` so the
//! app's own auth boundary applies. The public surface reads maintenance
//! through `PublicSource::maintenance`, never through this handler.

use crate::api::json::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::AppendHeaders;
use chrono::{Duration as ChronoDuration, Utc};
use serde::Deserialize;
use utoipa::IntoParams;
use uuid::Uuid;

use crate::api::handlers::validation;
use crate::app::AppState;
use crate::domain::{
    MaintenanceFilter, MaintenanceWindow, MaintenanceWindowUpdate, NewMaintenanceWindow, OrgId,
    WindowPhase,
};
use crate::error::ApiError;
use crate::error::codes;
use crate::error::{AppError, Result};
use crate::pagination::page::{PageEnvelope, PageOfMaintenanceWindow};
use crate::request::{
    Authorized, CurrentUser, MaintenanceDelete, MaintenanceRead, MaintenanceWrite, RequestSource,
};
use crate::storage::{MaintenanceListQuery, MaintenanceStore};

const MAX_WINDOW_DAYS: i64 = 30;
const LIST_LIMIT_DEFAULT: u32 = 50;
const LIST_LIMIT_MAX: u32 = 200;

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    /// Filter: active | upcoming | past | all (default: all). `past` is windows
    /// that ended or were cancelled; `all` includes cancelled windows.
    #[serde(default)]
    pub status: Option<MaintenanceFilter>,
    /// Page size (default 50, max 200).
    pub limit: Option<u32>,
    /// Page offset.
    pub offset: Option<u32>,
}

#[utoipa::path(
    post,
    path = "/api/v1/maintenance",
    tag = "maintenance",
    summary = "Schedule a maintenance window",
    request_body(content = NewMaintenanceWindow, example = json!({
        "title": "Database upgrade",
        "description": "Brief read-only window during PG13 → PG16 migration.",
        "starts_at": "2026-05-14T22:00:00Z",
        "ends_at":   "2026-05-14T23:00:00Z",
        "component_ids": ["01a7b1ce-0000-7000-8000-000000000001"]
    })),
    responses(
        (status = 201, body = MaintenanceWindow,
            headers(("Location" = String, description = "URL of the new maintenance window"))),
        (status = 400, body = ApiError,
            description = "Validation error: ends_at <= starts_at, ends_at not in the future, \
                           unknown component ids, title empty/too long, window longer than the \
                           configured limit"),
    ),
)]
pub async fn create_maintenance(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<MaintenanceWrite>,
    CurrentUser(actor): CurrentUser,
    RequestSource(source): RequestSource,
    Json(new): Json<NewMaintenanceWindow>,
) -> Result<(
    StatusCode,
    AppendHeaders<[(axum::http::HeaderName, HeaderValue); 1]>,
    Json<MaintenanceWindow>,
)> {
    validation::validate_title(&new.title, "title")?;
    validation::validate_description(new.description.as_deref(), "description")?;
    validate_time_range(new.starts_at, new.ends_at)?;
    require_future_end(new.ends_at)?;
    validate_component_ids(state.maintenance_store.as_ref(), org, &new.component_ids).await?;
    // Handler-entry quota check (friendly 422). Maintenance windows are a
    // singular, low-concurrency create — store-level atomic enforcement is a
    // tracked follow-up; the headline atomic path is targets.
    state
        .quotas
        .check_can_create_maintenance_window(org, None)
        .await?;
    let mw = state
        .maintenance_store
        .create(org, new, source, Some(actor))
        .await?;
    let location =
        HeaderValue::from_str(&format!("/api/v1/maintenance/{}", mw.id)).expect("uuid ascii");
    Ok((
        StatusCode::CREATED,
        AppendHeaders([(header::LOCATION, location)]),
        Json(mw),
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/maintenance",
    tag = "maintenance",
    summary = "List maintenance windows",
    params(ListQuery),
    responses(
        (status = 200, body = PageOfMaintenanceWindow),
    ),
)]
pub async fn list_maintenance(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<MaintenanceRead>,
    Query(q): Query<ListQuery>,
) -> Result<Json<PageEnvelope<MaintenanceWindow>>> {
    let limit = q
        .limit
        .unwrap_or(LIST_LIMIT_DEFAULT)
        .clamp(1, LIST_LIMIT_MAX);
    let offset = q.offset.unwrap_or(0);
    let filter = q.status.unwrap_or_default();
    let peek = state
        .maintenance_store
        .list(
            org,
            MaintenanceListQuery {
                filter,
                limit: limit + 1,
                offset,
            },
        )
        .await?;
    Ok(Json(PageEnvelope::from_peek(peek, limit, offset)))
}

#[utoipa::path(
    get,
    path = "/api/v1/maintenance/{id}",
    tag = "maintenance",
    summary = "Get a maintenance window",
    params(("id" = Uuid, Path)),
    responses(
        (status = 200, body = MaintenanceWindow),
        (status = 404, body = ApiError),
    ),
)]
pub async fn get_maintenance(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<MaintenanceRead>,
    Path(id): Path<Uuid>,
) -> Result<Json<MaintenanceWindow>> {
    match state.maintenance_store.get(org, id).await? {
        Some(mw) => Ok(Json(mw)),
        None => Err(AppError::not_found(
            codes::MAINTENANCE_NOT_FOUND,
            "maintenance window not found",
        )),
    }
}

#[utoipa::path(
    patch,
    path = "/api/v1/maintenance/{id}",
    tag = "maintenance",
    summary = "Edit a maintenance window",
    description = "Editing a window whose `ends_at` is already in the past, or that was cancelled, \
                   is rejected with 422. `ends_at` must stay in the future, except on a running \
                   window: there, an `ends_at` at or before the current time, with `starts_at` \
                   unchanged, ends the window now at the server's clock. A body that sets nothing \
                   changes nothing and is not recorded.",
    params(("id" = Uuid, Path)),
    request_body(content = MaintenanceWindowUpdate),
    responses(
        (status = 200, body = MaintenanceWindow),
        (status = 400, body = ApiError),
        (status = 404, body = ApiError),
        (status = 422, body = ApiError, description = "Cannot edit a completed or cancelled maintenance window"),
    ),
)]
pub async fn update_maintenance(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<MaintenanceWrite>,
    CurrentUser(actor): CurrentUser,
    RequestSource(source): RequestSource,
    Path(id): Path<Uuid>,
    Json(mut update): Json<MaintenanceWindowUpdate>,
) -> Result<Json<MaintenanceWindow>> {
    let now = Utc::now();
    // The store re-checks that the window has not ended, so an edit that loses
    // a race with the window ending or being ended reports 404 instead of
    // reviving it.
    let existing = state.maintenance_store.get(org, id).await?.ok_or_else(|| {
        AppError::not_found(codes::MAINTENANCE_NOT_FOUND, "maintenance window not found")
    })?;
    reject_closed(existing.phase(now))?;
    if update.changed_fields().is_empty() {
        return Ok(Json(existing));
    }
    if let Some(t) = update.title.as_deref() {
        validation::validate_title(t, "title")?;
    }
    validation::validate_description(update.description.as_deref(), "description")?;
    let ends_now = pin_end_now(&existing, &mut update, now);
    let starts = update.starts_at.unwrap_or(existing.starts_at);
    let ends = update.ends_at.unwrap_or(existing.ends_at);
    validate_time_range(starts, ends)?;
    if update.ends_at.is_some() && !ends_now {
        require_future_end(ends)?;
    }
    if let Some(ids) = update.component_ids.as_deref() {
        validate_component_ids(state.maintenance_store.as_ref(), org, ids).await?;
    }
    match state
        .maintenance_store
        .update(org, id, update, source, Some(actor))
        .await?
    {
        Some(mw) => Ok(Json(mw)),
        None => Err(AppError::not_found(
            codes::MAINTENANCE_NOT_FOUND,
            "maintenance window not found",
        )),
    }
}

#[utoipa::path(
    delete,
    path = "/api/v1/maintenance/{id}",
    tag = "maintenance",
    summary = "Cancel a maintenance window",
    description = "Cancels a window that has not ended. The window is kept as history with \
                   `deleted_at` and `deleted_by` set: it stops showing publicly, stops holding \
                   paging and stops counting toward the quota, and stays listed under `past` and \
                   `all`. Cancelling it again returns 404, and a window that already ended is \
                   history and is rejected with 422.",
    params(("id" = Uuid, Path)),
    responses(
        (status = 204, description = "Cancelled"),
        (status = 404, body = ApiError),
        (status = 422, body = ApiError, description = "Cannot cancel a completed maintenance window"),
    ),
)]
pub async fn delete_maintenance(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<MaintenanceDelete>,
    CurrentUser(actor): CurrentUser,
    RequestSource(source): RequestSource,
    Path(id): Path<Uuid>,
) -> Result<StatusCode> {
    if state
        .maintenance_store
        .delete(org, id, source, Some(actor))
        .await?
    {
        return Ok(StatusCode::NO_CONTENT);
    }
    let finished = state
        .maintenance_store
        .get(org, id)
        .await?
        .is_some_and(|w| w.phase(Utc::now()) == WindowPhase::Completed);
    if finished {
        return Err(AppError::unprocessable(
            codes::MAINTENANCE_COMPLETED,
            "cannot cancel a completed maintenance window",
        ));
    }
    Err(AppError::not_found(
        codes::MAINTENANCE_NOT_FOUND,
        "maintenance window not found",
    ))
}

// ── Validation ──────────────────────────────────────────────────────────

/// A completed or cancelled window is history, so it cannot be edited.
fn reject_closed(phase: WindowPhase) -> Result<()> {
    match phase {
        WindowPhase::Cancelled => Err(AppError::unprocessable(
            codes::MAINTENANCE_CANCELLED,
            "cannot edit a cancelled maintenance window",
        )),
        WindowPhase::Completed => Err(AppError::unprocessable(
            codes::MAINTENANCE_COMPLETED,
            "cannot edit a completed maintenance window",
        )),
        WindowPhase::Upcoming | WindowPhase::Active => Ok(()),
    }
}

/// An `ends_at` at or before `now` on a running window whose start is left
/// alone means "stop now": it is replaced by the server's clock instead of the
/// client's. Returns whether it did.
fn pin_end_now(
    existing: &MaintenanceWindow,
    update: &mut MaintenanceWindowUpdate,
    now: chrono::DateTime<Utc>,
) -> bool {
    let ends_now = existing.phase(now) == WindowPhase::Active
        && update.ends_at.is_some_and(|ends| ends <= now)
        && update
            .starts_at
            .is_none_or(|starts| starts == existing.starts_at);
    if ends_now {
        update.ends_at = Some(now);
    }
    ends_now
}

fn validate_time_range(starts: chrono::DateTime<Utc>, ends: chrono::DateTime<Utc>) -> Result<()> {
    if ends <= starts {
        return Err(AppError::bad_request_field(
            codes::INVALID_TIME_RANGE,
            "ends_at must be strictly after starts_at",
            "ends_at",
        ));
    }
    if ends - starts > ChronoDuration::days(MAX_WINDOW_DAYS) {
        return Err(AppError::bad_request_field(
            codes::INVALID_DURATION,
            format!("maintenance window cannot exceed {MAX_WINDOW_DAYS} days"),
            "ends_at",
        ));
    }
    Ok(())
}

fn require_future_end(ends: chrono::DateTime<Utc>) -> Result<()> {
    if ends <= Utc::now() {
        return Err(AppError::bad_request_field(
            codes::INVALID_TIME_RANGE,
            "ends_at must be in the future",
            "ends_at",
        ));
    }
    Ok(())
}

async fn validate_component_ids(
    store: &dyn MaintenanceStore,
    org: OrgId,
    ids: &[Uuid],
) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let known = store.existing_target_ids(org, ids).await?;
    let unknown: Vec<Uuid> = ids
        .iter()
        .copied()
        .filter(|id| !known.contains(id))
        .collect();
    if !unknown.is_empty() {
        return Err(AppError::bad_request_field(
            codes::INVALID_COMPONENT_ID,
            format!("{} component id(s) do not exist", unknown.len()),
            "component_ids",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;

    #[test]
    fn validate_time_range_rejects_zero_duration() {
        let t = Utc::now();
        assert!(matches!(
            validate_time_range(t, t),
            Err(AppError::BadRequest { code, .. }) if code == codes::INVALID_TIME_RANGE
        ));
    }

    #[test]
    fn validate_time_range_rejects_too_long() {
        let s = Utc::now();
        let e = s + ChronoDuration::days(MAX_WINDOW_DAYS + 1);
        assert!(matches!(
            validate_time_range(s, e),
            Err(AppError::BadRequest { code, .. }) if code == codes::INVALID_DURATION
        ));
    }

    #[test]
    fn validate_time_range_accepts_normal_window() {
        let s = Utc::now();
        let e = s + ChronoDuration::hours(2);
        assert!(validate_time_range(s, e).is_ok());
    }
}
