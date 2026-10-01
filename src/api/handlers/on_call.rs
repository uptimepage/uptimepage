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
use chrono::Datelike;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::app::AppState;
use crate::domain::preferences::validate_timezone;
use crate::domain::{
    FIRST_YEAR, LAST_YEAR, NewOnCallOverride, NewOnCallSchedule, OnCallOverride,
    OnCallScheduleDetail, OnCallScheduleSummary, OnCallWindow, RotationType, Shadowed, UserId,
    never_pages,
};
use crate::error::ApiError;
use crate::error::codes;
use crate::error::{AppError, Result};
use crate::request::{Authorized, CurrentUser, OnCallRead, OnCallWrite, OwnerAuthorized};
use crate::storage::on_call_feeds;

const MAX_NAME: usize = 100;
const MAX_LAYERS: usize = 10;
const MAX_PARTICIPANTS: usize = 50;
const MAX_WINDOWS: usize = 14;
const HOUR_SECS: i32 = 3_600;
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
    let mut orders = std::collections::HashSet::new();
    if !new.layers.iter().all(|l| orders.insert(l.layer_order)) {
        return Err(invalid("each layer needs its own layer_order"));
    }
    for layer in &new.layers {
        if layer.windows.len() > MAX_WINDOWS {
            return Err(invalid("too many windows in one layer"));
        }
        for w in &layer.windows {
            let mut seen = std::collections::HashSet::new();
            if w.days.is_empty() || !w.days.iter().all(|d| seen.insert(*d)) {
                return Err(invalid(
                    "each window needs one or more days, each named once",
                ));
            }
        }
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
            RotationType::Custom if layer.rotation_length_secs < HOUR_SECS => {
                return Err(invalid("a custom rotation length must be at least an hour"));
            }
            _ => {}
        }
        if !(FIRST_YEAR..=LAST_YEAR).contains(&layer.handoff_at.year()) {
            return Err(invalid(format!(
                "the first handoff must fall within years {FIRST_YEAR} to {LAST_YEAR}"
            )));
        }
        if layer.participants.is_empty() {
            return Err(invalid("each layer needs at least one participant"));
        }
        if layer.participants.len() > MAX_PARTICIPANTS {
            return Err(invalid("too many participants in one layer"));
        }
    }
    // Layers are numbered as sent, which is how the builder lists them.
    let mut asked: Vec<usize> = (0..new.layers.len()).collect();
    asked.sort_by_key(|&i| new.layers[i].layer_order);
    let stack: Vec<&[OnCallWindow]> = asked
        .iter()
        .map(|&i| new.layers[i].windows.as_slice())
        .collect();
    if let Some(Shadowed {
        layer,
        behind,
        whole_week,
    }) = never_pages(&stack)
    {
        let layer = asked[layer] + 1;
        return Err(invalid(match (behind, whole_week) {
            (Some(before), _) => {
                let before = asked[before] + 1;
                format!(
                    "layer {layer} would never page: layer {before} is on call at all hours; \
                     add hours to layer {before}, or remove layer {layer}"
                )
            }
            (None, true) => format!(
                "layer {layer} would never page: the layers before it are on call at all \
                 hours; remove it"
            ),
            (None, false) => format!(
                "layer {layer} would never page: the layers before it are on call at all its \
                 hours; change its hours, or remove it"
            ),
        }));
    }
    Ok(())
}

/// The window to store. One already over would change no page, and could
/// only be removed by id, so it must still reach past `now`; one already
/// begun starts at `now`, since who was paged before it stays as it was.
fn override_from(
    new: NewOnCallOverride,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<NewOnCallOverride> {
    if new.ends_at <= new.starts_at {
        return Err(invalid("override end must be after its start"));
    }
    if new.ends_at <= now {
        return Err(invalid("override must end in the future"));
    }
    if new.ends_at.year() > LAST_YEAR {
        return Err(invalid(format!(
            "override must end by the year {LAST_YEAR}"
        )));
    }
    Ok(NewOnCallOverride {
        starts_at: new.starts_at.max(now),
        ..new
    })
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
    let new = override_from(new, chrono::Utc::now())?;
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
    description = "One not yet begun is deleted; one that has begun ends now instead, so who was on call before stays as it was.",
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
        .remove_override(org, id, override_id, chrono::Utc::now())
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
    responses((status = 200, body = WhoResponse), (status = 400, body = ApiError), (status = 404, body = ApiError)),
)]
pub async fn who(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<OnCallRead>,
    Query(q): Query<WhoQuery>,
) -> Result<Json<WhoResponse>> {
    let at = q.at.unwrap_or_else(chrono::Utc::now);
    if !(FIRST_YEAR..=LAST_YEAR).contains(&at.year()) {
        return Err(AppError::bad_request_field(
            codes::BAD_TIME_RANGE,
            format!("at must fall within years {FIRST_YEAR} to {LAST_YEAR}"),
            "at",
        ));
    }
    let user_ids = state
        .on_call_store
        .resolve_now(org, q.schedule_id, at)
        .await?
        .ok_or_else(not_found)?;
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

/// The link a calendar app subscribes to for your own on-call shifts.
#[derive(Debug, Serialize, ToSchema)]
pub struct CalendarFeed {
    pub url: String,
}

#[utoipa::path(
    post, path = "/api/v1/on-call/my-calendar", tag = "on-call",
    summary = "Make a new calendar link for your on-call shifts",
    description = "An iCalendar feed of your shifts from the last 30 days and the next 90, read from the schedules as they stand now, which a calendar app fetches without signing in. The link made before this one stops working.",
    responses((status = 200, body = CalendarFeed)),
)]
pub async fn reset_my_calendar(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<OnCallWrite>,
    CurrentUser(user): CurrentUser,
) -> Result<Json<CalendarFeed>> {
    let token =
        on_call_feeds::reset(state.require_db()?, state.cipher.as_deref(), org, user).await?;
    Ok(Json(CalendarFeed {
        url: on_call_feeds::feed_url(&state.cfg.auth.public_base_url, &token),
    }))
}

