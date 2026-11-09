//! Alert sources: the tools that send alerts in, the URL each posts to, and
//! its delivery log. Binding a source to an escalation policy takes the same
//! owner rule as putting a monitor on a ladder.

use crate::request::json::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::AppendHeaders;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::app::AppState;
use crate::auth::scope::Scope;
use crate::domain::{
    AlertGrouping, AlertSource, AlertSourceKind, AlertSourceUpdate, NewAlertSource, OrgId,
    TargetAlerts, UserId, WriteSource,
};
use crate::error::{ApiError, AppError, Result, codes};
use crate::pagination::AlertEventCursor;
use crate::pagination::page::CursorPage;
use crate::request::{
    AlertSourcesDelete, AlertSourcesRead, AlertSourcesWrite, Authorized, CurrentUser,
    RequestSource, TokenScopes,
};
use crate::storage::alert_ingest::Status;
use crate::storage::traits::AlertEventView;
use crate::storage::{MembershipStatus, membership_status};

#[derive(Debug, Serialize, ToSchema)]
pub struct AlertSourceView {
    pub id: Uuid,
    pub name: String,
    pub kind: AlertSourceKind,
    /// Off while paused: firings open nothing, resolves still close.
    pub enabled: bool,
    /// Set while the plan holds the source: a firing for a new key answers
    /// 403, an open one still repeats, and a resolve still closes.
    #[schema(nullable = true)]
    pub plan_hold_at: Option<DateTime<Utc>>,
    pub grouping: AlertGrouping,
    #[schema(nullable = true)]
    pub escalation_policy_id: Option<Uuid>,
    #[schema(value_type = Vec<crate::domain::alert::AlertBindingInput>)]
    pub alerts: TargetAlerts,
    pub notify_recovery: bool,
    /// Seconds before the first reminder while an incident stays
    /// unacknowledged. 0 = off.
    pub renotify_interval_secs: u32,
    /// A signing secret is set, so unsigned requests are refused.
    pub signed: bool,
    /// Where the tool posts, with the token in the path. Only for a caller
    /// that can write alert sources; `null` too when the stored token can't
    /// be opened (the key that sealed it was rotated out).
    #[schema(nullable = true)]
    pub ingest_url: Option<String>,
    /// The same token, for a tool that can send it as `Authorization: Bearer`
    /// to `/hooks/alerts`. Shown under the same rule as `ingest_url`.
    #[schema(nullable = true)]
    pub ingest_token: Option<String>,
    #[schema(nullable = true)]
    pub token_rotated_at: Option<DateTime<Utc>>,
    /// While set, the token before the last rotation still works, until this
    /// instant.
    #[schema(nullable = true)]
    pub previous_token_expires_at: Option<DateTime<Utc>>,
    /// Evidence the old token is still in use. `null` once the overlap ends.
    #[schema(nullable = true)]
    pub previous_token_last_used_at: Option<DateTime<Utc>>,
    /// The last request that reached the source, stamped at most once a
    /// minute.
    #[schema(nullable = true)]
    pub last_event_at: Option<DateTime<Utc>>,
    /// No live policy of its own, no org default and no enabled bound
    /// channel: an alert would open an incident and page nobody.
    pub routes_to_nobody: bool,
    pub write_source: WriteSource,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The URL a source's tool posts to.
pub(crate) fn ingest_url(public_base_url: &str, token: &str) -> String {
    format!(
        "{}/hooks/alerts/{token}",
        public_base_url.trim_end_matches('/')
    )
}

impl AlertSourceView {
    fn new(source: AlertSource, public_base_url: &str, shows_token: bool) -> Self {
        let token = source.token.as_deref().filter(|_| shows_token);
        Self {
            id: source.id,
            ingest_url: token.map(|t| ingest_url(public_base_url, t)),
            ingest_token: token.map(str::to_owned),
            previous_token_expires_at: source.open_overlap(),
            previous_token_last_used_at: source.open_overlap().and(source.prev_token_last_used_at),
            name: source.name,
            kind: source.kind,
            enabled: source.enabled,
            plan_hold_at: source.plan_hold_at,
            grouping: source.grouping,
            escalation_policy_id: source.escalation_policy_id,
            alerts: source.alerts,
            notify_recovery: source.notify_recovery,
            renotify_interval_secs: source.renotify_interval_secs,
            signed: source.signed,
            token_rotated_at: source.token_rotated_at,
            last_event_at: source.last_event_at,
            routes_to_nobody: source.routes_to_nobody,
            write_source: source.write_source,
            created_at: source.created_at,
            updated_at: source.updated_at,
        }
    }
}

fn view(state: &AppState, source: AlertSource, scopes: &TokenScopes) -> AlertSourceView {
    let shows_token = scopes.require(Scope::AlertSourcesWrite).is_ok();
    AlertSourceView::new(source, &state.cfg.auth.public_base_url, shows_token)
}

pub(crate) fn not_found() -> AppError {
    AppError::not_found(codes::ALERT_SOURCE_NOT_FOUND, "alert source not found")
}

/// A change to the source's escalation policy decides who is paged, so it
/// takes the owner and a token allowed `oncall:write`, as a monitor's binding
/// does. Binding one also takes on-call in the plan; clearing one does not, so
/// an account that lost on-call can still unbind.
async fn gate_policy(
    state: &AppState,
    org: OrgId,
    user: UserId,
    scopes: &TokenScopes,
    wanted: Option<Uuid>,
) -> Result<()> {
    if wanted.is_some() {
        let plan = state.quotas.limit_for_org(org).await?;
        crate::api::handlers::on_call::gate_on_call(state, &plan)?;
    }
    scopes.require(Scope::OnCallWrite)?;
    match membership_status(state.require_db()?, user, org).await? {
        MembershipStatus::Owner => Ok(()),
        MembershipStatus::Member | MembershipStatus::None => Err(AppError::Forbidden),
    }
}

#[utoipa::path(
    get, path = "/api/v1/alert-sources", tag = "alert-sources",
    summary = "List alert sources",
    responses((status = 200, body = [AlertSourceView])),
)]
pub async fn list(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<AlertSourcesRead>,
    scopes: TokenScopes,
) -> Result<Json<Vec<AlertSourceView>>> {
    let sources = state.alert_source_store.list(org).await?;
    Ok(Json(
        sources
            .into_iter()
            .map(|s| view(&state, s, &scopes))
            .collect(),
    ))
}

