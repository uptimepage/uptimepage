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

use crate::api::ApiError;
use crate::api::error::codes;
use crate::api::handlers::validation;
use crate::api::page::{PageEnvelope, PageOfMaintenanceWindow};
use crate::app::AppState;
use crate::domain::{
    MaintenanceFilter, MaintenanceWindow, MaintenanceWindowUpdate, NewMaintenanceWindow, OrgId,
};
use crate::error::{AppError, Result};
use crate::storage::{MaintenanceListQuery, MaintenanceStore};
use crate::web::{Authorized, MaintenanceDelete, MaintenanceRead, MaintenanceWrite, RequestSource};

const MAX_WINDOW_DAYS: i64 = 30;
const LIST_LIMIT_DEFAULT: u32 = 50;
const LIST_LIMIT_MAX: u32 = 200;

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    /// Filter: active | upcoming | past | all (default: all).
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
            description = "Validation error: ends_at <= starts_at, unknown component ids, \
                           title empty/too long, window longer than the configured limit"),
    ),
)]
pub async fn create_maintenance(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<MaintenanceWrite>,
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
    validate_component_ids(state.maintenance_store.as_ref(), org, &new.component_ids).await?;
    // Handler-entry quota check (friendly 422). Maintenance windows are a
    // singular, low-concurrency create — store-level atomic enforcement is a
    // tracked follow-up; the headline atomic path is targets.
    state
        .quotas
        .check_can_create_maintenance_window(org, None)
        .await?;
    let mw = state.maintenance_store.create(org, new, source).await?;
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
    description = "Editing a window whose `ends_at` is already in the past is rejected with 422.",
    params(("id" = Uuid, Path)),
    request_body(content = MaintenanceWindowUpdate),
    responses(
        (status = 200, body = MaintenanceWindow),
        (status = 400, body = ApiError),
        (status = 404, body = ApiError),
        (status = 422, body = ApiError, description = "Cannot edit completed maintenance window"),
    ),
)]
pub async fn update_maintenance(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<MaintenanceWrite>,
    RequestSource(source): RequestSource,
    Path(id): Path<Uuid>,
    Json(update): Json<MaintenanceWindowUpdate>,
) -> Result<Json<MaintenanceWindow>> {
    // Narrow TOCTOU: a concurrent operator PATCH that flips ends_at into the
    // past between this GET and the UPDATE below could let one final edit slip
    // through. The window is tiny (single-digit ms in normal operation) and
    // the data loss is just "a successful edit on a just-now-completed
    // window"; the next refresh will show the completed state. Worth
    // documenting; not worth a transaction on this low-traffic operator path.
    let existing = state.maintenance_store.get(org, id).await?.ok_or_else(|| {
        AppError::not_found(codes::MAINTENANCE_NOT_FOUND, "maintenance window not found")
    })?;
    if existing.ends_at <= Utc::now() {
        return Err(AppError::unprocessable(
            codes::MAINTENANCE_COMPLETED,
            "cannot edit a completed maintenance window",
        ));
    }
    if let Some(t) = update.title.as_deref() {
        validation::validate_title(t, "title")?;
    }
    validation::validate_description(update.description.as_deref(), "description")?;
    let starts = update.starts_at.unwrap_or(existing.starts_at);
    let ends = update.ends_at.unwrap_or(existing.ends_at);
    validate_time_range(starts, ends)?;
    if let Some(ids) = update.component_ids.as_deref() {
        validate_component_ids(state.maintenance_store.as_ref(), org, ids).await?;
    }
    match state
        .maintenance_store
        .update(org, id, update, source)
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
    description = "Deletion is permitted at any time. For audit, consider PATCHing the title \
                   to indicate cancellation instead of hard-deleting historical windows.",
    params(("id" = Uuid, Path)),
    responses(
        (status = 204, description = "Deleted"),
        (status = 404, body = ApiError),
    ),
)]
pub async fn delete_maintenance(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<MaintenanceDelete>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode> {
    if state.maintenance_store.delete(org, id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::not_found(
            codes::MAINTENANCE_NOT_FOUND,
            "maintenance window not found",
        ))
    }
}

// ── Validation ──────────────────────────────────────────────────────────

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
