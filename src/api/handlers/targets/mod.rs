use crate::api::json::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::AppendHeaders;
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use uuid::Uuid;

use crate::api::redaction::Redacted;
use crate::api::types::{
    BulkAction, BulkActionFailure, BulkActionRequest, BulkActionResponse, TestRequest, TestResponse,
};
use crate::app::AppState;
use crate::auth::scope::Scope;
use crate::domain::agent_wire::DispatchKind;
use crate::domain::{
    CheckResult, CheckSpec, NewTarget, NewTargetWithRegions, OrgId, Target, TargetUpdate,
};
use crate::error::ApiError;
use crate::error::codes;
use crate::error::{AppError, Result};
use crate::metric_names;
use crate::pagination::page::{PageEnvelope, PageOfTarget};
use crate::request::{
    Authorized, CurrentOrg, CurrentUser, RequestSource, TargetsDelete, TargetsExecute, TargetsRead,
    TargetsWrite, TokenScopes,
};
use crate::storage::TargetFilter;
use crate::targets::{HeartbeatInfo, SetManualState, heartbeat_info, heartbeat_info_from};

const BULK_MAX: usize = 10_000;
const LIST_LIMIT_DEFAULT: usize = 50;
const LIST_LIMIT_MAX: usize = 10_000;

use crate::targets::validate::{
    canonicalize_check, carry_credentials, carry_flags, carry_flow_secrets, gate_flow,
    gate_flow_steps, normalize_tags, reject_passive_probe, take_cleared_credentials,
    validate_alert_confirmations, validate_alerts, validate_check, validate_group_name,
    validate_new_target, validate_region_policy, validate_renotify_interval,
};

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    /// Page size (default 50, max 10000).
    pub limit: Option<usize>,
    /// Page offset (default 0).
    #[serde(default)]
    pub offset: usize,
    /// Filter by exact tag match.
    pub tag: Option<String>,
    /// Filter by enabled flag.
    pub enabled: Option<bool>,
}

impl ListQuery {
    fn effective_limit(&self) -> usize {
        self.limit.unwrap_or(LIST_LIMIT_DEFAULT).min(LIST_LIMIT_MAX)
    }

    fn to_filter(&self) -> TargetFilter {
        TargetFilter {
            limit: Some(self.effective_limit()),
            offset: self.offset,
            tag: self.tag.clone(),
            enabled: self.enabled,
            ..Default::default()
        }
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/targets",
    tag = "targets",
    summary = "List targets (paginated)",
    params(ListQuery),
    responses(
        (status = 200, body = PageOfTarget, example = json!({
            "items": [{
                "id": "01h7m8z4n6v0e1m7v7y6x8x8x8",
                "name": "api prod",
                "check": {"type": "http", "url": "https://example.com/healthz", "method": "GET"},
                "interval": 60,
                "enabled": true,
                "tags": ["prod"],
                "created_at": "2026-05-13T12:00:00.000Z",
                "updated_at": "2026-05-13T12:00:00.000Z"
            }],
            "limit": 50, "offset": 0, "has_more": false
        })),
        (status = 400, body = ApiError),
    ),
)]
pub async fn list(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<TargetsRead>,
    Query(query): Query<ListQuery>,
) -> Result<Redacted<PageOfTarget>> {
    let limit = query.effective_limit();
    let offset = query.offset;
    let filter = query.to_filter();
    // The filter struct owns its limit/offset; bump the limit by 1 to peek
    // a row past the page boundary and let the envelope decide has_more.
    let peek_filter = TargetFilter {
        limit: filter.limit.map(|n| n + 1),
        ..filter
    };
    let peek = state.target_store.list(org, peek_filter).await?;
    Ok(Redacted::new(PageEnvelope::from_peek(
        peek,
        limit as u32,
        offset as u32,
    )))
}

#[utoipa::path(
    get,
    path = "/api/v1/targets/{id}",
    tag = "targets",
    summary = "Get one target",
    params(("id" = Uuid, Path, description = "Target id")),
    responses(
        (status = 200, body = Target, example = json!({
            "id": "01h7m8z4n6v0e1m7v7y6x8x8x8",
            "name": "api prod",
            "check": {"type": "http", "url": "https://example.com/healthz", "method": "GET"},
            "interval": 60,
            "enabled": true,
            "tags": ["prod"]
        })),
        (status = 404, body = ApiError, example = json!({
            "error": {"code": "TARGET_NOT_FOUND", "message": "target not found", "field": null, "details": null, "trace_id": null}
        })),
    ),
)]
pub async fn get(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<TargetsRead>,
    Path(id): Path<Uuid>,
) -> Result<Redacted<Target>> {
    match state.target_store.get(org, id).await? {
        Some(t) => Ok(Redacted::new(t)),
        None => Err(AppError::not_found(
            codes::TARGET_NOT_FOUND,
            "target not found",
        )),
    }
}