#[utoipa::path(
    get, path = "/api/v1/alert-sources/{id}", tag = "alert-sources",
    summary = "Get an alert source",
    params(("id" = Uuid, Path)),
    responses((status = 200, body = AlertSourceView), (status = 404, body = ApiError)),
)]
pub async fn get(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<AlertSourcesRead>,
    scopes: TokenScopes,
    Path(id): Path<Uuid>,
) -> Result<Json<AlertSourceView>> {
    let source = state
        .alert_source_store
        .get(org, id)
        .await?
        .ok_or_else(not_found)?;
    Ok(Json(view(&state, source, &scopes)))
}

#[utoipa::path(
    post, path = "/api/v1/alert-sources", tag = "alert-sources",
    summary = "Create an alert source",
    description = "Mints the URL the tool posts to, shown in `ingest_url`. `kind` is fixed \
                   from here on. The source must reach someone: its own escalation policy, \
                   the org default policy, or an enabled channel in `alerts`.",
    request_body = NewAlertSource,
    responses(
        (status = 201, body = AlertSourceView,
            headers(("Location" = String, description = "URL of the new source"))),
        (status = 400, body = ApiError, description = "Blank or long name, bad secret, reminder or channels"),
        (status = 403, body = ApiError, description = "ALERT_SOURCES_DISABLED, or binding a policy without the owner rule"),
        (status = 409, body = ApiError, description = "ALERT_SOURCE_NAME_TAKEN"),
        (status = 422, body = ApiError, description = "ALERT_SOURCE_NO_ROUTE, or QUOTA_EXCEEDED naming max_alert_sources"),
    ),
)]
pub async fn create(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<AlertSourcesWrite>,
    CurrentUser(user): CurrentUser,
    RequestSource(source): RequestSource,
    scopes: TokenScopes,
    Json(new): Json<NewAlertSource>,
) -> Result<(
    StatusCode,
    AppendHeaders<[(axum::http::HeaderName, HeaderValue); 1]>,
    Json<AlertSourceView>,
)> {
    let cap = state.quotas.alert_source_cap(org, Some(user)).await?;
    if new.escalation_policy_id.is_some() {
        gate_policy(&state, org, user, &scopes, new.escalation_policy_id).await?;
    }
    let created = state
        .alert_source_store
        .create(org, new, source, Some(user), cap)
        .await?;
    let location = HeaderValue::from_str(&format!("/api/v1/alert-sources/{}", created.id))
        .expect("uuid produces ascii-only path");
    Ok((
        StatusCode::CREATED,
        AppendHeaders([(header::LOCATION, location)]),
        Json(view(&state, created, &scopes)),
    ))
}

