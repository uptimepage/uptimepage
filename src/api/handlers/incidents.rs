//! Operator endpoints for incident narration and timeline updates.
//!
//! Both routes read/write the materialised `incidents` + `incident_updates`
//! tables via `IncidentNarrationStore` — disjoint from the background writer
//! (`public_status::incident_writer`) which only opens/closes incidents based
//! on check results.

use crate::api::json::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::AppendHeaders;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::api::ApiError;
use crate::api::error::codes;
use crate::api::handlers::validation::{self, validate_message};
use crate::app::AppState;
use crate::domain::{
    Incident, IncidentEvent, IncidentEventKind, IncidentMetrics, IncidentNarrationUpdate,
    IncidentOrigin, IncidentPostmortem, IncidentState, IncidentVisibility, NewIncidentUpdate,
    NewManualIncident, NotificationReason, OpsIncident, PostmortemUpsert, PublicIncidentUpdate,
    UserId,
};
use crate::error::{AppError, Result};
use crate::storage::{Actor, IncidentOpsFilter, LifecycleOutcome};
use crate::web::{Authorized, CurrentUser, IncidentsRead, IncidentsWrite};

#[utoipa::path(
    patch,
    path = "/api/v1/incidents/{id}",
    tag = "incidents",
    summary = "Amend an incident (title, severity, urgency, public narration)",
    description = "Sending JSON `null` for `title`, `public_title` or `public_description` clears \
                   the stored value; omitting the field leaves it unchanged. The public page falls \
                   back to auto-generated content when the public title is null. A severity change \
                   is recorded on the incident's internal timeline. `counts_as_downtime` decides \
                   whether the incident's duration reaches the monitor's uptime figure, and is \
                   editable only on a declared incident.",
    params(("id" = Uuid, Path)),
    request_body(content = IncidentNarrationUpdate, example = json!({
        "title": "Checkout failing for EU customers",
        "public_title": "Latency spike on EU API",
        "public_description": "Investigation in progress.",
        "severity": "major",
        "urgency": "low",
        "counts_as_downtime": true
    })),
    responses(
        (status = 200, body = Incident),
        (status = 400, body = ApiError),
        (status = 404, body = ApiError),
        (status = 422, body = ApiError),
    ),
)]
pub async fn update_incident_narration(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<IncidentsWrite>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
    Json(update): Json<IncidentNarrationUpdate>,
) -> Result<Json<Incident>> {
    validation::validate_optional_title(update.title.as_ref(), "title")?;
    validation::validate_optional_title(update.public_title.as_ref(), "public_title")?;
    validation::validate_optional_description(
        update.public_description.as_ref(),
        "public_description",
    )?;
    // Read before the write so the timeline can say what it changed from.
    let was = if update.severity.is_some() || update.counts_as_downtime.is_some() {
        state.incident_ops_store.get(org, id).await?
    } else {
        None
    };
    if update.counts_as_downtime.is_some()
        && let Some(before) = was.as_ref()
        && before.origin != IncidentOrigin::Manual
    {
        return Err(AppError::unprocessable(
            codes::INCIDENT_DOWNTIME_NOT_EDITABLE,
            "only a declared incident can be excluded from uptime; this one was opened by \
             its monitor and the check history behind it stands",
        ));
    }
    let patched = state
        .incident_narration_store
        .patch_narration(org, id, update)
        .await?
        .ok_or_else(|| AppError::not_found(codes::INCIDENT_NOT_FOUND, "incident not found"))?;
    // Already committed: failing here would report a save that happened as a
    // failure, and the retry would find nothing to log.
    if let Some(before) = was.as_ref()
        && before.severity != patched.severity
    {
        log_amendment(
            &state,
            org,
            id,
            user,
            IncidentEventKind::SeverityChanged,
            format!(
                "severity {} to {}",
                before.severity.as_db_str(),
                patched.severity.as_db_str()
            ),
        )
        .await;
    }
    if let Some(before) = was.as_ref()
        && before.counts_as_downtime != patched.counts_as_downtime
    {
        log_amendment(
            &state,
            org,
            id,
            user,
            IncidentEventKind::DowntimeChanged,
            match patched.counts_as_downtime {
                true => "now counts toward uptime".to_string(),
                false => "no longer counts toward uptime".to_string(),
            },
        )
        .await;
    }
    Ok(Json(patched))
}

