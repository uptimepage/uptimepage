//! Org-management endpoints (`POST/GET/PATCH/DELETE /api/v1/orgs/...`).
//!
//! Every route here operates on an *explicit* `:id` path parameter, not on
//! `CurrentOrg`. That's intentional: a user with multiple memberships often
//! needs to act on an org other than their active one (look at someone else's
//! membership list, restore an org they soft-deleted yesterday), so binding
//! the route to the active org would be wrong. Access control is therefore
//! per-handler: each route extracts the caller and then consults
//! [`storage::orgs`] to decide whether they are a member / owner / deleter of
//! the path-id org. Reads use [`CurrentUser`]; mutations use [`BrowserUser`],
//! so an API token cannot create, rename, delete, or change membership of an
//! org — those are browser-session operations. A token that reaches a mutation
//! gets a `SESSION_REQUIRED` 401 (distinct from `UNAUTHORIZED`) so it reads as
//! "use a browser session", not "your token is bad".

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::AppendHeaders;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::api::ApiError;
use crate::api::error::codes;
use crate::app::AppState;
use crate::domain::{OrgId, Organization, Role, UserId, validate_slug};
use crate::error::{AppError, Result};
use crate::storage::orgs as orgs_store;
use crate::web::{BrowserUser, CurrentUser};

// ── DTOs ────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateOrgRequest {
    /// 3-30 chars, `[a-z0-9-]`, leading letter, no trailing hyphen, no double
    /// hyphens, not in the reserved list.
    pub slug: String,
    pub name: String,
}

/// Partial-update payload. Fields are optional; an absent field leaves the
/// stored value alone. A request with neither field set is rejected so the
/// caller learns the no-op was unintentional rather than silently 200-ing.
#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct UpdateOrgRequest {
    #[serde(default)]
    pub name: Option<String>,
    /// New URL slug. Same shape rules as create (`validate_slug`). Renaming
    /// is a hard cutover: the old slug becomes free for any new org (no
    /// alias / redirect is kept).
    #[serde(default)]
    pub slug: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OrgView {
    pub id: OrgId,
    pub slug: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<DateTime<Utc>>,
    /// Caller's role on this org; omitted on payloads where the caller is not
    /// guaranteed to be a member (e.g. the deleted-orgs list).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
}