#[utoipa::path(
    post,
    path = "/api/v1/targets",
    tag = "targets",
    summary = "Create a target",
    request_body(content = NewTarget, description = "Target definition", example = json!({
        "name": "api prod",
        "check": {
            "type": "http",
            "url": "https://example.com/healthz",
            "method": "GET",
            "timeout": 10000,
            "follow_redirects": true,
            "max_redirects": 5,
            "expected_status": {"kind": "exact", "value": 200},
            "headers": {},
            "verify_tls": true
        },
        "interval": 60,
        "tags": ["prod"],
        "regions": ["eu-frankfurt", "us-east"]
    })),
    responses(
        (status = 201, description = "Created", body = Target),
        (status = 400, description = "Validation error", body = ApiError, example = json!({
            "error": {"code": "INVALID_URL_SCHEME", "message": "url scheme 'ftp' not allowed", "field": "check.url", "details": null, "trace_id": null}
        })),
        (status = 409, description = "Duplicate (if uniqueness constraint exists)", body = ApiError),
        (status = 503, body = ApiError),
    ),
)]
pub async fn create(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<TargetsWrite>,
    CurrentUser(user): CurrentUser,
    RequestSource(source): RequestSource,
    Json(mut new): Json<NewTarget>,
) -> Result<(
    StatusCode,
    AppendHeaders<[(axum::http::HeaderName, HeaderValue); 1]>,
    Redacted<Target>,
)> {
    let ops = state.target_ops();
    let plan = state.quotas.limit_for_org(org).await?;
    canonicalize_check(&mut new.check)?;
    gate_flow(&new.check, &plan)?;
    ops.vet_new_target(org, &mut new, &plan).await?;
    ops.verify_alert_channels(org, &new.alerts).await?;
    ops.validate_owner_is_member(org, new.owner()).await?;
    new.default_owner(user);
    if matches!(&new.check, CheckSpec::Flow(_)) {
        state.quotas.check_can_create_flow(org, None, 1).await?;
    }
    let snapshot = ops.region_snapshot().await?;
    let regions = ops
        .resolve_create_regions(org, &new, &plan, &snapshot)
        .await?;
    let t = ops.create_target(org, new, source, &plan, regions).await?;
    // UUID hex is always ASCII-safe → infallible.
    let location = HeaderValue::from_str(&format!("/api/v1/targets/{}", t.id))
        .expect("uuid produces ascii-only path");
    Ok((
        StatusCode::CREATED,
        AppendHeaders([(header::LOCATION, location)]),
        Redacted::new(t),
    ))
}