/// Amendments that move a published number need provenance, but the row is
/// already written, so a lost timeline entry must not fail the request.
async fn log_amendment(
    state: &AppState,
    org: crate::domain::OrgId,
    id: Uuid,
    user: UserId,
    kind: IncidentEventKind,
    message: String,
) {
    if let Err(err) = state
        .incident_ops_store
        .append_event(org, id, kind, Actor::User(user), Some(message))
        .await
    {
        tracing::warn!(
            %org, incident_id = %id, kind = kind.as_db_str(), error = %err,
            "incident amended but its timeline entry was lost"
        );
    }
}

#[utoipa::path(
    post,
    path = "/api/v1/incidents/{id}/updates",
    tag = "incidents",
    summary = "Post a status update to an incident timeline",
    description = "Appends an operator-authored entry to the incident's public update timeline. \
                   Setting `phase=resolved` does NOT end the incident automatically — that's \
                   driven by check-result transitions. Posting an update on an already-ended \
                   incident is allowed (useful for postmortems).",
    params(("id" = Uuid, Path)),
    request_body(content = NewIncidentUpdate, example = json!({
        "phase": "identified",
        "message": "Rolled back the offending deploy. Verifying recovery."
    })),
    responses(
        (status = 201, body = PublicIncidentUpdate,
            headers(("Location" = String))),
        (status = 400, body = ApiError),
        (status = 404, body = ApiError),
    ),
)]
pub async fn post_incident_update(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<IncidentsWrite>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
    Json(new): Json<NewIncidentUpdate>,
) -> Result<(
    StatusCode,
    AppendHeaders<[(axum::http::HeaderName, HeaderValue); 1]>,
    Json<PublicIncidentUpdate>,
)> {
    validate_message(&new.message, "message")?;
    let entry = state
        .incident_narration_store
        .append_update(org, id, new, Some(user.0.to_string()))
        .await?
        .ok_or_else(|| AppError::not_found(codes::INCIDENT_NOT_FOUND, "incident not found"))?;
    let location = HeaderValue::from_str(&format!("/api/v1/incidents/{id}")).expect("uuid ascii");
    Ok((
        StatusCode::CREATED,
        AppendHeaders([(header::LOCATION, location)]),
        Json(entry),
    ))
}

// ── Operational lifecycle ──────────────────────────────────────────────────

