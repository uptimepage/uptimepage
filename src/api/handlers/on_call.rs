//! On-call schedule endpoints + the per-member contact channels a `user`/
//! `schedule` escalation target pages through.
//!
//! Schedule reads require `oncall:read`; schedule mutations are owner-only
//! (`OwnerAuthorized<OnCallWrite>`) because a schedule is org configuration. A
//! member manages their own contact channels (`oncall:read`/`oncall:write` on
//! the acting session). Who-is-on-call is computed by the store's resolver.

use crate::api::json::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::api::ApiError;
use crate::api::error::codes;
use crate::app::AppState;
use crate::domain::preferences::validate_timezone;
use crate::domain::{
    NewOnCallOverride, NewOnCallSchedule, OnCallOverride, OnCallScheduleDetail,
    OnCallScheduleSummary, RotationType, UserId,
};
use crate::error::{AppError, Result};
use crate::web::{Authorized, CurrentUser, OnCallRead, OnCallWrite, OwnerAuthorized};

const MAX_NAME: usize = 100;
const MAX_LAYERS: usize = 10;
const MAX_PARTICIPANTS: usize = 50;
const DAY_SECS: i32 = 86_400;
const WEEK_SECS: i32 = 7 * DAY_SECS;

fn invalid(msg: impl Into<String>) -> AppError {
    AppError::unprocessable(codes::ON_CALL_SCHEDULE_INVALID, msg)
}

fn not_found() -> AppError {
    AppError::not_found(
        codes::ON_CALL_SCHEDULE_NOT_FOUND,
        "on-call schedule not found",
    )
}

/// Validate schedule metadata + the layer stack. Participant membership is
/// enforced in the store on write (closes the IDOR regardless of caller).
fn validate(new: &NewOnCallSchedule) -> Result<()> {
    let name = new.name.trim();
    if name.is_empty() {
        return Err(invalid("schedule name is required"));
    }
    if name.len() > MAX_NAME {
        return Err(invalid("schedule name is too long"));
    }
    validate_timezone(&new.timezone).map_err(|_| invalid("timezone must be a known IANA name"))?;
    if new.layers.len() > MAX_LAYERS {
        return Err(invalid("too many layers"));
    }
    for layer in &new.layers {
        if layer.rotation_length_secs <= 0 {
            return Err(invalid("rotation length must be positive"));
        }
        match layer.rotation_type {
            RotationType::Daily if layer.rotation_length_secs % DAY_SECS != 0 => {
                return Err(invalid(
                    "a daily rotation length must be a whole number of days",
                ));
            }
            RotationType::Weekly if layer.rotation_length_secs % WEEK_SECS != 0 => {
                return Err(invalid(
                    "a weekly rotation length must be a whole number of weeks",
                ));
            }
            _ => {}
        }
        if layer.participants.is_empty() {
            return Err(invalid("each layer needs at least one participant"));
        }
        if layer.participants.len() > MAX_PARTICIPANTS {
            return Err(invalid("too many participants in one layer"));
        }
    }
    Ok(())
}

fn validate_override(new: &NewOnCallOverride) -> Result<()> {
    if new.ends_at <= new.starts_at {
        return Err(invalid("override end must be after its start"));
    }
    Ok(())
}

#[utoipa::path(
    get, path = "/api/v1/on-call/schedules", tag = "on-call",
    summary = "List on-call schedules",
    responses((status = 200, body = [OnCallScheduleSummary])),
)]
pub async fn list(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<OnCallRead>,
) -> Result<Json<Vec<OnCallScheduleSummary>>> {
    Ok(Json(state.on_call_store.list(org).await?))
}

#[utoipa::path(
    post, path = "/api/v1/on-call/schedules", tag = "on-call",
    summary = "Create an on-call schedule",
    request_body = NewOnCallSchedule,
    responses((status = 201, body = OnCallScheduleDetail), (status = 422, body = ApiError)),
)]
pub async fn create(
    State(state): State<AppState>,
    OwnerAuthorized(org, _): OwnerAuthorized<OnCallWrite>,
    Json(new): Json<NewOnCallSchedule>,
) -> Result<(StatusCode, Json<OnCallScheduleDetail>)> {
    validate(&new)?;
    let plan = state.quotas.limit_for_org(org).await?;
    gate_on_call(&state, &plan)?;
    state
        .quotas
        .check_can_create_on_call_schedule(org, None)
        .await?;
    let limit = i64::from(plan.max_on_call_schedules);
    let detail = state.on_call_store.create(org, new, limit).await?;
    Ok((StatusCode::CREATED, Json(detail)))
}