#[utoipa::path(
    patch, path = "/api/v1/alert-sources/{id}", tag = "alert-sources",
    summary = "Change an alert source",
    description = "Omit fields you don't want to change. `enabled: false` pauses it: firings \
                   open nothing and resolves still close. `grouping` can change only while the \
                   source holds no open incident. `signing_secret: null` stops checking \
                   signatures.",
    params(("id" = Uuid, Path)),
    request_body = AlertSourceUpdate,
    responses(
        (status = 200, body = AlertSourceView),
        (status = 400, body = ApiError),
        (status = 403, body = ApiError, description = "Changing the policy without the owner rule"),
        (status = 404, body = ApiError),
        (status = 409, body = ApiError, description = "ALERT_SOURCE_NAME_TAKEN or ALERT_SOURCE_GROUPING_LOCKED"),
        (status = 422, body = ApiError, description = "ALERT_SOURCE_NO_ROUTE"),
    ),
)]
pub async fn update(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<AlertSourcesWrite>,
    CurrentUser(user): CurrentUser,
    RequestSource(source): RequestSource,
    scopes: TokenScopes,
    Path(id): Path<Uuid>,
    Json(mut update): Json<AlertSourceUpdate>,
) -> Result<Json<AlertSourceView>> {
    if let Some(wanted) = update.escalation_policy_id {
        let current = state
            .alert_source_store
            .get(org, id)
            .await?
            .ok_or_else(not_found)?
            .escalation_policy_id;
        if wanted == current {
            // Unchanged, so not written: writing it back would undo an owner's
            // change landing in between, without the owner rule.
            update.escalation_policy_id = None;
        } else {
            gate_policy(&state, org, user, &scopes, wanted).await?;
        }
    }
    let updated = state
        .alert_source_store
        .update(org, id, update, source, Some(user))
        .await?
        .ok_or_else(not_found)?;
    Ok(Json(view(&state, updated, &scopes)))
}