#[utoipa::path(
    patch,
    path = "/api/v1/targets/{id}",
    tag = "targets",
    summary = "Partial update of a target",
    description = "Omit fields you don't want to change. A `check` must keep the stored `type`: a monitor's kind is fixed after creation (400 `CHECK_KIND_IMMUTABLE`), so create a new monitor to watch something else. For HTTP credentials: omit `basic_auth`/`bearer_token` (or send null) to keep the stored value, send an empty sentinel (`[\"\",\"\"]` / `\"\"`) to clear it, or a real value to replace it. The redaction sentinels `[\"***\",\"***\"]` / `\"***\"` return 400, so never echo them back.",
    params(("id" = Uuid, Path)),
    request_body(content = TargetUpdate, example = json!({
        "enabled": false,
        "tags": ["prod", "frozen"]
    })),
    responses(
        (status = 200, body = Target),
        (status = 400, description = "Validation error; REDACTION_SENTINEL if `***` submitted, CHECK_KIND_IMMUTABLE if `check.type` differs from the stored kind", body = ApiError, example = json!({
            "error": {"code": "REDACTION_SENTINEL", "message": "bearer_token contains redaction sentinel", "field": "check.bearer_token", "details": null, "trace_id": null}
        })),
        (status = 404, body = ApiError),
    ),
)]
pub async fn update(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<TargetsWrite>,
    CurrentUser(user): CurrentUser,
    RequestSource(source): RequestSource,
    Path(id): Path<Uuid>,
    Json(mut update): Json<TargetUpdate>,
) -> Result<Redacted<Target>> {
    let ops = state.target_ops();
    // Read once and share: the kind guard, the credential carry, the flow
    // secret carry, and the interval floor all want the same row.
    let mut stored_target: Option<Target> = None;
    if let Some(check) = update.check.as_mut() {
        canonicalize_check(check)?;
        let stored = state
            .target_store
            .get(org, id)
            .await?
            .ok_or_else(|| AppError::not_found(codes::TARGET_NOT_FOUND, "target not found"))?;
        if check.kind() != stored.check.kind() {
            return Err(AppError::check_kind_immutable());
        }
        if let CheckSpec::Http(http) = check {
            let (cleared_basic, cleared_bearer) = take_cleared_credentials(http);
            let (carry_basic, carry_bearer) = carry_flags(http, cleared_basic, cleared_bearer);
            if (carry_basic || carry_bearer)
                && let CheckSpec::Http(stored_http) = &stored.check
            {
                carry_credentials(http, stored_http, carry_basic, carry_bearer);
            }
        }
        // An untouched `***` keeps the stored fill value, so carry before
        // validation.
        if let CheckSpec::Flow(flow) = check
            && let CheckSpec::Flow(stored_flow) = &stored.check
        {
            carry_flow_secrets(flow, stored_flow);
        }
        validate_check(check, &ops.ssrf_guard())?;
        ops.check_abuse(org, check)?;
        ops.validate_variable_refs(org, check).await?;
        // A flow edit is never a net-new flow, so the capability and the count
        // are not re-checked: a downgraded org can still fix a monitor it
        // already runs. The step cap still binds.
        if matches!(check, CheckSpec::Flow(_)) {
            let plan = state.quotas.limit_for_org(org).await?;
            gate_flow_steps(check, &plan)?;
            if let Some(regions) = state.target_store.regions_for_target(org, id).await? {
                ops.ensure_flow_regions_covered(check, &regions).await?;
            }
        }
        stored_target = Some(stored);
    }
    if let Some(alerts) = &update.alerts {
        validate_alerts(alerts)?;
        ops.verify_alert_channels(org, alerts).await?;
    }
    if let Some(tags) = update.tags.as_ref() {
        update.tags = Some(normalize_tags(tags)?);
    }
    validate_alert_confirmations(update.alert_confirmations)?;
    validate_renotify_interval(update.renotify_interval_secs)?;
    if update.region_policy.is_some() {
        let available = state.target_store.available_regions().await?;
        validate_region_policy(update.region_policy, available.len())?;
    }
    if let Some(Some(g)) = update.group_name.as_ref() {
        validate_group_name(Some(g.as_str()))?;
    }
    if let Some(Some(uid)) = update.owner_user_id {
        ops.validate_owner_is_member(org, Some(uid)).await?;
    }
    ops.validate_patch_schedule(org, id, &mut update, stored_target.as_ref())
        .await?;
    // The disabled→enabled re-arm is folded into the store's enable statement,
    // so this path (and every other enable surface) inherits it.
    match state
        .target_store
        .update(org, id, update, Some(source), Some(user))
        .await?
    {
        Some(t) => {
            state.publishing().invalidate_targets(org, &[id]).await;
            Ok(Redacted::new(t))
        }
        None => Err(AppError::not_found(
            codes::TARGET_NOT_FOUND,
            "target not found",
        )),
    }
}

/// The regions a monitor probes from. A single-region deployment is one entry.
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetRegions {
    pub regions: Vec<String>,
}

/// One region in the catalog returned by `GET /api/v1/regions`.
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RegionInfo {
    pub id: String,
    pub name: String,
    pub city: String,
    pub country_code: Option<String>,
    pub continent: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
}

/// The enabled region catalog a monitor may be assigned to.
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RegionCatalog {
    pub regions: Vec<RegionInfo>,
}

#[utoipa::path(
    get, path = "/api/v1/regions", tag = "targets",
    summary = "List the available probe regions",
    responses((status = 200, body = RegionCatalog)),
)]
pub async fn list_regions(
    State(state): State<AppState>,
    Authorized(_org, _): Authorized<TargetsRead>,
) -> Result<Json<RegionCatalog>> {
    let regions = state
        .regions_detailed()
        .await?
        .into_iter()
        .map(|r| RegionInfo {
            id: r.id,
            name: r.name,
            city: r.city,
            country_code: r.country_code,
            continent: r.continent,
            latitude: r.latitude,
            longitude: r.longitude,
        })
        .collect();
    Ok(Json(RegionCatalog { regions }))
}