impl From<Organization> for OrgView {
    fn from(o: Organization) -> Self {
        Self {
            id: o.id,
            slug: o.slug,
            name: o.name,
            created_at: o.created_at,
            updated_at: o.updated_at,
            deleted_at: o.deleted_at,
            role: None,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MemberView {
    pub user_id: UserId,
    pub email: String,
    pub role: Role,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CheckSlugResponse {
    pub available: bool,
    /// When `available = false`, why — `taken`, `invalid`, etc. Keeps the
    /// signup form able to show a precise message without a separate validate
    /// endpoint.
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CheckSlugQuery {
    pub slug: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct SwitchActiveOrgRequest {
    pub org_id: OrgId,
}

// ── Handlers ────────────────────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/api/v1/orgs",
    tag = "orgs",
    summary = "Create an organisation owned by the caller",
    request_body = CreateOrgRequest,
    responses(
        (status = 201, body = OrgView,
            headers(("Location" = String))),
        (status = 400, body = ApiError,
            description = "Slug failed validation (charset, length, reserved)"),
        (status = 409, body = ApiError,
            description = "Slug already taken (including by a soft-deleted org)"),
        (status = 422, body = ApiError,
            description = "Account is at its plan's org limit"),
    ),
)]
pub async fn create_org(
    State(state): State<AppState>,
    BrowserUser(CurrentUser(user)): BrowserUser,
    Json(req): Json<CreateOrgRequest>,
) -> Result<(
    StatusCode,
    AppendHeaders<[(axum::http::HeaderName, HeaderValue); 1]>,
    Json<OrgView>,
)> {
    let pool = require_db(&state)?;
    let slug = normalize_slug(&req.slug)?;
    let name = trim_name(&req.name)?;

    let Some(org) = orgs_store::create_org_with_owner(pool, user, &slug, &name).await? else {
        return Err(AppError::conflict(
            codes::SLUG_TAKEN,
            "slug is already in use",
        ));
    };
    let mut view: OrgView = org.into();
    view.role = Some(Role::Owner);
    let location =
        HeaderValue::from_str(&format!("/api/v1/orgs/{}", view.id)).expect("uuid is ascii");
    Ok((
        StatusCode::CREATED,
        AppendHeaders([(header::LOCATION, location)]),
        Json(view),
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/orgs",
    tag = "orgs",
    summary = "List the caller's active organisations",
    description = "Returns active (non-soft-deleted) orgs the caller is a \
                   member of. Soft-deleted orgs the caller has restore rights \
                   on live at `/api/v1/me/deleted-orgs`.",
    responses(
        (status = 200, body = Vec<OrgView>),
        (status = 401, body = ApiError),
    ),
)]
pub async fn list_my_orgs(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> Result<Json<Vec<OrgView>>> {
    let pool = require_db(&state)?;
    let rows = orgs_store::list_orgs_for_user(pool, user).await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| {
                let mut v: OrgView = r.org.into();
                v.role = Some(r.role);
                v
            })
            .collect(),
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/me/deleted-orgs",
    tag = "orgs",
    summary = "List the caller's soft-deleted organisations eligible for restore",
    description = "Orgs the caller was the deleter of, still inside the \
                   restore grace window. Past the window the org is purged and \
                   no longer appears.",
    responses(
        (status = 200, body = Vec<OrgView>),
        (status = 401, body = ApiError),
    ),
)]
pub async fn list_my_deleted_orgs(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> Result<Json<Vec<OrgView>>> {
    let pool = require_db(&state)?;
    let rows = orgs_store::list_deleted_orgs_deleted_by(pool, user).await?;
    Ok(Json(rows.into_iter().map(OrgView::from).collect()))
}

#[utoipa::path(
    get,
    path = "/api/v1/orgs/{id}",
    tag = "orgs",
    summary = "Get one organisation (member-only)",
    params(("id" = Uuid, Path)),
    responses(
        (status = 200, body = OrgView),
        (status = 401, body = ApiError),
        (status = 404, body = ApiError),
    ),
)]
pub async fn get_org(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
) -> Result<Json<OrgView>> {
    let pool = require_db(&state)?;
    let org_id = OrgId(id);
    let active = orgs_store::is_active_member(pool, user, org_id).await?;
    if !active {
        return Err(AppError::not_found(
            codes::ORG_NOT_FOUND,
            "organisation not found",
        ));
    }
    let org = orgs_store::get_org(pool, org_id)
        .await?
        .ok_or_else(|| AppError::not_found(codes::ORG_NOT_FOUND, "organisation not found"))?;
    Ok(Json(org.into()))
}

#[utoipa::path(
    patch,
    path = "/api/v1/orgs/{id}",
    tag = "orgs",
    summary = "Update an organisation (owner-only)",
    description = "Partial update: any subset of `name` and `slug`. Renaming \
                   the slug changes the public status-page URL (hard cutover \
                   — the old slug becomes free, no redirect is kept).",
    params(("id" = Uuid, Path)),
    request_body = UpdateOrgRequest,
    responses(
        (status = 200, body = OrgView),
        (status = 400, body = ApiError,
            description = "No mutable field provided, name blank, or slug failed validation"),
        (status = 401, body = ApiError),
        (status = 403, body = ApiError),
        (status = 404, body = ApiError),
        (status = 409, body = ApiError, description = "Slug is already in use"),
    ),
)]
pub async fn update_org(
    State(state): State<AppState>,
    BrowserUser(CurrentUser(user)): BrowserUser,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateOrgRequest>,
) -> Result<Json<OrgView>> {
    let pool = require_db(&state)?;
    let org_id = OrgId(id);

    let new_name = req.name.as_deref().map(trim_name).transpose()?;
    let new_slug = req.slug.as_deref().map(normalize_slug).transpose()?;
    if new_name.is_none() && new_slug.is_none() {
        return Err(AppError::bad_request(
            codes::EMPTY_PATCH,
            "patch must include at least one of `name` or `slug`",
        ));
    }

    require_owner(pool, user, org_id).await?;

    let outcome =
        orgs_store::update_org_fields(pool, org_id, user, new_name.as_deref(), new_slug.as_deref())
            .await?;
    let org = match outcome {
        orgs_store::UpdateOrgOutcome::Updated(o) => o,
        orgs_store::UpdateOrgOutcome::NotFound => {
            return Err(AppError::not_found(
                codes::ORG_NOT_FOUND,
                "organisation not found",
            ));
        }
        orgs_store::UpdateOrgOutcome::SlugTaken => {
            return Err(AppError::conflict(
                codes::SLUG_TAKEN,
                "slug is already in use",
            ));
        }
    };

    Ok(Json(org.into()))
}

#[utoipa::path(
    delete,
    path = "/api/v1/orgs/{id}",
    tag = "orgs",
    summary = "Soft-delete an organisation (owner-only)",
    description = "The row stays in place for the configured grace period so \
                   the slug remains held and the deleter can restore. After \
                   the grace period a daily worker purges it for good. Live \
                   sessions pointing at the org are moved to their user's \
                   oldest surviving one.",
    params(("id" = Uuid, Path)),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, body = ApiError),
        (status = 403, body = ApiError),
        (status = 404, body = ApiError),
        (status = 422, body = ApiError, description = "Caller's only organisation"),
    ),
)]
pub async fn delete_org(
    State(state): State<AppState>,
    BrowserUser(CurrentUser(user)): BrowserUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode> {
    let pool = require_db(&state)?;
    let org_id = OrgId(id);
    require_owner(pool, user, org_id).await?;
    match orgs_store::soft_delete_org_for_user(pool, org_id, user).await? {
        orgs_store::DeleteOutcome::Deleted => Ok(StatusCode::NO_CONTENT),
        orgs_store::DeleteOutcome::LastOrg => Err(AppError::unprocessable(
            codes::LAST_ORG,
            "deleting your only organisation would leave your account with no \
             organisation to sign in to",
        )),
        orgs_store::DeleteOutcome::NotFound => Err(AppError::not_found(
            codes::ORG_NOT_FOUND,
            "organisation not found",
        )),
    }
}

#[utoipa::path(
    post,
    path = "/api/v1/orgs/{id}/restore",
    tag = "orgs",
    summary = "Restore a soft-deleted organisation (deleter-only, within grace window)",
    params(("id" = Uuid, Path)),
    responses(
        (status = 200, body = OrgView),
        (status = 401, body = ApiError),
        (status = 404, body = ApiError,
            description = "Org doesn't exist, isn't soft-deleted, or caller \
                           isn't the deleter (cloaked as 404 — never confirm \
                           existence to a non-deleter)"),
        (status = 409, body = ApiError,
            description = "Slug was claimed by another org while this one was deleted"),
        (status = 422, body = ApiError,
            description = "Past the restore grace window, or the org's rows would put \
                           the account over a pooled cap"),
    ),
)]
pub async fn restore_org(
    State(state): State<AppState>,
    BrowserUser(CurrentUser(user)): BrowserUser,
    Path(id): Path<Uuid>,
) -> Result<Json<OrgView>> {
    let pool = require_db(&state)?;
    let org_id = OrgId(id);
    let grace = state.cfg.tenancy.deletion_grace_period_days;
    // The storage call performs the deleter check + grace check + UPDATE in
    // one transaction and returns the restored row, so the handler doesn't
    // race with a concurrent re-delete or get a stale read.
    // The org is still tombstoned, so its account resolves through it and the
    // plan carries the caps its rows are about to be counted against.
    let plan = state.quotas.limit_for_org(org_id).await?;
    match orgs_store::restore_org(pool, org_id, user, grace, &plan).await? {
        orgs_store::RestoreOutcome::Restored(org) => Ok(Json(org.into())),
        orgs_store::RestoreOutcome::NotFound | orgs_store::RestoreOutcome::NotDeleted => Err(
            AppError::not_found(codes::ORG_NOT_FOUND, "organisation not found"),
        ),
        orgs_store::RestoreOutcome::OwnerLimit => Err(AppError::unprocessable(
            codes::OWNER_ORG_LIMIT,
            "restoring would put this account over its plan's organisation limit",
        )),
        orgs_store::RestoreOutcome::QuotaExceeded {
            quota,
            current,
            limit,
        } => Err(AppError::quota_exceeded(
            quota,
            current,
            limit,
            plan.id.clone(),
        )),
        orgs_store::RestoreOutcome::SlugTaken => Err(AppError::conflict(
            codes::SLUG_TAKEN,
            "another organisation took this slug while it was deleted",
        )),
        orgs_store::RestoreOutcome::WindowExpired => Err(AppError::unprocessable(
            codes::RESTORE_WINDOW_EXPIRED,
            "restore window has expired",
        )),
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/orgs/check-slug",
    tag = "orgs",
    summary = "Check whether a slug is available for signup",
    params(CheckSlugQuery),
    responses(
        (status = 200, body = CheckSlugResponse),
    ),
)]
pub async fn check_slug(
    State(state): State<AppState>,
    Query(q): Query<CheckSlugQuery>,
) -> Result<Json<CheckSlugResponse>> {
    let pool = require_db(&state)?;
    let normalised = q.slug.trim().to_ascii_lowercase();
    if let Err(e) = validate_slug(&normalised) {
        return Ok(Json(CheckSlugResponse {
            available: false,
            reason: Some(e.to_string()),
        }));
    }
    let free = orgs_store::slug_is_available(pool, &normalised).await?;
    Ok(Json(CheckSlugResponse {
        available: free,
        reason: (!free).then(|| "slug is already in use".into()),
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/orgs/{id}/members",
    tag = "orgs",
    summary = "List the members of an organisation (owner-only)",
    params(("id" = Uuid, Path)),
    responses(
        (status = 200, body = Vec<MemberView>),
        (status = 401, body = ApiError),
        (status = 403, body = ApiError),
        (status = 404, body = ApiError),
    ),
)]
pub async fn list_org_members(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<MemberView>>> {
    let pool = require_db(&state)?;
    let org_id = OrgId(id);
    require_owner(pool, user, org_id).await?;
    let members = orgs_store::list_members(pool, org_id).await?;
    Ok(Json(
        members
            .into_iter()
            .map(|m| MemberView {
                user_id: m.membership.user_id,
                email: m.email,
                role: m.membership.role,
                created_at: m.membership.created_at,
            })
            .collect(),
    ))
}

#[utoipa::path(
    delete,
    path = "/api/v1/orgs/{id}/members/{user_id}",
    tag = "orgs",
    summary = "Remove a member from an organisation (owner-only)",
    description = "Refuses to remove the org's only owner; the caller must \
                   demote, transfer, or delete the org instead.",
    params(("id" = Uuid, Path), ("user_id" = Uuid, Path)),
    responses(
        (status = 204, description = "Removed"),
        (status = 401, body = ApiError),
        (status = 403, body = ApiError),
        (status = 404, body = ApiError),
        (status = 409, body = ApiError, description = "Cannot remove the last owner, or the owner of the account the org bills to"),
    ),
)]
pub async fn remove_org_member(
    State(state): State<AppState>,
    session: crate::web::Session,
    Path((id, target_user_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode> {
    let pool = require_db(&state)?;
    // Cookie-session only (no Bearer): member management is a browser action,
    // and the self-leave path below rotates the active org on this session.
    let Some(user) = session.user_id() else {
        return Err(AppError::Unauthorized);
    };
    let org_id = OrgId(id);
    let target = UserId(target_user_id);
    require_owner(pool, user, org_id).await?;
    match orgs_store::remove_member(pool, org_id, user, target).await? {
        orgs_store::RemoveOutcome::Removed => {
            // Leaving yourself: the session's active org just vanished and
            // CurrentOrg rejects it everywhere — rotate to a surviving org
            // so the next page load works. Best-effort; no org left = the
            // org-less account state, nothing to rotate to.
            if target == user
                && session.active_org_id == Some(org_id)
                && let Some(hash) = session.session_id_hash.as_deref()
                && let Some(next) = orgs_store::oldest_membership_for_user(pool, user).await?
                && let Err(err) =
                    crate::auth::session::set_active_org_by_hash(pool, hash, next).await
            {
                tracing::warn!(error = %err, "active-org rotation after self-removal failed");
            }
            Ok(StatusCode::NO_CONTENT)
        }
        orgs_store::RemoveOutcome::NotFound => Err(AppError::not_found(
            codes::MEMBER_NOT_FOUND,
            "member not found",
        )),
        orgs_store::RemoveOutcome::LastOwner => Err(AppError::conflict(
            codes::LAST_OWNER,
            "cannot remove the last owner of an organisation",
        )),
        orgs_store::RemoveOutcome::AccountOwner => Err(AppError::conflict(
            codes::ACCOUNT_OWNER,
            "this organisation bills to that user's account; delete the organisation instead",
        )),
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateMemberRoleRequest {
    pub role: Role,
}

#[utoipa::path(
    patch,
    path = "/api/v1/orgs/{id}/members/{user_id}",
    tag = "orgs",
    summary = "Change a member's role (owner-only)",
    description = "Promote a member to owner or demote an owner to member. \
                   Refuses to demote the org's only owner.",
    params(("id" = Uuid, Path), ("user_id" = Uuid, Path)),
    request_body = UpdateMemberRoleRequest,
    responses(
        (status = 204, description = "Role updated (or already that role)"),
        (status = 401, body = ApiError),
        (status = 403, body = ApiError),
        (status = 404, body = ApiError),
        (status = 409, body = ApiError, description = "Cannot demote the last owner"),
    ),
)]
pub async fn update_org_member_role(
    State(state): State<AppState>,
    BrowserUser(CurrentUser(user)): BrowserUser,
    Path((id, target_user_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<UpdateMemberRoleRequest>,
) -> Result<StatusCode> {
    let pool = require_db(&state)?;
    let org_id = OrgId(id);
    let target = UserId(target_user_id);
    require_owner(pool, user, org_id).await?;
    match orgs_store::set_member_role(pool, org_id, user, target, req.role).await? {
        orgs_store::SetRoleOutcome::Updated | orgs_store::SetRoleOutcome::Unchanged => {
            Ok(StatusCode::NO_CONTENT)
        }
        orgs_store::SetRoleOutcome::NotFound => Err(AppError::not_found(
            codes::MEMBER_NOT_FOUND,
            "member not found",
        )),
        orgs_store::SetRoleOutcome::LastOwner => Err(AppError::conflict(
            codes::LAST_OWNER,
            "cannot demote the last owner of an organisation",
        )),
    }
}

#[utoipa::path(
    post,
    path = "/api/v1/me/active-org",
    tag = "orgs",
    summary = "Switch the caller's active organisation",
    description = "Verifies active membership in the requested org and \
                   updates the session's `active_org_id`.",
    request_body = SwitchActiveOrgRequest,
    responses(
        (status = 204, description = "Active org switched"),
        (status = 401, body = ApiError),
        (status = 403, body = ApiError),
        (status = 404, body = ApiError),
    ),
)]
pub async fn switch_active_org(
    State(state): State<AppState>,
    session: crate::web::Session,
    Json(req): Json<SwitchActiveOrgRequest>,
) -> Result<StatusCode> {
    let pool = require_db(&state)?;
    // Cookie-session only: a Bearer request carries no cookie, so `user_id`
    // and the hash are both absent and it 401s — an API token can't repoint a
    // browser's active org.
    let (Some(user), Some(hash)) = (session.user_id(), session.session_id_hash.as_deref()) else {
        return Err(AppError::Unauthorized);
    };
    if !orgs_store::is_active_member(pool, user, req.org_id).await? {
        return Err(AppError::Forbidden);
    }
    if !crate::auth::session::set_active_org_by_hash(pool, hash, req.org_id).await? {
        // Session revoked/expired between extraction and the UPDATE.
        return Err(AppError::Unauthorized);
    }
    Ok(StatusCode::NO_CONTENT)
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn require_db(state: &AppState) -> Result<&sqlx::PgPool> {
    state.db.as_ref().ok_or_else(|| {
        AppError::Other(anyhow::anyhow!(
            "org management requires a postgres pool — AppState.db is None"
        ))
    })
}

pub(crate) async fn require_owner(pool: &sqlx::PgPool, user: UserId, org: OrgId) -> Result<()> {
    // One round-trip: returns role + active-or-not. Cloak any non-active
    // membership as 404; a member who isn't an owner gets 403.
    match orgs_store::membership_status(pool, user, org).await? {
        orgs_store::MembershipStatus::Owner => Ok(()),
        orgs_store::MembershipStatus::Member => Err(AppError::Forbidden),
        orgs_store::MembershipStatus::None => Err(AppError::not_found(
            codes::ORG_NOT_FOUND,
            "organisation not found",
        )),
    }
}

const MAX_ORG_NAME: usize = 120;

/// Normalise + validate a user-supplied slug exactly the way `create_org`
/// does, so a PATCH-rejection on `slug` is indistinguishable from a
/// create-rejection. Field is always reported as `"slug"` so the form can
/// highlight the input.
fn normalize_slug(raw: &str) -> Result<String> {
    let s = raw.trim().to_ascii_lowercase();
    validate_slug(&s)
        .map_err(|e| AppError::bad_request_field(codes::SLUG_INVALID, e.to_string(), "slug"))?;
    Ok(s)
}

fn trim_name(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(AppError::bad_request_field(
            codes::EMPTY_NAME,
            "name must not be empty",
            "name",
        ));
    }
    if trimmed.chars().count() > MAX_ORG_NAME {
        return Err(AppError::bad_request_field(
            codes::TITLE_TOO_LONG,
            format!("name must be at most {MAX_ORG_NAME} characters"),
            "name",
        ));
    }
    // Reject control characters (incl. CR/LF/NUL). The org name flows into
    // the invitation email subject; a CRLF here would let a hostile owner
    // inject an SMTP header (Bcc, Reply-To) at the upstream provider's
    // boundary, even though our outgoing JSON encodes them. Defense in
    // depth at our validation layer, not at render time.
    if trimmed.chars().any(|c| c.is_control()) {
        return Err(AppError::bad_request_field(
            codes::INVALID_NAME,
            "name must not contain control characters",
            "name",
        ));
    }
    Ok(trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_invalid_name(err: AppError) {
        match err {
            AppError::BadRequest { code, .. } => assert_eq!(code, codes::INVALID_NAME),
            other => panic!("expected BadRequest(INVALID_NAME), got {other:?}"),
        }
    }

    #[test]
    fn trim_name_rejects_crlf() {
        let err = trim_name("Acme\r\nBcc: a@b").expect_err("CRLF must reject");
        assert_invalid_name(err);
    }

    #[test]
    fn trim_name_rejects_lf_only() {
        let err = trim_name("Acme\nX").expect_err("LF must reject");
        assert_invalid_name(err);
    }

    #[test]
    fn trim_name_rejects_nul() {
        let err = trim_name("Acme\0evil").expect_err("NUL must reject");
        assert_invalid_name(err);
    }

    #[test]
    fn trim_name_rejects_tab() {
        let err = trim_name("Acme\tInc").expect_err("tab must reject");
        assert_invalid_name(err);
    }

    #[test]
    fn trim_name_accepts_unicode_punctuation() {
        // Em-dash, smart quotes, accented chars — all valid names.
        trim_name("Acme — “Hello” café").expect("unicode punctuation must pass");
    }
}