/// Full operational incident plus its internal activity timeline.
#[derive(Debug, Serialize, ToSchema)]
pub struct IncidentDetail {
    pub incident: OpsIncident,
    pub timeline: Vec<IncidentEvent>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ListIncidentsQuery {
    /// Filter by operational state: `triggered`, `acknowledged`, `resolved`.
    pub state: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

/// Optional operator note attached to a lifecycle action.
#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct LifecycleBody {
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct AssignBody {
    /// The responder to own the incident, or `null` to clear the assignee.
    #[serde(default)]
    pub user_id: Option<UserId>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct NoteBody {
    pub message: String,
}

/// Optional public narration seeded when publishing. An omitted field leaves
/// the stored copy unchanged; clearing copy is the narration patch endpoint.
#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct PublishBody {
    #[serde(default)]
    pub public_title: Option<String>,
    #[serde(default)]
    pub public_description: Option<String>,
}

fn parse_state_filter(s: Option<&str>) -> Result<Option<IncidentState>> {
    match s {
        None => Ok(None),
        Some("triggered") => Ok(Some(IncidentState::Triggered)),
        Some("acknowledged") => Ok(Some(IncidentState::Acknowledged)),
        Some("resolved") => Ok(Some(IncidentState::Resolved)),
        Some(other) => Err(AppError::bad_request(
            codes::INCIDENT_INVALID_STATE,
            format!("unknown incident state filter: {other}"),
        )),
    }
}

/// Trim a blank note to `None`, reject one over the message length cap. Caps by
/// character count to match the shared validators (a byte cap spuriously
/// rejected multibyte notes well under the advertised character limit).
fn clean_note(note: Option<String>) -> Result<Option<String>> {
    let Some(n) = note else { return Ok(None) };
    if n.trim().is_empty() {
        return Ok(None);
    }
    validation::check_length(&n, "note", validation::MAX_MESSAGE, codes::MESSAGE_TOO_LONG)?;
    Ok(Some(n))
}

fn lifecycle_response(outcome: LifecycleOutcome) -> Result<Json<OpsIncident>> {
    match outcome {
        LifecycleOutcome::Updated(inc) => Ok(Json(*inc)),
        LifecycleOutcome::NotFound => Err(AppError::not_found(
            codes::INCIDENT_NOT_FOUND,
            "incident not found",
        )),
        LifecycleOutcome::IllegalTransition(err) => Err(AppError::conflict(
            codes::INCIDENT_INVALID_STATE,
            err.to_string(),
        )),
        // Unreachable: only a notification-borne acknowledgement pins an
        // episode. A conflict rather than a panic.
        LifecycleOutcome::Stale => Err(AppError::conflict(
            codes::INCIDENT_INVALID_STATE,
            "this incident has reopened since the action was prepared",
        )),
    }
}

#[utoipa::path(
    get, path = "/api/v1/incidents", tag = "incidents",
    summary = "List operational incidents (newest first)",
    params(
        ("state" = Option<String>, Query, description = "triggered | acknowledged | resolved"),
        ("limit" = Option<usize>, Query),
    ),
    responses((status = 200, body = [OpsIncident]), (status = 400, body = ApiError)),
)]
pub async fn list_incidents(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<IncidentsRead>,
    Query(q): Query<ListIncidentsQuery>,
) -> Result<Json<Vec<OpsIncident>>> {
    let filter = IncidentOpsFilter {
        state: parse_state_filter(q.state.as_deref())?,
        limit: q.limit,
        offset: q.offset.unwrap_or(0),
        ..Default::default()
    };
    Ok(Json(state.incident_ops_store.list(org, filter).await?))
}

#[utoipa::path(
    get, path = "/api/v1/incidents/{id}", tag = "incidents",
    summary = "Get one incident with its internal activity timeline",
    params(("id" = Uuid, Path)),
    responses((status = 200, body = IncidentDetail), (status = 404, body = ApiError)),
)]
pub async fn get_incident(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<IncidentsRead>,
    Path(id): Path<Uuid>,
) -> Result<Json<IncidentDetail>> {
    let incident = state
        .incident_ops_store
        .get(org, id)
        .await?
        .ok_or_else(|| AppError::not_found(codes::INCIDENT_NOT_FOUND, "incident not found"))?;
    let timeline = state.incident_ops_store.timeline(org, id).await?;
    Ok(Json(IncidentDetail { incident, timeline }))
}