#[utoipa::path(
    get, path = "/api/v1/targets/{id}/regions", tag = "targets",
    summary = "List the regions a monitor probes from",
    params(("id" = Uuid, Path)),
    responses((status = 200, body = TargetRegions), (status = 404, body = ApiError)),
)]
pub async fn get_target_regions(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<TargetsRead>,
    Path(id): Path<Uuid>,
) -> Result<Json<TargetRegions>> {
    match state.target_store.regions_for_target(org, id).await? {
        Some(regions) => Ok(Json(TargetRegions { regions })),
        None => Err(AppError::not_found(
            codes::TARGET_NOT_FOUND,
            "target not found",
        )),
    }
}

#[utoipa::path(
    put, path = "/api/v1/targets/{id}/regions", tag = "targets",
    summary = "Set the regions a monitor probes from",
    params(("id" = Uuid, Path)), request_body = TargetRegions,
    responses(
        (status = 200, body = TargetRegions),
        (status = 404, body = ApiError),
        (status = 422, body = ApiError),
    ),
)]
pub async fn set_target_regions(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<TargetsWrite>,
    Path(id): Path<Uuid>,
    Json(req): Json<TargetRegions>,
) -> Result<Json<TargetRegions>> {
    let ops = state.target_ops();
    let target = state
        .target_store
        .get(org, id)
        .await?
        .ok_or_else(|| AppError::not_found(codes::TARGET_NOT_FOUND, "target not found"))?;
    crate::targets::validate::reject_passive_regions(&target.check)?;
    let snapshot = ops.region_snapshot().await?;
    let regions = ops
        .vet_requested_regions(org, &req.regions, &snapshot)
        .await?;
    if !state
        .target_store
        .set_target_regions(org, id, &regions)
        .await?
    {
        return Err(AppError::not_found(
            codes::TARGET_NOT_FOUND,
            "target not found",
        ));
    }
    Ok(Json(TargetRegions { regions }))
}

#[utoipa::path(
    get, path = "/api/v1/targets/{id}/heartbeat", tag = "targets",
    summary = "Get a heartbeat monitor's ping URL and last reported run",
    params(("id" = Uuid, Path)),
    responses(
        (status = 200, body = HeartbeatInfo),
        (status = 404, body = ApiError),
    ),
)]
pub async fn get_heartbeat(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<TargetsWrite>,
    Path(id): Path<Uuid>,
) -> Result<Json<HeartbeatInfo>> {
    let target = state
        .target_store
        .get(org, id)
        .await?
        .ok_or_else(|| AppError::not_found(codes::TARGET_NOT_FOUND, "target not found"))?;
    let Some(check) = target.check.as_heartbeat() else {
        return Err(AppError::not_found(
            codes::HEARTBEAT_NOT_CONFIGURED,
            "this monitor is not a heartbeat",
        ));
    };
    Ok(Json(
        heartbeat_info(
            state.heartbeat_store.as_ref(),
            state.results_store.as_ref(),
            &state.cfg.auth.public_base_url,
            org,
            id,
            check,
            target.enabled,
        )
        .await?,
    ))
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RotateHeartbeatRequest {
    /// `true` revokes the old URL in the same commit (for a leaked token);
    /// the default keeps it pinging for 24 hours.
    #[serde(default)]
    pub revoke_previous_immediately: bool,
}

#[utoipa::path(
    post, path = "/api/v1/targets/{id}/heartbeat/rotate", tag = "targets",
    summary = "Rotate a heartbeat monitor's ping URL",
    description = "Mints a replacement ping URL on the same monitor: incidents, \
                   history, share links and status-page bindings are untouched, \
                   and the silence clock is not re-armed. Unless \
                   `revoke_previous_immediately`, the old URL keeps working for \
                   24 hours; `previous_url_last_used_at` in the response shows \
                   whether anything still calls it.",
    params(("id" = Uuid, Path)),
    request_body = RotateHeartbeatRequest,
    responses(
        (status = 200, body = HeartbeatInfo),
        (status = 404, body = ApiError),
    ),
)]
pub async fn rotate_heartbeat(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<TargetsWrite>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
    Json(req): Json<RotateHeartbeatRequest>,
) -> Result<Json<HeartbeatInfo>> {
    let target = state
        .target_store
        .get(org, id)
        .await?
        .ok_or_else(|| AppError::not_found(codes::TARGET_NOT_FOUND, "target not found"))?;
    let Some(check) = target.check.as_heartbeat() else {
        return Err(AppError::not_found(
            codes::HEARTBEAT_NOT_CONFIGURED,
            "this monitor is not a heartbeat",
        ));
    };
    // A row lost to a partial create has never shown a URL, so minting one is
    // the whole job; superseding it would spend the one overlap slot on a
    // token nobody holds.
    if state.heartbeat_store.get(org, id).await?.is_none() {
        let healed = state
            .heartbeat_store
            .ensure(org, id)
            .await?
            .ok_or_else(|| AppError::not_found(codes::TARGET_NOT_FOUND, "target not found"))?;
        return Ok(Json(
            heartbeat_info_from(
                state.results_store.as_ref(),
                &state.cfg.auth.public_base_url,
                org,
                id,
                check,
                target.enabled,
                Some(healed),
            )
            .await,
        ));
    }
    let rotated = state
        .heartbeat_store
        .rotate(org, id, req.revoke_previous_immediately, Some(user))
        .await?
        .ok_or_else(|| AppError::not_found(codes::TARGET_NOT_FOUND, "target not found"))?;
    Ok(Json(
        heartbeat_info_from(
            state.results_store.as_ref(),
            &state.cfg.auth.public_base_url,
            org,
            id,
            check,
            target.enabled,
            Some(rotated),
        )
        .await,
    ))
}