#[utoipa::path(
    get, path = "/api/v1/on-call/schedules/{id}", tag = "on-call",
    summary = "Get an on-call schedule with its layers and overrides",
    params(("id" = Uuid, Path)),
    responses((status = 200, body = OnCallScheduleDetail), (status = 404, body = ApiError)),
)]
pub async fn get(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<OnCallRead>,
    Path(id): Path<Uuid>,
) -> Result<Json<OnCallScheduleDetail>> {
    state
        .on_call_store
        .get(org, id)
        .await?
        .map(Json)
        .ok_or_else(not_found)
}

#[utoipa::path(
    patch, path = "/api/v1/on-call/schedules/{id}", tag = "on-call",
    summary = "Replace an on-call schedule's metadata and layers",
    params(("id" = Uuid, Path)), request_body = NewOnCallSchedule,
    responses((status = 200, body = OnCallScheduleDetail), (status = 404, body = ApiError), (status = 422, body = ApiError)),
)]
pub async fn replace(
    State(state): State<AppState>,
    OwnerAuthorized(org, _): OwnerAuthorized<OnCallWrite>,
    Path(id): Path<Uuid>,
    Json(new): Json<NewOnCallSchedule>,
) -> Result<Json<OnCallScheduleDetail>> {
    validate(&new)?;
    state
        .on_call_store
        .replace(org, id, new)
        .await?
        .map(Json)
        .ok_or_else(not_found)
}

#[utoipa::path(
    delete, path = "/api/v1/on-call/schedules/{id}", tag = "on-call",
    summary = "Delete an on-call schedule",
    params(("id" = Uuid, Path)),
    responses((status = 204), (status = 404, body = ApiError)),
)]
pub async fn delete(
    State(state): State<AppState>,
    OwnerAuthorized(org, _): OwnerAuthorized<OnCallWrite>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode> {
    if state.on_call_store.delete(org, id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(not_found())
    }
}

#[utoipa::path(
    post, path = "/api/v1/on-call/schedules/{id}/overrides", tag = "on-call",
    summary = "Add a coverage override",
    params(("id" = Uuid, Path)), request_body = NewOnCallOverride,
    responses((status = 201, body = OnCallOverride), (status = 404, body = ApiError), (status = 422, body = ApiError)),
)]
pub async fn add_override(
    State(state): State<AppState>,
    OwnerAuthorized(org, _): OwnerAuthorized<OnCallWrite>,
    CurrentUser(actor): CurrentUser,
    Path(id): Path<Uuid>,
    Json(new): Json<NewOnCallOverride>,
) -> Result<(StatusCode, Json<OnCallOverride>)> {
    validate_override(&new)?;
    let plan = state.quotas.limit_for_org(org).await?;
    gate_on_call(&state, &plan)?;
    state
        .on_call_store
        .add_override(org, id, Some(actor), new)
        .await?
        .map(|o| (StatusCode::CREATED, Json(o)))
        .ok_or_else(not_found)
}

#[utoipa::path(
    delete, path = "/api/v1/on-call/schedules/{id}/overrides/{override_id}", tag = "on-call",
    summary = "Remove a coverage override",
    params(("id" = Uuid, Path), ("override_id" = Uuid, Path)),
    responses((status = 204), (status = 404, body = ApiError)),
)]
pub async fn delete_override(
    State(state): State<AppState>,
    OwnerAuthorized(org, _): OwnerAuthorized<OnCallWrite>,
    Path((id, override_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode> {
    if state
        .on_call_store
        .delete_override(org, id, override_id)
        .await?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::not_found(
            codes::ON_CALL_OVERRIDE_NOT_FOUND,
            "override not found",
        ))
    }
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct WhoQuery {
    schedule_id: Uuid,
    /// Resolve as of this instant; defaults to now.
    #[serde(default)]
    at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Who is on call for a schedule at an instant.
#[derive(Debug, Serialize, ToSchema)]
pub struct WhoResponse {
    pub schedule_id: Uuid,
    pub at: chrono::DateTime<chrono::Utc>,
    #[schema(value_type = Vec<String>)]
    pub user_ids: Vec<UserId>,
}

#[utoipa::path(
    get, path = "/api/v1/on-call/who", tag = "on-call",
    summary = "Resolve who is on call for a schedule",
    params(WhoQuery),
    responses((status = 200, body = WhoResponse), (status = 404, body = ApiError)),
)]
pub async fn who(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<OnCallRead>,
    Query(q): Query<WhoQuery>,
) -> Result<Json<WhoResponse>> {
    if state.on_call_store.get(org, q.schedule_id).await?.is_none() {
        return Err(not_found());
    }
    let at = q.at.unwrap_or_else(chrono::Utc::now);
    let user_ids = state
        .on_call_store
        .resolve_now(org, q.schedule_id, at)
        .await?;
    Ok(Json(WhoResponse {
        schedule_id: q.schedule_id,
        at,
        user_ids,
    }))
}

// ── Per-member contact channels ──────────────────────────────────────────

/// The channels that page the acting member.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ContactChannels {
    pub channel_ids: Vec<Uuid>,
}

