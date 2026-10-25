//! New monitors that start as an exact copy of an existing one. The copy is
//! taken from the stored row, so credentials, regions and the escalation policy
//! carry over without ever reaching the client; only each copy's name and
//! address are new input.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Deserialize;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::api::redaction::Redacted;
use crate::app::AppState;
use crate::domain::target::DUPLICATE_MAX;
use crate::domain::{CheckSpec, NewTarget, OrgId, PlacedTarget, Target, UserId};
use crate::error::{ApiError, AppError, Result, codes};
use crate::request::json::Json;
use crate::request::{
    Authorized, CurrentUser, LadderBinding, RequestSource, TargetsWrite, TokenScopes,
    ladder_binding,
};
use crate::security::SsrfGuard;
use crate::target_ops::TargetOps;
use crate::targets::validate::{canonicalize_check, gate_flow, validate_check};

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DuplicateRequest {
    #[schema(min_items = 1, max_items = 100)]
    pub copies: Vec<DuplicateCopy>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DuplicateCopy {
    #[schema(example = "api eu")]
    pub name: String,
    /// What the copy checks: a URL for http and flow, a host or `host:port`
    /// for tcp and tls, a host for ping, a domain for dns and domain expiry.
    /// Omit to keep the source's. Heartbeat and manual monitors have none.
    #[serde(default)]
    #[schema(nullable = true, example = "https://eu.example.com/health")]
    pub address: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/v1/targets/{id}/duplicate",
    tag = "targets",
    summary = "Create monitors that copy an existing one",
    description = "Each copy takes every setting of the source monitor as stored: the check with its credentials, interval, paused or running state, tags, group, alert bindings, regions and detection threshold, and its escalation policy when the caller could bind one by hand (the org owner, on a plan with on-call, with a token allowed oncall:write). Only the name, the address and the owner differ; the caller owns every copy. A flow copy is held to the plan's flow limits like any new flow. A heartbeat copy gets its own ping URL. Status page membership, share links and history are not copied. All copies are created or none are. Accepts an `Idempotency-Key` header.",
    params(("id" = Uuid, Path, description = "The monitor to copy")),
    request_body(content = DuplicateRequest, example = json!({
        "copies": [
            {"name": "api eu", "address": "https://eu.example.com/health"},
            {"name": "api us", "address": "https://us.example.com/health"}
        ]
    })),
    responses(
        (status = 201, description = "All created, in request order", body = Vec<Target>),
        (status = 400, description = "No copies, or a copy failed validation; `field` names it as `copies[i].…`. Nothing was created", body = ApiError, example = json!({
            "error": {"code": "INVALID_ADDRESS", "message": "address must be an absolute http or https URL", "field": "copies[1].address", "details": null, "trace_id": null}
        })),
        (status = 404, body = ApiError),
        (status = 413, description = "More than 100 copies", body = ApiError),
        (status = 422, description = "The plan has no room for this many monitors", body = ApiError),
        (status = 409, body = ApiError,
            description = "A request with the same Idempotency-Key is still running \
                           (`IDEMPOTENCY_IN_PROGRESS`); retry once it answers"),
        ),
)]
pub async fn duplicate(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<TargetsWrite>,
    CurrentUser(user): CurrentUser,
    scopes: TokenScopes,
    RequestSource(write_source): RequestSource,
    Path(id): Path<Uuid>,
    Json(req): Json<DuplicateRequest>,
) -> Result<(StatusCode, Redacted<Vec<Target>>)> {
    if req.copies.is_empty() {
        return Err(AppError::bad_request(
            codes::BULK_EMPTY,
            "name at least one copy",
        ));
    }
    if req.copies.len() > DUPLICATE_MAX {
        return Err(AppError::payload_too_large(
            codes::BULK_TOO_LARGE,
            format!("{} copies exceeds max {DUPLICATE_MAX}", req.copies.len()),
        ));
    }
    let source = state
        .target_store
        .get(org, id)
        .await?
        .ok_or_else(|| AppError::not_found(codes::TARGET_NOT_FOUND, "target not found"))?;
    let plan = state.quotas.limit_for_org(org).await?;
    // A copy is a new flow, so it takes the plan's flow limits and needs an
    // agent able to run it, as a create does.
    gate_flow(&source.check, &plan)?;
    let ops = state.target_ops();
    let regions = state
        .target_store
        .regions_for_target(org, id)
        .await?
        .unwrap_or_default();
    if matches!(source.check, CheckSpec::Flow(_)) {
        ops.region_snapshot()
            .await?
            .ensure_flow_covered(&source.check, &regions)?;
    }
    let escalation_policy_id = match state.escalation_policy_store.target_policy(org, id).await? {
        Some(policy)
            if ladder_binding(&state.cfg, state.db.as_ref(), org, user, &scopes, &plan).await?
                == LadderBinding::Allowed =>
        {
            Some(policy)
        }
        _ => None,
    };
    let guard = ops.ssrf_guard();
    let mut placed = Vec::with_capacity(req.copies.len());
    for (i, copy) in req.copies.into_iter().enumerate() {
        let name = copy.name.trim().to_owned();
        let target = vet_copy(&ops, &guard, org, user, &source, copy)
            .await
            .map_err(|e| within_copy(i, &name, e))?;
        placed.push(PlacedTarget {
            target,
            regions: regions.clone(),
            escalation_policy_id,
        });
    }
    let n = placed.len() as i64;
    state.quotas.check_can_create_targets(org, None, n).await?;
    if matches!(source.check, CheckSpec::Flow(_)) {
        state.quotas.check_can_create_flow(org, None, n).await?;
    }
    let created = state
        .target_store
        .bulk_create(
            org,
            placed,
            write_source,
            i64::from(plan.max_targets),
            i64::from(plan.max_flow_checks),
        )
        .await?;
    ops.settle_created(org, &created, &regions).await;
    Ok((StatusCode::CREATED, Redacted::new(created)))
}