#[utoipa::path(
    get, path = "/api/v1/incidents/{id}/notifications", tag = "incidents",
    summary = "Per-channel delivery log for an incident",
    description = "Every paging attempt for the incident — channel, transport, status, \
                   attempt count, error (secrets redacted), and retry schedule — so a \
                   responder can confirm a page went through or see why it failed.",
    params(("id" = Uuid, Path)),
    responses(
        (status = 200, body = [crate::domain::IncidentNotification]),
        (status = 404, body = ApiError),
    ),
)]
pub async fn incident_notifications(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<IncidentsRead>,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<crate::domain::IncidentNotification>>> {
    // 404 a missing/foreign incident rather than returning an empty log, so the
    // response can't be used to probe which incident ids exist in other tenants.
    if state.incident_ops_store.get(org, id).await?.is_none() {
        return Err(AppError::not_found(
            codes::INCIDENT_NOT_FOUND,
            "incident not found",
        ));
    }
    Ok(Json(
        state.incident_ops_store.notifications_for(org, id).await?,
    ))
}

#[utoipa::path(
    post, path = "/api/v1/incidents", tag = "incidents",
    summary = "Declare a manual incident",
    description = "Opens an incident nobody's monitor found. It stays internal and pages no \
                   one unless `visibility` and `notify` say otherwise, so declaring is safe to \
                   do while you are still working out what broke.",
    request_body = NewManualIncident,
    responses((status = 201, body = OpsIncident), (status = 400, body = ApiError)),
)]
pub async fn declare_incident(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<IncidentsWrite>,
    CurrentUser(user): CurrentUser,
    Json(new): Json<NewManualIncident>,
) -> Result<(StatusCode, Json<OpsIncident>)> {
    validation::validate_optional_title(Some(&new.title), "title")?;
    let (notify, publish) = (new.notify, new.visibility == IncidentVisibility::Public);
    let actor = Actor::User(user);
    let inc = state.incident_ops_store.declare(org, new, actor).await?;
    // Same path as the publish button, so the Published event, opening update
    // and subscriber fan-out keep one owner. A failure must not fail the
    // request: the incident is open, and retrying it would 409 on the
    // one-open-incident index forever.
    let inc = if publish {
        match state
            .incident_ops_store
            .publish(org, inc.id, None, None, actor)
            .await
        {
            Ok(published) => published.unwrap_or(inc),
            Err(err) => {
                tracing::warn!(
                    %org, incident_id = %inc.id, error = %err,
                    "declared incident stayed internal: publishing it failed"
                );
                inc
            }
        }
    } else {
        inc
    };
    if notify {
        state.signal_incident(org, inc.id, NotificationReason::Opened);
    }
    Ok((StatusCode::CREATED, Json(inc)))
}

#[utoipa::path(
    post, path = "/api/v1/incidents/{id}/acknowledge", tag = "incidents",
    summary = "Acknowledge an incident (take ownership, halt escalation)",
    params(("id" = Uuid, Path)), request_body = LifecycleBody,
    responses((status = 200, body = OpsIncident), (status = 404, body = ApiError), (status = 409, body = ApiError)),
)]
pub async fn acknowledge_incident(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<IncidentsWrite>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
    Json(body): Json<LifecycleBody>,
) -> Result<Json<OpsIncident>> {
    let note = clean_note(body.note)?;
    let outcome = state
        .incident_ops_store
        .acknowledge(org, id, Actor::User(user), note, None)
        .await?;
    lifecycle_response(outcome)
}

#[utoipa::path(
    post, path = "/api/v1/incidents/{id}/resolve", tag = "incidents",
    summary = "Resolve an incident",
    params(("id" = Uuid, Path)), request_body = LifecycleBody,
    responses((status = 200, body = OpsIncident), (status = 404, body = ApiError)),
)]
pub async fn resolve_incident(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<IncidentsWrite>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
    Json(body): Json<LifecycleBody>,
) -> Result<Json<OpsIncident>> {
    let note = clean_note(body.note)?;
    let outcome = state
        .incident_ops_store
        .resolve(org, id, Actor::User(user), note)
        .await?;
    if let LifecycleOutcome::Updated(inc) = &outcome {
        state.signal_incident(org, inc.id, NotificationReason::Resolved);
    }
    lifecycle_response(outcome)
}

#[utoipa::path(
    post, path = "/api/v1/incidents/{id}/reopen", tag = "incidents",
    summary = "Reopen a resolved incident",
    params(("id" = Uuid, Path)), request_body = LifecycleBody,
    responses((status = 200, body = OpsIncident), (status = 404, body = ApiError), (status = 409, body = ApiError)),
)]
pub async fn reopen_incident(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<IncidentsWrite>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
    Json(body): Json<LifecycleBody>,
) -> Result<Json<OpsIncident>> {
    let note = clean_note(body.note)?;
    let outcome = state
        .incident_ops_store
        .reopen(org, id, Actor::User(user), note)
        .await?;
    // Declared quiet stays quiet for its whole life, reopening included.
    if let LifecycleOutcome::Updated(inc) = &outcome
        && inc.paging_enabled
    {
        state.signal_incident(org, inc.id, NotificationReason::Reopened);
    }
    lifecycle_response(outcome)
}