/// Refuses new on-call coverage on a plan that does not sell it.
///
/// Write-time only, following the text-message gate: an org that already has
/// policies or schedules keeps them working and keeps them editable, so a
/// rota can still be corrected and a departing engineer removed. Silencing
/// live paging on a plan change would be a far worse failure than carrying a
/// feature the account has stopped paying for.
pub(crate) fn gate_on_call(state: &AppState, plan: &crate::domain::Plan) -> Result<()> {
    if !state.cfg.on_call_available(plan) {
        return Err(AppError::forbidden_code(
            crate::error::codes::ON_CALL_DISABLED,
            "on-call and escalation are not available on your plan",
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
            windows: vec![],
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
    fn rejects_custom_length_under_an_hour() {
        assert!(validate(&schedule("UTC", vec![layer(RotationType::Custom, 1800)])).is_err());
        assert!(
            validate(&schedule(
                "UTC",
                vec![layer(RotationType::Custom, HOUR_SECS)]
            ))
            .is_ok()
        );
    }

    #[test]
    fn rejects_a_handoff_outside_the_years() {
        let mut l = layer(RotationType::Daily, DAY_SECS);
        l.handoff_at = "+10000-01-01T00:00:00Z".parse().unwrap();
        assert!(validate(&schedule("UTC", vec![l])).is_err());
    }

    #[test]
    fn rejects_layers_sharing_an_order() {
        let first = in_order(0, vec![weekdays()]);
        assert!(validate(&schedule("UTC", vec![first.clone(), first.clone()])).is_err());
        assert!(validate(&schedule("UTC", vec![first, in_order(1, vec![])])).is_ok());
    }

    fn in_order(n: i32, windows: Vec<OnCallWindow>) -> NewOnCallLayer {
        NewOnCallLayer {
            layer_order: n,
            windows,
            ..layer(RotationType::Daily, DAY_SECS)
        }
    }

    fn weekdays() -> OnCallWindow {
        serde_json::from_str(
            r#"{"days":["mon","tue","wed","thu","fri"],"from":"09:00","to":"17:00"}"#,
        )
        .unwrap()
    }

    #[test]
    fn a_layer_that_would_never_page_is_refused() {
        let err = validate(&schedule(
            "UTC",
            vec![in_order(1, vec![weekdays()]), in_order(0, vec![])],
        ))
        .unwrap_err();
        assert!(
            err.to_string().contains(
                "layer 1 would never page: layer 2 is on call at all hours; \
                 add hours to layer 2, or remove layer 1"
            ),
            "{err}"
        );
        assert!(
            validate(&schedule(
                "UTC",
                vec![in_order(0, vec![weekdays()]), in_order(1, vec![])],
            ))
            .is_ok()
        );
        assert!(
            validate(&schedule(
                "UTC",
                vec![in_order(0, vec![weekdays()]), in_order(1, vec![weekdays()])],
            ))
            .is_err()
        );
    }

    #[test]
    fn a_window_names_each_of_its_days_once() {
        let mut none = weekdays();
        none.days.clear();
        assert!(validate(&schedule("UTC", vec![in_order(0, vec![none])])).is_err());
        let mut twice = weekdays();
        twice.days.push(twice.days[0]);
        assert!(validate(&schedule("UTC", vec![in_order(0, vec![twice])])).is_err());
        assert!(
            validate(&schedule(
                "UTC",
                vec![in_order(0, vec![weekdays(); MAX_WINDOWS + 1])]
            ))
            .is_err()
        );
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
        assert!(override_from(bad, "2026-05-01T00:00:00Z".parse().unwrap()).is_err());
    }

    #[test]
    fn rejects_an_override_already_over() {
        let window = NewOnCallOverride {
            user_id: UserId(Uuid::now_v7()),
            starts_at: "2026-06-01T00:00:00Z".parse().unwrap(),
            ends_at: "2026-06-02T00:00:00Z".parse().unwrap(),
        };
        assert!(override_from(window, "2026-06-02T00:00:00Z".parse().unwrap()).is_err());
    }

    #[test]
    fn an_override_already_begun_starts_now() {
        let window = || NewOnCallOverride {
            user_id: UserId(Uuid::now_v7()),
            starts_at: "2026-06-01T00:00:00Z".parse().unwrap(),
            ends_at: "2026-06-02T00:00:00Z".parse().unwrap(),
        };
        let now: chrono::DateTime<chrono::Utc> = "2026-06-01T12:00:00Z".parse().unwrap();
        assert_eq!(override_from(window(), now).unwrap().starts_at, now);
        let early: chrono::DateTime<chrono::Utc> = "2026-05-31T12:00:00Z".parse().unwrap();
        assert_eq!(
            override_from(window(), early).unwrap().starts_at,
            window().starts_at
        );
    }
}