#[utoipa::path(
    get, path = "/api/v1/on-call/my-contacts", tag = "on-call",
    summary = "Get the channels that page you",
    responses((status = 200, body = ContactChannels)),
)]
pub async fn get_my_contacts(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<OnCallRead>,
    CurrentUser(user): CurrentUser,
) -> Result<Json<ContactChannels>> {
    let channel_ids = state.contact_store.for_user(org, user).await?;
    Ok(Json(ContactChannels { channel_ids }))
}

#[utoipa::path(
    put, path = "/api/v1/on-call/my-contacts", tag = "on-call",
    summary = "Replace the channels that page you",
    request_body = ContactChannels,
    responses((status = 200, body = ContactChannels), (status = 422, body = ApiError)),
)]
pub async fn set_my_contacts(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<OnCallWrite>,
    CurrentUser(user): CurrentUser,
    Json(body): Json<ContactChannels>,
) -> Result<Json<ContactChannels>> {
    // These rows are what a rung resolves a person to, so wiring up a new one
    // is new paging coverage even though no schedule is touched. Taking one
    // away is not, and must stay open: an org that has lost the tier still has
    // to be able to stop paging someone who has left.
    //
    // This is a whole-set replace, so the question is which channels the set
    // *gains*, not whether it is empty. Testing emptiness would refuse
    // [A, B] -> [A], which is a removal and the very case the tier must not
    // block. Same shape as the text-message gate's `already_sms`.
    let current = state.contact_store.for_user(org, user).await?;
    if body.channel_ids.iter().any(|id| !current.contains(id)) {
        let plan = state.quotas.limit_for_org(org).await?;
        gate_on_call(&state, &plan)?;
    }
    state
        .contact_store
        .replace_for_user(org, user, body.channel_ids.clone())
        .await?;
    Ok(Json(body))
}

/// Refuses new on-call coverage on a plan that does not sell it.
///
/// Write-time only, following the text-message gate: an org that already has
/// policies or schedules keeps them working and keeps them editable, so a
/// rota can still be corrected and a departing engineer removed. Silencing
/// live paging on a plan change would be a far worse failure than carrying a
/// feature the account has stopped paying for.
///
/// Self-host is exempt for the same reason it is exempt from the SMS gate: the
/// operator owns the `plans` row, so gating them against it means nothing.
pub(crate) fn gate_on_call(state: &AppState, plan: &crate::domain::Plan) -> Result<()> {
    if state.cfg.marketing.enabled && !plan.on_call_enabled {
        return Err(AppError::forbidden_code(
            crate::api::error::codes::ON_CALL_DISABLED,
            "on-call scheduling and escalation are not available on your plan",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{NewOnCallLayer, NewOnCallParticipant};

    fn layer(rotation: RotationType, len: i32) -> NewOnCallLayer {
        NewOnCallLayer {
            name: None,
            rotation_type: rotation,
            rotation_length_secs: len,
            handoff_at: "2026-06-01T00:00:00Z".parse().unwrap(),
            layer_order: 0,
            participants: vec![NewOnCallParticipant {
                user_id: UserId(Uuid::now_v7()),
            }],
        }
    }

    fn schedule(tz: &str, layers: Vec<NewOnCallLayer>) -> NewOnCallSchedule {
        NewOnCallSchedule {
            name: "primary".into(),
            timezone: tz.into(),
            layers,
        }
    }

    #[test]
    fn accepts_a_valid_schedule() {
        assert!(validate(&schedule("UTC", vec![layer(RotationType::Daily, DAY_SECS)])).is_ok());
        assert!(
            validate(&schedule(
                "America/New_York",
                vec![layer(RotationType::Custom, 3600)]
            ))
            .is_ok()
        );
    }

    #[test]
    fn rejects_unknown_timezone() {
        assert!(
            validate(&schedule(
                "Mars/Phobos",
                vec![layer(RotationType::Daily, DAY_SECS)]
            ))
            .is_err()
        );
    }

    #[test]
    fn rejects_daily_length_that_is_not_whole_days() {
        assert!(validate(&schedule("UTC", vec![layer(RotationType::Daily, 3600)])).is_err());
    }

    #[test]
    fn rejects_layer_with_no_participants() {
        let mut l = layer(RotationType::Daily, DAY_SECS);
        l.participants.clear();
        assert!(validate(&schedule("UTC", vec![l])).is_err());
    }

    #[test]
    fn rejects_inverted_override_window() {
        let bad = NewOnCallOverride {
            user_id: UserId(Uuid::now_v7()),
            starts_at: "2026-06-02T00:00:00Z".parse().unwrap(),
            ends_at: "2026-06-01T00:00:00Z".parse().unwrap(),
        };
        assert!(validate_override(&bad).is_err());
    }
}