#[utoipa::path(
    post, path = "/api/v1/incidents/{id}/assign", tag = "incidents",
    summary = "Assign or unassign an incident's owner",
    params(("id" = Uuid, Path)), request_body = AssignBody,
    responses((status = 200, body = OpsIncident), (status = 404, body = ApiError)),
)]
pub async fn assign_incident(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<IncidentsWrite>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
    Json(body): Json<AssignBody>,
) -> Result<Json<OpsIncident>> {
    match state
        .incident_ops_store
        .assign(org, id, body.user_id, Actor::User(user))
        .await?
    {
        Some(inc) => Ok(Json(inc)),
        None => Err(AppError::not_found(
            codes::INCIDENT_NOT_FOUND,
            "incident not found",
        )),
    }
}

#[utoipa::path(
    post, path = "/api/v1/incidents/{id}/notes", tag = "incidents",
    summary = "Add an internal note to the incident timeline",
    params(("id" = Uuid, Path)), request_body = NoteBody,
    responses((status = 201, body = IncidentEvent), (status = 400, body = ApiError), (status = 404, body = ApiError)),
)]
pub async fn add_incident_note(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<IncidentsWrite>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
    Json(body): Json<NoteBody>,
) -> Result<(StatusCode, Json<IncidentEvent>)> {
    validate_message(&body.message, "message")?;
    let event = state
        .incident_ops_store
        .add_note(org, id, Actor::User(user), body.message)
        .await?
        .ok_or_else(|| AppError::not_found(codes::INCIDENT_NOT_FOUND, "incident not found"))?;
    Ok((StatusCode::CREATED, Json(event)))
}

#[utoipa::path(
    post, path = "/api/v1/incidents/{id}/publish", tag = "incidents",
    summary = "Publish an incident to its monitor's status pages",
    description = "Flips visibility to public so the incident shows on any status page \
                   whose components include this monitor. Optionally seeds the public \
                   title/description; omitted fields keep their stored value.",
    params(("id" = Uuid, Path)), request_body = PublishBody,
    responses((status = 200, body = OpsIncident), (status = 400, body = ApiError), (status = 404, body = ApiError)),
)]
pub async fn publish_incident(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<IncidentsWrite>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
    Json(body): Json<PublishBody>,
) -> Result<Json<OpsIncident>> {
    validation::validate_optional_title(Some(&body.public_title), "public_title")?;
    validation::validate_optional_description(
        Some(&body.public_description),
        "public_description",
    )?;
    let inc = state
        .incident_ops_store
        .publish(
            org,
            id,
            body.public_title,
            body.public_description,
            Actor::User(user),
        )
        .await?
        .ok_or_else(|| AppError::not_found(codes::INCIDENT_NOT_FOUND, "incident not found"))?;
    invalidate_incident_pages(&state, org, inc.target_id).await;
    Ok(Json(inc))
}

/// Bust the cached HTML status pages that list this incident's monitor, so a
/// visibility flip shows/hides immediately instead of after the page TTL. The
/// JSON/RSS/detail surfaces are already live. Best-effort.
async fn invalidate_incident_pages(
    state: &AppState,
    org: crate::domain::OrgId,
    target_id: Option<Uuid>,
) {
    super::invalidate_pages_for(state, org, target_id.as_slice()).await;
}

#[utoipa::path(
    post, path = "/api/v1/incidents/{id}/unpublish", tag = "incidents",
    summary = "Hide an incident from public status pages",
    params(("id" = Uuid, Path)),
    responses((status = 200, body = OpsIncident), (status = 404, body = ApiError)),
)]
pub async fn unpublish_incident(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<IncidentsWrite>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
) -> Result<Json<OpsIncident>> {
    let inc = state
        .incident_ops_store
        .unpublish(org, id, Actor::User(user))
        .await?
        .ok_or_else(|| AppError::not_found(codes::INCIDENT_NOT_FOUND, "incident not found"))?;
    invalidate_incident_pages(&state, org, inc.target_id).await;
    Ok(Json(inc))
}

// ── Metrics & postmortem ────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
pub struct MetricsQuery {
    /// Trailing window in days (clamped to 1..=365; default 30).
    pub window_days: Option<u32>,
}