#[utoipa::path(
    delete, path = "/api/v1/targets/{id}/heartbeat/previous", tag = "targets",
    summary = "End a rotation's overlap window early",
    description = "Revokes the pre-rotation ping URL now instead of at \
                   `previous_url_expires_at`. Idempotent: 204 whether or not an \
                   overlap was open.",
    params(("id" = Uuid, Path)),
    responses(
        (status = 204, description = "No overlap remains"),
        (status = 404, body = ApiError),
    ),
)]
pub async fn revoke_heartbeat_previous(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<TargetsWrite>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode> {
    let target = state
        .target_store
        .get(org, id)
        .await?
        .ok_or_else(|| AppError::not_found(codes::TARGET_NOT_FOUND, "target not found"))?;
    if target.check.as_heartbeat().is_none() {
        return Err(AppError::not_found(
            codes::HEARTBEAT_NOT_CONFIGURED,
            "this monitor is not a heartbeat",
        ));
    }
    state
        .heartbeat_store
        .revoke_previous(org, id, Some(user))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get, path = "/api/v1/targets/{id}/state", tag = "targets",
    summary = "Get a manual monitor's state",
    params(("id" = Uuid, Path)),
    responses(
        (status = 200, body = crate::domain::ManualState),
        (status = 404, body = ApiError),
    ),
)]
pub async fn get_manual_state(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<TargetsRead>,
    Path(id): Path<Uuid>,
) -> Result<Json<crate::domain::ManualState>> {
    Ok(Json(state.manual_ops().get(org, id).await?))
}

#[utoipa::path(
    put, path = "/api/v1/targets/{id}/state", tag = "targets",
    summary = "Set a manual monitor's state",
    description = "Sets the state of a monitor whose check `type` is `manual`. \
                   `down` and `degraded` open an incident within about 30 seconds, \
                   and `up` closes it. Each set replaces the previous note, and \
                   is written to the organization's audit log with who made it. \
                   Setting the same state with the same note again changes nothing.",
    params(("id" = Uuid, Path)),
    request_body = SetManualState,
    responses(
        (status = 200, body = crate::domain::ManualState),
        (status = 400, body = ApiError),
        (status = 404, body = ApiError),
        (status = 422, description = "Unknown field or status", body = ApiError),
    ),
)]
pub async fn set_manual_state(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<TargetsWrite>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
    Json(req): Json<SetManualState>,
) -> Result<Json<crate::domain::ManualState>> {
    let change = state.manual_ops().set(org, id, req, Some(user)).await?;
    Ok(Json(change.state))
}

#[utoipa::path(
    delete,
    path = "/api/v1/targets/{id}",
    tag = "targets",
    summary = "Delete a target",
    description = "Deletes target metadata. Historical results are retained until normal retention expires (90 days).",
    params(("id" = Uuid, Path)),
    responses(
        (status = 204, description = "Deleted"),
        (status = 404, body = ApiError, example = json!({
            "error": {"code": "TARGET_NOT_FOUND", "message": "target not found", "field": null, "details": null, "trace_id": null}
        })),
    ),
)]
pub async fn delete(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<TargetsDelete>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode> {
    // Resolve curated pages before the FK cascade clears the join rows.
    let pages = state
        .status_page_store
        .pages_for_targets(org, &[id])
        .await
        .unwrap_or_default();
    if state.target_store.delete(org, id, Some(user)).await? {
        for page in pages {
            state.public_source.invalidate(page).await;
        }
        note_if_emptied(&state, org, 1).await;
        if let Ok(pool) = state.require_db() {
            crate::quotas::holds::release_after_delete(pool, &state.quotas, org).await;
        }
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::not_found(
            codes::TARGET_NOT_FOUND,
            "target not found",
        ))
    }
}