#[utoipa::path(
    delete, path = "/api/v1/alert-sources/{id}", tag = "alert-sources",
    summary = "Delete an alert source",
    description = "Its URL stops working at once. The incidents it holds open are resolved, \
                   and the people paged about them hear that they closed.",
    params(("id" = Uuid, Path)),
    responses((status = 204, description = "Deleted"), (status = 404, body = ApiError)),
)]
pub async fn delete(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<AlertSourcesDelete>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode> {
    let deleted = state
        .alert_source_store
        .delete(org, id, Some(user))
        .await?
        .ok_or_else(not_found)?;
    for incident in deleted.closed {
        state.signal_incident(org, incident, crate::domain::NotificationReason::Resolved);
    }
    if let Ok(pool) = state.require_db() {
        crate::quotas::holds::release_after_delete(pool, &state.quotas, org).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RotateAlertSourceRequest {
    /// `true` cuts the old token off in the same commit (for a leaked one);
    /// the default keeps it working for 24 hours.
    #[serde(default)]
    pub revoke_previous_immediately: bool,
}

#[utoipa::path(
    post, path = "/api/v1/alert-sources/{id}/rotate", tag = "alert-sources",
    summary = "Rotate an alert source's token",
    description = "The new token works at once. Unless `revoke_previous_immediately`, the old \
                   one keeps working for 24 hours, so a tool not yet updated still pages; \
                   `previous_token_last_used_at` shows whether anything still sends it.",
    params(("id" = Uuid, Path)),
    request_body = RotateAlertSourceRequest,
    responses((status = 200, body = AlertSourceView), (status = 404, body = ApiError)),
)]
pub async fn rotate(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<AlertSourcesWrite>,
    CurrentUser(user): CurrentUser,
    scopes: TokenScopes,
    Path(id): Path<Uuid>,
    Json(req): Json<RotateAlertSourceRequest>,
) -> Result<Json<AlertSourceView>> {
    let rotated = state
        .alert_source_store
        .rotate(org, id, req.revoke_previous_immediately, Some(user))
        .await?
        .ok_or_else(not_found)?;
    Ok(Json(view(&state, rotated, &scopes)))
}

#[utoipa::path(
    delete, path = "/api/v1/alert-sources/{id}/previous-token", tag = "alert-sources",
    summary = "End a rotation's overlap early",
    description = "Cuts off the token from before the last rotation now instead of at \
                   `previous_token_expires_at`. Idempotent: 204 whether or not an overlap \
                   was open.",
    params(("id" = Uuid, Path)),
    responses((status = 204, description = "No overlap remains"), (status = 404, body = ApiError)),
)]
pub async fn revoke_previous(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<AlertSourcesWrite>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode> {
    state
        .alert_source_store
        .get(org, id)
        .await?
        .ok_or_else(not_found)?;
    state
        .alert_source_store
        .revoke_previous(org, id, Some(user))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

const DEFAULT_EVENTS: usize = 50;
pub const MAX_EVENTS: usize = 200;

#[derive(Debug, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct EventsQuery {
    /// `next_cursor` from the page before.
    pub cursor: Option<String>,
    /// At most this many, newest first. Default 50, at most 200.
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AlertEventItem {
    /// When the request arrived, shared by every event in it.
    pub received_at: DateTime<Utc>,
    /// `opened`, `repeat`, `resolved`, `ignored`, `paused`, `held`, `capped`,
    /// `unparsed`, or `unavailable` for a request the database failed or did
    /// not keep up with, answered 503 for the sender to retry.
    pub outcome: String,
    /// Empty for a request that named no alert.
    pub dedup_key: String,
    #[schema(nullable = true)]
    pub incident_id: Option<Uuid>,
    /// Why, when the outcome alone does not say.
    pub note: String,
    /// The start of the request's body, on its first event only. Empty for a
    /// request that only repeated open alerts, and once the evidence window
    /// has passed.
    pub body: String,
}

impl From<AlertEventView> for AlertEventItem {
    fn from(e: AlertEventView) -> Self {
        Self {
            received_at: e.received_at,
            outcome: e.outcome.as_str().to_owned(),
            dedup_key: e.dedup_key,
            incident_id: e.incident_id,
            note: e.note,
            body: e.body,
        }
    }
}

#[utoipa::path(
    get, path = "/api/v1/alert-sources/{id}/events", tag = "alert-sources",
    summary = "An alert source's delivery log",
    description = "What each event in each request did, newest first. Kept for the org's \
                   retention; the body sample for its shorter evidence window. A request \
                   refused before it was read (bad token, over the size limit, throttled) \
                   is not here.",
    params(("id" = Uuid, Path), EventsQuery),
    responses(
        (status = 200, body = CursorPage<AlertEventItem>),
        (status = 400, body = ApiError, description = "INVALID_CURSOR"),
        (status = 404, body = ApiError),
    ),
)]
pub async fn events(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<AlertSourcesRead>,
    Path(id): Path<Uuid>,
    Query(q): Query<EventsQuery>,
) -> Result<Json<CursorPage<AlertEventItem>>> {
    state
        .alert_source_store
        .get(org, id)
        .await?
        .ok_or_else(not_found)?;
    Ok(Json(
        events_page(&state, org, id, &[], q.cursor.as_deref(), q.limit).await?,
    ))
}

/// A page of a source's delivery log, newest first, after `cursor`, limited to
/// `outcomes` when any are given. The caller has checked the source is the
/// org's. Empty without ClickHouse.
pub(crate) async fn events_page(
    state: &AppState,
    org: OrgId,
    id: Uuid,
    outcomes: &[Status],
    cursor: Option<&str>,
    limit: Option<usize>,
) -> Result<CursorPage<AlertEventItem>> {
    let after = cursor.map(AlertEventCursor::decode).transpose()?;
    let Some(log) = state.alert_event_log.as_ref() else {
        return Ok(CursorPage::new(Vec::new(), None));
    };
    let limit = limit.unwrap_or(DEFAULT_EVENTS).clamp(1, MAX_EVENTS);
    let after = after
        .as_ref()
        .map(|c| (c.received_at, c.dedup_key.as_str()));
    let mut rows = log.recent(org, id, outcomes, after, limit + 1).await?;
    let next_cursor = (rows.len() > limit).then(|| {
        rows.truncate(limit);
        let last = rows.last().expect("a full page has a last row");
        AlertEventCursor {
            received_at: last.received_at,
            dedup_key: last.dedup_key.clone(),
        }
        .encode()
    });
    Ok(CursorPage::new(
        rows.into_iter().map(Into::into).collect(),
        next_cursor,
    ))
}