/// The rest was valid when the source was saved and is copied as it stands,
/// the way the plan reads it at probe time: an interval or region set a later
/// plan no longer sells is governed there, not refused here. The check itself
/// passes today's guards even when the address is unchanged, since they may
/// have tightened since the source was saved.
async fn vet_copy(
    ops: &TargetOps<'_>,
    guard: &SsrfGuard,
    org: OrgId,
    owner: UserId,
    source: &Target,
    copy: DuplicateCopy,
) -> Result<NewTarget> {
    let name = copy.name.trim();
    if name.is_empty() {
        return Err(AppError::bad_request_field(
            codes::EMPTY_NAME,
            "a copy needs a name",
            "name",
        ));
    }
    let mut new = source.duplicate(name.to_owned(), owner);
    if let Some(address) = copy.address {
        new.check.retarget(&address).map_err(|e| {
            AppError::bad_request_field(codes::INVALID_ADDRESS, e.to_string(), "address")
        })?;
    }
    canonicalize_check(&mut new.check)?;
    validate_check(&new.check, guard)?;
    ops.check_abuse(org, &new.check)?;
    ops.validate_variable_refs(org, &new.check).await?;
    Ok(new)
}

/// A field pins a refusal to its row; a refusal without one names the copy.
fn within_copy(i: usize, name: &str, err: AppError) -> AppError {
    match err {
        AppError::BadRequest {
            code,
            message,
            field,
        } => AppError::BadRequest {
            code,
            message,
            field: Some(match field {
                Some(f) => format!("copies[{i}].{f}"),
                None => format!("copies[{i}]"),
            }),
        },
        AppError::Unprocessable { code, message } => AppError::Unprocessable {
            code,
            message: format!("copy \"{name}\": {message}"),
        },
        AppError::ForbiddenCoded { code, message } => AppError::ForbiddenCoded {
            code,
            message: format!("copy \"{name}\": {message}"),
        },
        other => other,
    }
}