/// An org clearing its whole inventory is a customer walking out, and reads as
/// routine tidying until the account is already cold. `deleted` gates the
/// signal here rather than at each call site, so a delete that hit nothing
/// can't report an org as newly emptied.
async fn note_if_emptied(state: &AppState, org: OrgId, deleted: usize) {
    if deleted == 0 {
        return;
    }
    match state.target_store.summary(org).await {
        Ok(summary) if summary.total == 0 => {
            metrics::counter!(metric_names::ORGS_EMPTIED).increment(1);
            tracing::warn!(org_id = %org.0, "org has no monitors left after a delete");
        }
        Ok(_) => {}
        Err(err) => tracing::warn!(error = %err, "emptied-org check failed (non-fatal)"),
    }
}

#[utoipa::path(
    post,
    path = "/api/v1/targets/bulk",
    tag = "targets",
    summary = "Create up to 10 000 targets in one call",
    request_body(content = Vec<NewTarget>, example = json!([{
        "name": "api-a",
        "check": {"type": "tcp", "host": "db.example.com", "port": 5432, "timeout": 3000},
        "interval": 30
    }])),
    responses(
        (status = 201, description = "All created", body = Vec<Target>),
        (status = 400, description = "Empty payload or per-entry validation error; nothing was created", body = ApiError, example = json!({
            "error": {"code": "BULK_EMPTY", "message": "empty bulk payload", "field": null, "details": null, "trace_id": null}
        })),
        (status = 413, description = "Payload exceeds 10 000 items", body = ApiError),
        (status = 503, body = ApiError),
    ),
)]
pub async fn bulk_create(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<TargetsWrite>,
    CurrentUser(user): CurrentUser,
    RequestSource(source): RequestSource,
    Json(mut items): Json<Vec<NewTarget>>,
) -> Result<(StatusCode, Redacted<Vec<Target>>)> {
    let ops = state.target_ops();
    if items.is_empty() {
        return Err(AppError::bad_request(
            codes::BULK_EMPTY,
            "empty bulk payload",
        ));
    }
    if items.len() > BULK_MAX {
        return Err(AppError::payload_too_large(
            codes::BULK_TOO_LARGE,
            format!("bulk size {} exceeds max {BULK_MAX}", items.len()),
        ));
    }
    let plan = state.quotas.limit_for_org(org).await?;
    let guard = ops.ssrf_guard();
    let snapshot = ops.region_snapshot().await?;
    for new in &mut items {
        canonicalize_check(&mut new.check)?;
        gate_flow(&new.check, &plan)?;
        validate_new_target(new, &guard, &plan)?;
        validate_region_policy(new.region_policy, snapshot.available_count())?;
        ops.verify_alert_channels(org, &new.alerts).await?;
        ops.check_abuse(org, &new.check)?;
        ops.validate_variable_refs(org, &new.check).await?;
    }
    let owner_ids: std::collections::HashSet<Uuid> =
        items.iter().filter_map(NewTarget::owner).collect();
    if !owner_ids.is_empty() {
        let pool = state.require_db()?;
        let members = crate::storage::orgs::list_members(pool, org).await?;
        let member_set: std::collections::HashSet<Uuid> =
            members.iter().map(|m| m.membership.user_id.0).collect();
        for uid in owner_ids {
            if !member_set.contains(&uid) {
                return Err(AppError::bad_request_field(
                    codes::OWNER_NOT_MEMBER,
                    format!("owner_user_id {uid} is not a member of this org"),
                    "owner_user_id",
                ));
            }
        }
    }
    for new in &mut items {
        new.default_owner(user);
    }
    let n = items.len() as i64;
    // Quantity-aware friendly pre-check; the store INSERT re-enforces the
    // same `current + n <= limit` bound atomically against a concurrent bulk.
    state.quotas.check_can_create_targets(org, None, n).await?;
    let flow_count = items
        .iter()
        .filter(|i| matches!(&i.check, CheckSpec::Flow(_)))
        .count() as i64;
    if flow_count > 0 {
        state
            .quotas
            .check_can_create_flow(org, None, flow_count)
            .await?;
    }
    // A heartbeat's ping row comes from the next scheduler refresh, so bulk
    // stays one batch.
    let mut placed = Vec::with_capacity(items.len());
    for target in items {
        let regions = ops
            .resolve_create_regions(org, &target, &plan, &snapshot)
            .await?;
        placed.push(NewTargetWithRegions { target, regions });
    }
    let out = state
        .target_store
        .bulk_create(
            org,
            placed,
            source,
            i64::from(plan.max_targets),
            i64::from(plan.max_flow_checks),
        )
        .await?;
    ops.report_initial_states(org, &out);
    Ok((StatusCode::CREATED, Redacted::new(out)))
}