#[utoipa::path(
    get, path = "/api/v1/incidents/metrics", tag = "incidents",
    summary = "Incident metrics over a trailing window (MTTA/MTTR, counts, noisiest monitors)",
    params(("window_days" = Option<u32>, Query, description = "1..=365, default 30")),
    responses((status = 200, body = IncidentMetrics)),
)]
pub async fn incident_metrics(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<IncidentsRead>,
    Query(q): Query<MetricsQuery>,
) -> Result<Json<IncidentMetrics>> {
    let window = q.window_days.unwrap_or(30).clamp(1, 365);
    Ok(Json(state.incident_ops_store.metrics(org, window).await?))
}

fn validate_postmortem(p: &PostmortemUpsert) -> Result<()> {
    validation::validate_optional_description(Some(&p.summary), "summary")?;
    validation::validate_optional_description(Some(&p.root_cause), "root_cause")?;
    validation::validate_optional_description(Some(&p.impact), "impact")?;
    for item in &p.action_items {
        validate_message(&item.text, "action_items.text")?;
    }
    Ok(())
}

#[utoipa::path(
    get, path = "/api/v1/incidents/{id}/postmortem", tag = "incidents",
    summary = "Get an incident's postmortem",
    params(("id" = Uuid, Path)),
    responses((status = 200, body = IncidentPostmortem), (status = 404, body = ApiError)),
)]
pub async fn get_postmortem(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<IncidentsRead>,
    Path(id): Path<Uuid>,
) -> Result<Json<IncidentPostmortem>> {
    state
        .postmortem_store
        .get(org, id)
        .await?
        .map(Json)
        .ok_or_else(|| AppError::not_found(codes::POSTMORTEM_NOT_FOUND, "postmortem not found"))
}

#[utoipa::path(
    put, path = "/api/v1/incidents/{id}/postmortem", tag = "incidents",
    summary = "Create or replace an incident's postmortem",
    params(("id" = Uuid, Path)), request_body = PostmortemUpsert,
    responses((status = 200, body = IncidentPostmortem), (status = 400, body = ApiError), (status = 404, body = ApiError)),
)]
pub async fn put_postmortem(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<IncidentsWrite>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
    Json(body): Json<PostmortemUpsert>,
) -> Result<Json<IncidentPostmortem>> {
    validate_postmortem(&body)?;
    state
        .postmortem_store
        .upsert(org, id, user, body)
        .await?
        .map(Json)
        .ok_or_else(|| AppError::not_found(codes::INCIDENT_NOT_FOUND, "incident not found"))
}

#[utoipa::path(
    post, path = "/api/v1/incidents/{id}/postmortem/publish", tag = "incidents",
    summary = "Publish an incident's postmortem",
    params(("id" = Uuid, Path)),
    responses((status = 200, body = IncidentPostmortem), (status = 404, body = ApiError)),
)]
pub async fn publish_postmortem(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<IncidentsWrite>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
) -> Result<Json<IncidentPostmortem>> {
    let pm = state
        .postmortem_store
        .set_published(org, id, true)
        .await?
        .ok_or_else(|| AppError::not_found(codes::POSTMORTEM_NOT_FOUND, "postmortem not found"))?;
    // Best-effort audit event: the publish already committed, so a failure here
    // must not 500 (which would prompt a retry that double-publishes/logs).
    if let Err(err) = state
        .incident_ops_store
        .append_event(
            org,
            id,
            IncidentEventKind::PostmortemPublished,
            Actor::User(user),
            None,
        )
        .await
    {
        tracing::warn!(incident_id = %id, error = %err, "failed to record postmortem-published event");
    }
    Ok(Json(pm))
}

#[utoipa::path(
    post, path = "/api/v1/incidents/{id}/postmortem/unpublish", tag = "incidents",
    summary = "Unpublish an incident's postmortem",
    params(("id" = Uuid, Path)),
    responses((status = 200, body = IncidentPostmortem), (status = 404, body = ApiError)),
)]
pub async fn unpublish_postmortem(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<IncidentsWrite>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
) -> Result<Json<IncidentPostmortem>> {
    let pm = state
        .postmortem_store
        .set_published(org, id, false)
        .await?
        .ok_or_else(|| AppError::not_found(codes::POSTMORTEM_NOT_FOUND, "postmortem not found"))?;
    // Best-effort audit event (see publish_postmortem).
    if let Err(err) = state
        .incident_ops_store
        .append_event(
            org,
            id,
            IncidentEventKind::PostmortemUnpublished,
            Actor::User(user),
            None,
        )
        .await
    {
        tracing::warn!(incident_id = %id, error = %err, "failed to record postmortem-unpublished event");
    }
    Ok(Json(pm))
}

