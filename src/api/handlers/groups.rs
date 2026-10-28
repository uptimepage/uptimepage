//! Monitor groups: the record a monitor's `group_id` points at, and the switch
//! that makes the group alert as one. Monitors scopes cover groups.

use crate::request::json::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::AppendHeaders;
use chrono::{DateTime, Utc};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::app::AppState;
use crate::domain::{
    MonitorGroup, MonitorGroupUpdate, NewMonitorGroup, OrgId, TargetAlerts, WriteSource,
};
use crate::error::ApiError;
use crate::error::codes;
use crate::error::{AppError, Result};
use crate::request::{
    Authorized, CurrentUser, RequestSource, TargetsDelete, TargetsRead, TargetsWrite,
};
use crate::storage::GroupUsage;

#[derive(Debug, Serialize, ToSchema)]
pub struct GroupView {
    pub id: Uuid,
    pub name: String,
    /// On: a member's failure opens one group incident that pages the group's
    /// channels and policy, and the members' own stay quiet. Off: a label.
    pub alert_as_one: bool,
    #[schema(value_type = Vec<crate::domain::alert::AlertBindingInput>)]
    pub alerts: TargetAlerts,
    /// Seconds before the first reminder while the group incident stays
    /// unacknowledged. 0 = off.
    pub renotify_interval_secs: u32,
    /// Whether another member going down while the group incident is open
    /// sends a message (at most one a minute) or only a timeline line.
    pub alert_again: bool,
    /// Shown on the members while the group alerts as one, and what the
    /// group's tag-matched channels go by.
    pub tags: Vec<String>,
    pub write_source: WriteSource,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub member_count: u32,
    /// The group incident open now.
    #[schema(nullable = true)]
    pub open_incident_id: Option<Uuid>,
}

impl GroupView {
    pub fn new(group: MonitorGroup, usage: GroupUsage) -> Self {
        Self {
            id: group.id,
            name: group.name,
            alert_as_one: group.alert_as_one,
            alerts: group.alerts,
            renotify_interval_secs: group.renotify_interval_secs,
            alert_again: group.alert_again,
            tags: group.tags,
            write_source: group.write_source,
            created_at: group.created_at,
            updated_at: group.updated_at,
            member_count: usage.member_count,
            open_incident_id: usage.open_incident.map(|i| i.id),
        }
    }
}

pub(crate) fn not_found() -> AppError {
    AppError::not_found(codes::GROUP_NOT_FOUND, "group not found")
}

async fn view(state: &AppState, org: OrgId, group: MonitorGroup) -> Result<GroupView> {
    let usage = state
        .monitor_group_store
        .usage(org, &[group.id])
        .await?
        .remove(&group.id)
        .unwrap_or_default();
    Ok(GroupView::new(group, usage))
}

#[utoipa::path(
    get, path = "/api/v1/groups", tag = "groups",
    summary = "List monitor groups, empty ones included",
    responses((status = 200, body = [GroupView])),
)]
pub async fn list(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<TargetsRead>,
) -> Result<Json<Vec<GroupView>>> {
    let groups = state.monitor_group_store.list(org).await?;
    let ids: Vec<Uuid> = groups.iter().map(|g| g.id).collect();
    let mut usage = state.monitor_group_store.usage(org, &ids).await?;
    Ok(Json(
        groups
            .into_iter()
            .map(|g| {
                let u = usage.remove(&g.id).unwrap_or_default();
                GroupView::new(g, u)
            })
            .collect(),
    ))
}

#[utoipa::path(
    get, path = "/api/v1/groups/{id}", tag = "groups",
    summary = "Get a monitor group",
    params(("id" = Uuid, Path)),
    responses((status = 200, body = GroupView), (status = 404, body = ApiError)),
)]
pub async fn get(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<TargetsRead>,
    Path(id): Path<Uuid>,
) -> Result<Json<GroupView>> {
    let group = state
        .monitor_group_store
        .get(org, id)
        .await?
        .ok_or_else(not_found)?;
    Ok(Json(view(&state, org, group).await?))
}

#[utoipa::path(
    post, path = "/api/v1/groups", tag = "groups",
    summary = "Create a monitor group",
    request_body = NewMonitorGroup,
    responses(
        (status = 201, body = GroupView,
            headers(("Location" = String, description = "URL of the new group"))),
        (status = 400, body = ApiError, description = "Blank or long name, bad tags, reminder or channels"),
        (status = 409, body = ApiError, description = "GROUP_NAME_TAKEN"),
        (status = 422, body = ApiError, description = "QUOTA_EXCEEDED: as many groups as the plan allows monitors"),
    ),
)]
pub async fn create(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<TargetsWrite>,
    CurrentUser(user): CurrentUser,
    RequestSource(source): RequestSource,
    Json(mut new): Json<NewMonitorGroup>,
) -> Result<(
    StatusCode,
    AppendHeaders<[(axum::http::HeaderName, HeaderValue); 1]>,
    Json<GroupView>,
)> {
    state.target_ops().vet_new_group(org, &mut new).await?;
    state
        .quotas
        .check_can_create_groups(org, Some(user), 1)
        .await?;
    let plan = state.quotas.limit_for_org(org).await?;
    let group = state
        .monitor_group_store
        .create(org, new, source, Some(user), i64::from(plan.max_targets))
        .await?;
    let location = HeaderValue::from_str(&format!("/api/v1/groups/{}", group.id))
        .expect("uuid produces ascii-only path");
    Ok((
        StatusCode::CREATED,
        AppendHeaders([(header::LOCATION, location)]),
        Json(GroupView::new(group, GroupUsage::default())),
    ))
}

#[utoipa::path(
    patch, path = "/api/v1/groups/{id}", tag = "groups",
    summary = "Change a monitor group",
    description = "Omit fields you don't want to change. Turning `alert_as_one` on or off changes only what happens to failures that start after it: an incident open now ends the way it started.",
    params(("id" = Uuid, Path)),
    request_body = MonitorGroupUpdate,
    responses(
        (status = 200, body = GroupView),
        (status = 400, body = ApiError),
        (status = 404, body = ApiError),
        (status = 409, body = ApiError, description = "GROUP_NAME_TAKEN"),
    ),
)]
pub async fn update(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<TargetsWrite>,
    CurrentUser(user): CurrentUser,
    RequestSource(source): RequestSource,
    Path(id): Path<Uuid>,
    Json(mut update): Json<MonitorGroupUpdate>,
) -> Result<Json<GroupView>> {
    state
        .target_ops()
        .vet_group_update(org, &mut update)
        .await?;
    let group = state
        .monitor_group_store
        .update(org, id, update, source, Some(user))
        .await?
        .ok_or_else(not_found)?;
    Ok(Json(view(&state, org, group).await?))
}

#[utoipa::path(
    delete, path = "/api/v1/groups/{id}", tag = "groups",
    summary = "Delete a monitor group",
    description = "Its monitors stay, ungrouped. An open group incident closes with a note, not a recovery, and each member still down alerts on its own channels from then on.",
    params(("id" = Uuid, Path)),
    responses((status = 204, description = "Deleted"), (status = 404, body = ApiError)),
)]
pub async fn delete(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<TargetsDelete>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode> {
    state
        .monitor_group_store
        .delete(org, id, Some(user))
        .await?
        .ok_or_else(not_found)?;
    Ok(StatusCode::NO_CONTENT)
}