#[utoipa::path(
    post,
    path = "/api/v1/targets/bulk-action",
    tag = "targets",
    summary = "Apply enable/disable/delete/tag-add/tag-remove to many targets",
    description = "Partial failure is allowed — the response lists which ids succeeded and which failed and why. Up to 10 000 ids per request.",
    request_body(content = BulkActionRequest, example = json!({
        "ids": ["01h7m8z4n6v0e1m7v7y6x8x8x8"],
        "action": {"type": "disable"}
    })),
    responses(
        (status = 200, body = BulkActionResponse, example = json!({
            "succeeded": ["01h7m8z4n6v0e1m7v7y6x8x8x8"],
            "failed": []
        })),
        (status = 400, description = "Malformed request (e.g., empty ids, unknown action)", body = ApiError),
        (status = 413, body = ApiError),
    ),
)]
pub async fn bulk_action(
    State(state): State<AppState>,
    CurrentOrg(org): CurrentOrg,
    CurrentUser(user): CurrentUser,
    scopes: TokenScopes,
    Json(req): Json<BulkActionRequest>,
) -> Result<Json<BulkActionResponse>> {
    scopes.require(match &req.action {
        BulkAction::Delete {} => Scope::TargetsDelete,
        _ => Scope::TargetsWrite,
    })?;
    if req.ids.is_empty() {
        return Err(AppError::bad_request(
            codes::BULK_EMPTY,
            "bulk-action requires at least one id",
        ));
    }
    if req.ids.len() > BULK_MAX {
        return Err(AppError::payload_too_large(
            codes::BULK_TOO_LARGE,
            format!("bulk size {} exceeds max {BULK_MAX}", req.ids.len()),
        ));
    }

    let mut over_cap: Vec<Uuid> = Vec::new();
    let succeeded = match &req.action {
        BulkAction::Enable {} => {
            state
                .target_store
                .set_enabled(org, &req.ids, true, Some(user))
                .await?
        }
        BulkAction::Disable {} => {
            state
                .target_store
                .set_enabled(org, &req.ids, false, Some(user))
                .await?
        }
        BulkAction::Delete {} => {
            // Capture curated pages before the cascade drops the join rows.
            let pages = state
                .status_page_store
                .pages_for_targets(org, &req.ids)
                .await
                .unwrap_or_default();
            let succeeded = state
                .target_store
                .delete_bulk(org, &req.ids, Some(user))
                .await?;
            for page in pages {
                state.public_source.invalidate(page).await;
            }
            note_if_emptied(&state, org, succeeded.len()).await;
            if let Ok(pool) = state.require_db() {
                crate::quotas::holds::release_after_delete(pool, &state.quotas, org).await;
            }
            succeeded
        }
        BulkAction::TagAdd { tags } => {
            if tags.is_empty() {
                return Err(AppError::bad_request_field(
                    codes::INVALID_TAG,
                    "tag_add requires at least one tag",
                    "action.tags",
                ));
            }
            let tags = normalize_tags(tags)?;
            let outcome = state.target_store.add_tags(org, &req.ids, &tags).await?;
            over_cap = outcome.over_cap;
            outcome.updated
        }
        // Not normalized: removal is how a tag that predates the rules gets
        // cleaned up, so it must accept one the write rules would reject.
        BulkAction::TagRemove { tags } => {
            if tags.is_empty() {
                return Err(AppError::bad_request_field(
                    codes::INVALID_TAG,
                    "tag_remove requires at least one tag",
                    "action.tags",
                ));
            }
            state.target_store.remove_tags(org, &req.ids, tags).await?
        }
        BulkAction::SetGroup { group } => {
            // Trim + treat "" as clear so the wire format stays one shape
            // (omit field = no-op, send "" or null = clear).
            let normalized = group.as_deref().map(str::trim).filter(|s| !s.is_empty());
            state
                .target_store
                .set_group(org, &req.ids, normalized)
                .await?
        }
    };

    // Sets, not scans: `ids` runs to BULK_MAX and both lookups are per id.
    let done: std::collections::HashSet<Uuid> = succeeded.iter().copied().collect();
    let over_cap: std::collections::HashSet<Uuid> = over_cap.into_iter().collect();
    let failed: Vec<BulkActionFailure> = req
        .ids
        .iter()
        .filter(|id| !done.contains(id))
        .map(|id| {
            if over_cap.contains(id) {
                BulkActionFailure {
                    id: *id,
                    code: codes::TOO_MANY_TAGS,
                    message: format!(
                        "adding these would take it past the {} tag limit",
                        crate::domain::target::MAX_TAGS_PER_TARGET
                    ),
                }
            } else {
                BulkActionFailure {
                    id: *id,
                    code: codes::TARGET_NOT_FOUND,
                    message: "target not found".into(),
                }
            }
        })
        .collect();

    Ok(Json(BulkActionResponse { succeeded, failed }))
}