#[cfg(test)]
mod tests {
    use super::*;
    use validation::{validate_optional_description, validate_optional_title};

    #[test]
    fn empty_title_rejected_only_when_explicit_value() {
        // null clears — allowed
        assert!(validate_optional_title(Some(&None), "public_title").is_ok());
        // missing — allowed
        assert!(validate_optional_title(None, "public_title").is_ok());
        // whitespace — rejected
        let bad = Some("   ".to_string());
        assert!(matches!(
            validate_optional_title(Some(&bad), "public_title"),
            Err(AppError::BadRequest { code, .. }) if code == codes::EMPTY_TITLE
        ));
    }

    #[test]
    fn title_length_capped() {
        let bad = Some("x".repeat(validation::MAX_TITLE + 1));
        assert!(matches!(
            validate_optional_title(Some(&bad), "public_title"),
            Err(AppError::BadRequest { code, .. }) if code == codes::TITLE_TOO_LONG
        ));
    }

    #[test]
    fn description_length_capped() {
        let bad = Some("x".repeat(validation::MAX_DESCRIPTION + 1));
        assert!(matches!(
            validate_optional_description(Some(&bad), "public_description"),
            Err(AppError::BadRequest { code, .. }) if code == codes::DESCRIPTION_TOO_LONG
        ));
    }

    #[test]
    fn empty_message_rejected() {
        assert!(matches!(
            validate_message("", "message"),
            Err(AppError::BadRequest { code, .. }) if code == codes::EMPTY_MESSAGE
        ));
        assert!(matches!(
            validate_message("   \n\t", "message"),
            Err(AppError::BadRequest { code, .. }) if code == codes::EMPTY_MESSAGE
        ));
    }

    #[test]
    fn long_message_rejected() {
        let m = "x".repeat(validation::MAX_MESSAGE + 1);
        assert!(matches!(
            validate_message(&m, "message"),
            Err(AppError::BadRequest { code, .. }) if code == codes::MESSAGE_TOO_LONG
        ));
    }

    #[test]
    fn state_filter_parses_known_and_rejects_unknown() {
        assert_eq!(parse_state_filter(None).unwrap(), None);
        assert_eq!(
            parse_state_filter(Some("acknowledged")).unwrap(),
            Some(IncidentState::Acknowledged)
        );
        assert!(matches!(
            parse_state_filter(Some("bogus")),
            Err(AppError::BadRequest { code, .. }) if code == codes::INCIDENT_INVALID_STATE
        ));
    }

    #[test]
    fn blank_note_becomes_none_long_note_rejected() {
        assert_eq!(clean_note(None).unwrap(), None);
        assert_eq!(clean_note(Some("  \n".into())).unwrap(), None);
        assert_eq!(
            clean_note(Some("on it".into())).unwrap(),
            Some("on it".into())
        );
        let long = "x".repeat(validation::MAX_MESSAGE + 1);
        assert!(matches!(
            clean_note(Some(long)),
            Err(AppError::BadRequest { code, .. }) if code == codes::MESSAGE_TOO_LONG
        ));
    }

    #[test]
    fn lifecycle_response_maps_outcomes() {
        assert!(matches!(
            lifecycle_response(LifecycleOutcome::NotFound),
            Err(AppError::NotFound { code, .. }) if code == codes::INCIDENT_NOT_FOUND
        ));
        let err = crate::domain::TransitionError {
            from: IncidentState::Resolved,
            transition: crate::domain::IncidentTransition::Acknowledge,
        };
        assert!(matches!(
            lifecycle_response(LifecycleOutcome::IllegalTransition(err)),
            Err(AppError::Conflict { code, .. }) if code == codes::INCIDENT_INVALID_STATE
        ));
    }
}