#[utoipa::path(
    post,
    path = "/api/v1/targets/test",
    tag = "targets",
    summary = "Run a one-shot check against a CheckSpec without persisting anything",
    description = "Used by the UI's 'Test now' button on create/edit forms. Runs through the same validation (SSRF, schema) as create. Result is not stored.",
    request_body(content = TestRequest, example = json!({
        "check": {
            "type": "http",
            "url": "https://example.com/",
            "method": "GET",
            "timeout": 10000,
            "follow_redirects": true,
            "max_redirects": 5,
            "expected_status": {"kind": "exact", "value": 200},
            "headers": {},
            "verify_tls": true
        }
    })),
    responses(
        (status = 200, body = TestResponse, example = json!({
            "result": {"target_id": "00000000-0000-0000-0000-000000000000", "timestamp": "2026-05-13T12:00:00.000Z", "status": "up", "duration_ms": 142},
            "matched_expectations": true,
            "warnings": []
        })),
        (status = 400, body = ApiError),
        (status = 503, description = "No probe available; probing runs on agents", body = ApiError),
    ),
)]
pub async fn test_check(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<TargetsExecute>,
    Json(mut req): Json<TestRequest>,
) -> Result<Json<TestResponse>> {
    let ops = state.target_ops();
    let guard = ops.ssrf_guard();
    canonicalize_check(&mut req.check)?;
    validate_check(&req.check, &guard)?;
    ops.check_abuse(org, &req.check)?;
    reject_passive_probe(&req.check)?;
    let requested = req.region.filter(|r| !r.trim().is_empty());
    let region = if matches!(&req.check, CheckSpec::Flow(_)) {
        let plan = state.quotas.limit_for_org(org).await?;
        gate_flow(&req.check, &plan)?;
        let prefer: Vec<String> = requested.into_iter().collect();
        ops.pick_flow_region(&prefer).await?
    } else {
        requested.unwrap_or_else(|| state.cfg.scheduler.effective_default_region().to_string())
    };
    let view = ops
        .run_ad_hoc(org, &region, DispatchKind::Test, None, req.check)
        .await?;
    let matched_expectations = matches!(view.result.status, crate::domain::CheckStatus::Up);
    Ok(Json(TestResponse {
        matched_expectations,
        result: view.result,
        warnings: Vec::new(),
        response_headers_preview: view.response_headers_preview,
        response_body_snippet: view.response_body_snippet,
        flow_evidence: view.flow_evidence,
        flow_steps: view.flow_steps,
        region: Some(region),
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/targets/{id}/check-now",
    tag = "targets",
    summary = "Run an immediate check against an existing target",
    description = "Dispatches a one-off check to an agent in the target's region and waits for the result. Uses the target's stored (un-redacted) credentials; the result IS persisted, same as a scheduled check. Returns 503 if no agent is available to run it.",
    params(
        ("id" = Uuid, Path),
    ),
    responses(
        (status = 200, body = CheckResult, example = json!({
            "target_id": "01h7m8z4n6v0e1m7v7y6x8x8x8",
            "timestamp": "2026-05-13T12:00:00.000Z",
            "status": "up",
            "duration_ms": 142,
            "response_code": 200
        })),
        (status = 404, body = ApiError),
        (status = 503, description = "No probe available; probing runs on agents", body = ApiError),
    ),
)]
pub async fn check_now(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<TargetsExecute>,
    Path(id): Path<Uuid>,
) -> Result<Json<CheckResult>> {
    let ops = state.target_ops();
    let target = state
        .target_store
        .get(org, id)
        .await?
        .ok_or_else(|| AppError::not_found(codes::TARGET_NOT_FOUND, "target not found"))?;
    Ok(Json(ops.check_now_via_dispatch(org, &target).await?))
}
