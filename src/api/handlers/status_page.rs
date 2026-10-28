//! Operator-side status-page management (`/api/v1/status-pages` + its
//! `/components` and `/logo` sub-resources).
//!
//! Pages are per-org; every route is scoped to the caller's active org (the
//! store rejects a page id that isn't in that org with a 404). Reads gate on
//! [`Authorized`] (any active member); every mutation gates on
//! [`OwnerAuthorized`] — the public brand surface is an owner-level asset.
//! Branding is validated through [`PublicOrgBranding::validate`] before it
//! touches the DB; the logo path is server-derived from a content hash and never
//! client-chosen.

use crate::request::json::Json;
use axum::extract::{Multipart, Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::app::AppState;
use crate::domain::{
    AccessError, AccessSpec, AccessWrite, AssetSlot, Locale, NewStatusPage, NewStatusPageComponent,
    OrgId, PageAccess, PagePasswordView, PageRef, PublicOrgBranding, PublicStyle, StatusPage,
    StatusPageComponent, StatusPageComponentUpdate, StatusPageId, StatusPageUpdate, UserId,
    validate_slug,
};
use crate::error::ApiError;
use crate::error::codes;
use crate::error::{AppError, Result};
use crate::public_status::LogoMime;
use crate::public_status::curation::{
    clean_curation, clean_curation_patch, normalise_opt, validate_name,
};
use crate::public_status::urls::{LOGO_ROUTE, public_base, public_logo_url, public_status_url};
use crate::request::{
    Authorized, CurrentUser, OwnerAuthorized, RequestSource, StatusPageDelete, StatusPageRead,
    StatusPageWrite,
};
use crate::storage::AddComponentOutcome;

// ── DTOs ────────────────────────────────────────────────────────────────────

/// One page as returned to the operator: the stored page plus its resolved
/// public URL and logo URL (computed from the current host shape).
#[derive(Debug, Serialize, ToSchema)]
pub struct StatusPageView {
    pub id: StatusPageId,
    pub slug: String,
    pub name: String,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_about: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_brand_color: Option<String>,
    pub public_style: PublicStyle,
    /// Language of the public page and its subscriber emails.
    pub public_locale: Locale,
    /// Raw override (absent = inherit the default); round-trips the tri-state
    /// that `show_powered_by` below collapses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_show_powered_by: Option<bool>,
    pub show_powered_by: bool,
    /// Keep the page out of search results.
    pub public_hide_from_search: bool,
    /// Operator's own site, linked from the page header.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_website_url: Option<String>,
    /// Versioned logo URL on the public surface, or `null` when no logo.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logo_url: Option<String>,
    /// Where the live page is reachable, or `null` when no public surface is
    /// mounted (drives the "view / preview" link).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_url: Option<String>,
    /// Who may open the page, or `null` for a page never made private.
    #[schema(nullable = true)]
    pub access: Option<AccessSpec>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreatePageRequest {
    pub slug: String,
    #[schema(max_length = 80)]
    pub name: String,
    #[serde(default)]
    pub enabled: bool,
    /// Who may open the page. Absent leaves it public.
    #[serde(default)]
    pub access: Option<AccessSpec>,
}

#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdatePageRequest {
    pub name: Option<String>,
    pub slug: Option<String>,
    pub enabled: Option<bool>,
    /// Language of the public page and its subscriber emails.
    pub public_locale: Option<Locale>,
    /// When present, replaces the page's display branding wholesale (the logo
    /// has its own endpoints, so it is untouched here).
    pub branding: Option<BrandingInput>,
    /// When present, replaces who may open the page, in the same transaction
    /// as the rest of the edit.
    pub access: Option<AccessSpec>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BrandingInput {
    #[serde(default)]
    pub public_display_name: Option<String>,
    #[serde(default)]
    pub public_about: Option<String>,
    #[serde(default)]
    pub public_brand_color: Option<String>,
    #[serde(default)]
    pub public_style: Option<PublicStyle>,
    #[serde(default)]
    pub public_show_powered_by: Option<bool>,
    /// Keep the page out of search results.
    #[serde(default)]
    pub public_hide_from_search: Option<bool>,
    /// Operator's own site, linked from the page header.
    #[serde(default)]
    pub public_website_url: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReorderRequest {
    /// Target ids in their new display order (0-based `sort_order`).
    pub target_ids: Vec<Uuid>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct LogoResponse {
    pub logo_url: String,
}

// ── Page CRUD ─────────────────────────────────────────────────────────────────

#[utoipa::path(
    get, path = "/api/v1/status-pages", tag = "status-pages",
    summary = "List the org's status pages",
    responses((status = 200, body = Vec<StatusPageView>), (status = 401, body = ApiError)),
)]
pub async fn list_pages(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<StatusPageRead>,
) -> Result<Json<Vec<StatusPageView>>> {
    let pages = state.status_page_store.list(org).await?;
    let access = match state.db.as_ref() {
        Some(pool) => crate::storage::page_access::settings::load_for_org(pool, org).await?,
        None => Default::default(),
    };
    Ok(Json(
        pages
            .into_iter()
            .map(|p| {
                let a = access.get(&p.id);
                view(&state, p, a)
            })
            .collect(),
    ))
}

#[utoipa::path(
    post, path = "/api/v1/status-pages", tag = "status-pages",
    summary = "Create a status page",
    request_body = CreatePageRequest,
    responses(
        (status = 201, body = StatusPageView),
        (status = 400, body = ApiError, description = "SLUG_INVALID / name invalid / ACCESS_INVALID / PASSWORD_REQUIRED / INVALID_PAGE_PASSWORD"),
        (status = 403, body = ApiError, description = "PRIVATE_PAGES_DISABLED: the plan does not include private pages"),
        (status = 422, body = ApiError, description = "SLUG_TAKEN or the per-org page cap reached"),
    ),
)]
pub async fn create_page(
    State(state): State<AppState>,
    OwnerAuthorized(org, _): OwnerAuthorized<StatusPageWrite>,
    CurrentUser(user): CurrentUser,
    RequestSource(source): RequestSource,
    Json(req): Json<CreatePageRequest>,
) -> Result<(StatusCode, Json<StatusPageView>)> {
    let slug = req.slug.trim().to_ascii_lowercase();
    validate_slug(&slug)
        .map_err(|e| AppError::bad_request_field(codes::SLUG_INVALID, e.to_string(), "slug"))?;
    let name = validate_name(&req.name)?;
    // Friendly pre-check (real plan, real count); the store's `None` is the
    // race backstop, mapped to the same quota error with the real plan id.
    state.quotas.check_can_create_status_page(org, None).await?;
    let plan = state.quotas.limit_for_org(org).await?;
    let access = access_write(req.access, plan.private_pages_enabled, None, user).await?;
    let max = i64::from(plan.max_status_pages);
    let page = state
        .status_page_store
        .create(
            org,
            NewStatusPage {
                slug,
                name,
                enabled: req.enabled,
                access,
            },
            source,
            max,
            Some(user),
        )
        .await?
        .ok_or_else(|| AppError::quota_exceeded("max_status_pages", max, max, plan.id.clone()))?;
    let access = page_access(&state, org, page.id).await?;
    Ok((
        StatusCode::CREATED,
        Json(view(&state, page, access.as_ref())),
    ))
}

#[utoipa::path(
    get, path = "/api/v1/status-pages/{id}", tag = "status-pages",
    summary = "Get a status page",
    params(("id" = Uuid, Path)),
    responses((status = 200, body = StatusPageView), (status = 404, body = ApiError)),
)]
pub async fn get_page(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<StatusPageRead>,
    Path(id): Path<Uuid>,
) -> Result<Json<StatusPageView>> {
    let page = load(&state, org, id).await?;
    let access = page_access(&state, org, page.id).await?;
    Ok(Json(view(&state, page, access.as_ref())))
}

#[utoipa::path(
    patch, path = "/api/v1/status-pages/{id}", tag = "status-pages",
    summary = "Update a status page (identity and/or branding)",
    params(("id" = Uuid, Path)),
    request_body = UpdatePageRequest,
    responses(
        (status = 200, body = StatusPageView),
        (status = 400, body = ApiError, description = "SLUG_INVALID / BRANDING_INVALID / ACCESS_INVALID / PASSWORD_REQUIRED (password on with none sent or stored) / INVALID_PAGE_PASSWORD (empty or over 128 characters)"),
        (status = 403, body = ApiError, description = "PRIVATE_PAGES_DISABLED: the plan does not include private pages, and the page is not private already"),
        (status = 404, body = ApiError),
        (status = 422, body = ApiError, description = "SLUG_TAKEN"),
    ),
)]
pub async fn update_page(
    State(state): State<AppState>,
    OwnerAuthorized(org, _): OwnerAuthorized<StatusPageWrite>,
    CurrentUser(user): CurrentUser,
    RequestSource(source): RequestSource,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdatePageRequest>,
) -> Result<Json<StatusPageView>> {
    let access = match req.access {
        Some(spec) => {
            let plan = state.quotas.limit_for_org(org).await?;
            let stored = page_access(&state, org, StatusPageId(id)).await?;
            access_write(
                Some(spec),
                plan.private_pages_enabled,
                stored.as_ref(),
                user,
            )
            .await?
        }
        None => None,
    };
    let opened = access.as_ref().is_some_and(|w| !w.access.is_private());
    let slug = match req.slug {
        Some(s) => {
            let s = s.trim().to_ascii_lowercase();
            validate_slug(&s).map_err(|e| {
                AppError::bad_request_field(codes::SLUG_INVALID, e.to_string(), "slug")
            })?;
            Some(s)
        }
        None => None,
    };
    let branding = match req.branding {
        Some(b) => {
            let pob = PublicOrgBranding {
                public_display_name: normalise_opt(b.public_display_name),
                public_about: normalise_opt(b.public_about),
                public_brand_color: normalise_opt(b.public_brand_color)
                    .map(|c| c.to_ascii_lowercase()),
                logo_hash: None,
                public_show_powered_by: b.public_show_powered_by,
                public_style: b.public_style.unwrap_or_default(),
                public_hide_from_search: b.public_hide_from_search.unwrap_or_default(),
                public_website_url: normalise_opt(b.public_website_url),
            };
            pob.validate().map_err(|e| {
                AppError::bad_request_field(codes::BRANDING_INVALID, e.to_string(), e.field())
            })?;
            Some(pob)
        }
        None => None,
    };
    let page = state
        .status_page_store
        .update(
            org,
            StatusPageId(id),
            StatusPageUpdate {
                name: req.name.map(|n| validate_name(&n)).transpose()?,
                slug,
                enabled: req.enabled,
                branding,
                public_locale: req.public_locale,
                access,
            },
            source,
        )
        .await?
        .ok_or_else(page_not_found)?;
    state.public_source.invalidate(StatusPageId(id)).await;
    state.page_gate.invalidate(StatusPageId(id)).await;
    // A page held because its plan lost private pages comes back once public.
    if opened && let Ok(pool) = state.require_db() {
        crate::quotas::holds::release_after_delete(pool, &state.quotas, org).await;
    }
    let access = page_access(&state, org, page.id).await?;
    Ok(Json(view(&state, page, access.as_ref())))
}

/// The access write a request asks for, validated against the plan and the
/// page's access now, with any new password hashed.
async fn access_write(
    spec: Option<AccessSpec>,
    plan_allows: bool,
    stored: Option<&PageAccess>,
    actor: UserId,
) -> Result<Option<AccessWrite>> {
    let Some(spec) = spec else { return Ok(None) };
    let valid = spec.validate(plan_allows, stored).map_err(access_error)?;
    let password_hash = match valid.password {
        Some(p) => Some(crate::security::page_password::hash(p.into_secret()).await?),
        None => None,
    };
    Ok(Some(AccessWrite {
        access: valid.access,
        password_hash,
        actor: Some(actor),
    }))
}

pub(crate) fn access_error(e: AccessError) -> AppError {
    let code = match e {
        AccessError::NotOnPlan => {
            return AppError::forbidden_code(codes::PRIVATE_PAGES_DISABLED, e.to_string());
        }
        AccessError::PasswordRequired => codes::PASSWORD_REQUIRED,
        AccessError::InvalidPassword => codes::INVALID_PAGE_PASSWORD,
        _ => codes::ACCESS_INVALID,
    };
    AppError::bad_request_field(code, e.to_string(), e.field())
}

async fn page_access(
    state: &AppState,
    org: OrgId,
    page: StatusPageId,
) -> Result<Option<PageAccess>> {
    match state.db.as_ref() {
        Some(pool) => {
            crate::storage::page_access::settings::load(pool, PageRef { page, org }).await
        }
        None => Ok(None),
    }
}

#[utoipa::path(
    delete, path = "/api/v1/status-pages/{id}", tag = "status-pages",
    summary = "Delete a status page",
    params(("id" = Uuid, Path)),
    responses((status = 204, description = "Deleted"), (status = 404, body = ApiError)),
)]
pub async fn delete_page(
    State(state): State<AppState>,
    OwnerAuthorized(org, _): OwnerAuthorized<StatusPageDelete>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode> {
    if !state
        .status_page_store
        .delete(org, StatusPageId(id), Some(user))
        .await?
    {
        return Err(page_not_found());
    }
    state.public_source.invalidate(StatusPageId(id)).await;
    if let Ok(pool) = state.require_db() {
        crate::quotas::holds::release_after_delete(pool, &state.quotas, org).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

// ── Components ────────────────────────────────────────────────────────────────

#[utoipa::path(
    get, path = "/api/v1/status-pages/{id}/components", tag = "status-pages",
    summary = "List the monitors curated onto a page",
    params(("id" = Uuid, Path)),
    responses((status = 200, body = Vec<StatusPageComponent>), (status = 404, body = ApiError)),
)]
pub async fn list_components(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<StatusPageRead>,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<StatusPageComponent>>> {
    load(&state, org, id).await?;
    let rows = state
        .status_page_store
        .list_components(org, StatusPageId(id))
        .await?;
    Ok(Json(rows))
}

#[utoipa::path(
    post, path = "/api/v1/status-pages/{id}/components", tag = "status-pages",
    summary = "Add a monitor to a page",
    params(("id" = Uuid, Path)),
    request_body = NewStatusPageComponent,
    responses(
        (status = 204, description = "Added"),
        (status = 400, body = ApiError, description = "Invalid curation field"),
        (status = 404, body = ApiError, description = "Page or target not in this org"),
        (status = 409, body = ApiError, description = "Monitor already on the page — edit via PATCH"),
        (status = 422, body = ApiError, description = "max_public_components reached"),
    ),
)]
pub async fn add_component(
    State(state): State<AppState>,
    OwnerAuthorized(org, _): OwnerAuthorized<StatusPageWrite>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
    Json(mut new): Json<NewStatusPageComponent>,
) -> Result<StatusCode> {
    new.public_name = clean_curation(new.public_name, "public_name", 80)?;
    new.public_description = clean_curation(new.public_description, "public_description", 200)?;
    new.public_group = clean_curation(new.public_group, "public_group", 50)?;
    let wants_detail_link = new.detail_link_enabled;
    let target_id = new.target_id;
    let plan = state.quotas.limit_for_org(org).await?;
    let max = i64::from(plan.max_public_components);
    match state
        .status_page_store
        .add_component(org, StatusPageId(id), new, max, Some(user))
        .await?
    {
        AddComponentOutcome::Added => {
            if wants_detail_link {
                state
                    .publishing()
                    .ensure_detail_share(org, StatusPageId(id), target_id, user)
                    .await?;
            }
            state.public_source.invalidate(StatusPageId(id)).await;
            Ok(StatusCode::NO_CONTENT)
        }
        AddComponentOutcome::AlreadyOnPage => Err(AppError::conflict(
            codes::COMPONENT_ALREADY_ON_PAGE,
            "monitor is already on this page — edit it with PATCH",
        )),
        AddComponentOutcome::OverCap { used } => Err(AppError::quota_exceeded(
            "max_public_components",
            used,
            max,
            plan.id.clone(),
        )),
    }
}

#[utoipa::path(
    patch, path = "/api/v1/status-pages/{id}/components/{target_id}", tag = "status-pages",
    summary = "Edit a component's per-page curation",
    params(("id" = Uuid, Path), ("target_id" = Uuid, Path)),
    request_body = StatusPageComponentUpdate,
    responses(
        (status = 204, description = "Updated"),
        (status = 400, body = ApiError, description = "Invalid curation field"),
        (status = 404, body = ApiError),
    ),
)]
pub async fn update_component(
    State(state): State<AppState>,
    OwnerAuthorized(org, _): OwnerAuthorized<StatusPageWrite>,
    CurrentUser(user): CurrentUser,
    Path((id, target_id)): Path<(Uuid, Uuid)>,
    Json(mut upd): Json<StatusPageComponentUpdate>,
) -> Result<StatusCode> {
    upd.public_name = clean_curation_patch(upd.public_name, "public_name", 80)?;
    upd.public_description =
        clean_curation_patch(upd.public_description, "public_description", 200)?;
    upd.public_group = clean_curation_patch(upd.public_group, "public_group", 50)?;
    let wants_detail_link = upd.detail_link_enabled == Some(true);
    if !state
        .status_page_store
        .update_component(org, StatusPageId(id), target_id, upd)
        .await?
    {
        return Err(component_not_found());
    }
    if wants_detail_link {
        state
            .publishing()
            .ensure_detail_share(org, StatusPageId(id), target_id, user)
            .await?;
    }
    state.public_source.invalidate(StatusPageId(id)).await;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    delete, path = "/api/v1/status-pages/{id}/components/{target_id}", tag = "status-pages",
    summary = "Remove a monitor from a page",
    params(("id" = Uuid, Path), ("target_id" = Uuid, Path)),
    responses(
        (status = 204, description = "Removed"),
        (status = 404, body = ApiError),
        (status = 409, body = ApiError, description = "SECTION_MEMBER: shown through its group's section; take it out of the group or remove the section"),
    ),
)]
pub async fn remove_component(
    State(state): State<AppState>,
    OwnerAuthorized(org, _): OwnerAuthorized<StatusPageDelete>,
    CurrentUser(user): CurrentUser,
    Path((id, target_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode> {
    if !state
        .status_page_store
        .remove_component(org, StatusPageId(id), target_id, Some(user))
        .await?
    {
        return Err(component_not_found());
    }
    state.public_source.invalidate(StatusPageId(id)).await;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    delete,
    path = "/api/v1/status-pages/{id}/subscribers/{subscriber_id}",
    tag = "status-pages",
    summary = "Remove a subscriber from a page",
    params(("id" = Uuid, Path), ("subscriber_id" = Uuid, Path)),
    responses((status = 204, description = "Removed"), (status = 404, body = ApiError)),
)]
pub async fn remove_subscriber(
    State(state): State<AppState>,
    OwnerAuthorized(org, _): OwnerAuthorized<StatusPageWrite>,
    Path((id, subscriber_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode> {
    let Some(pool) = state.db.as_ref() else {
        return Err(component_not_found());
    };
    if !crate::storage::subscribers::remove_for_page(pool, org.0, id, subscriber_id).await? {
        return Err(component_not_found());
    }
    Ok(StatusCode::NO_CONTENT)
}

// ── Private page viewers ─────────────────────────────────────────────────────

/// Someone who signed in to a private page.
#[derive(Debug, Serialize, ToSchema)]
pub struct ViewerView {
    pub id: Uuid,
    pub email: String,
    /// First sign-in.
    pub first_seen: chrono::DateTime<chrono::Utc>,
    #[schema(nullable = true)]
    pub last_visit: Option<chrono::DateTime<chrono::Utc>>,
    /// Set while blocked: signed out, passkeys and subscription removed, and
    /// no way back in until unblocked.
    #[schema(nullable = true)]
    pub blocked_at: Option<chrono::DateTime<chrono::Utc>>,
    pub passkeys: i64,
    /// Receives the page's updates by email.
    pub subscribed: bool,
}

/// One entry of a viewer's sign-in trail.
#[derive(Debug, Serialize, ToSchema)]
pub struct ViewerEventView {
    pub occurred_at: chrono::DateTime<chrono::Utc>,
    /// `code_sent`, `code_failed`, `signed_in`, `signed_out`, `passkey_added`,
    /// `passkey_removed`, `blocked` or `unblocked`.
    pub kind: String,
    /// `email` or `passkey`, where it applies.
    #[schema(nullable = true)]
    pub method: Option<String>,
    #[schema(nullable = true)]
    pub failure_reason: Option<String>,
    /// Who blocked or unblocked.
    #[schema(nullable = true)]
    pub actor_email: Option<String>,
}

/// The days a viewer opened the page and their sign-in trail, newest first.
/// Kept for a year.
#[derive(Debug, Serialize, ToSchema)]
pub struct ViewerHistory {
    pub visit_days: Vec<chrono::NaiveDate>,
    pub events: Vec<ViewerEventView>,
}

const HISTORY_EVENTS: i64 = 200;
const HISTORY_DAYS: i64 = 365;

#[utoipa::path(
    get, path = "/api/v1/status-pages/{id}/access/viewers", tag = "status-pages",
    summary = "List who signed in to a private page",
    params(("id" = Uuid, Path)),
    responses((status = 200, body = Vec<ViewerView>), (status = 404, body = ApiError)),
)]
pub async fn list_viewers(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<StatusPageRead>,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<ViewerView>>> {
    let page = load(&state, org, id).await?;
    let pool = state.require_db()?;
    let rows = crate::storage::page_access::viewers::list_for_page(pool, org, page.id).await?;
    Ok(Json(
        rows.into_iter()
            .map(|v| ViewerView {
                id: v.id,
                email: v.email,
                first_seen: v.created_at,
                last_visit: v.last_seen_at,
                blocked_at: v.blocked_at,
                passkeys: v.passkeys,
                subscribed: v.subscribed,
            })
            .collect(),
    ))
}

#[utoipa::path(
    post, path = "/api/v1/status-pages/{id}/access/viewers/{viewer_id}/block", tag = "status-pages",
    summary = "Block a viewer of a private page",
    description = "Signs them out at once and removes their passkeys and subscription. \
                   They cannot sign in again, whatever the list says, until unblocked.",
    params(("id" = Uuid, Path), ("viewer_id" = Uuid, Path)),
    responses((status = 204, description = "Blocked"), (status = 404, body = ApiError)),
)]
pub async fn block_viewer(
    State(state): State<AppState>,
    OwnerAuthorized(org, _): OwnerAuthorized<StatusPageWrite>,
    CurrentUser(user): CurrentUser,
    Path((id, viewer)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode> {
    set_blocked(&state, org, id, viewer, user, true).await
}

#[utoipa::path(
    post, path = "/api/v1/status-pages/{id}/access/viewers/{viewer_id}/unblock", tag = "status-pages",
    summary = "Unblock a viewer of a private page",
    description = "Lifts the block only. They sign in again by email if the list still covers them.",
    params(("id" = Uuid, Path), ("viewer_id" = Uuid, Path)),
    responses((status = 204, description = "Unblocked"), (status = 404, body = ApiError)),
)]
pub async fn unblock_viewer(
    State(state): State<AppState>,
    OwnerAuthorized(org, _): OwnerAuthorized<StatusPageWrite>,
    CurrentUser(user): CurrentUser,
    Path((id, viewer)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode> {
    set_blocked(&state, org, id, viewer, user, false).await
}

async fn set_blocked(
    state: &AppState,
    org: OrgId,
    page: Uuid,
    viewer: Uuid,
    actor: UserId,
    block: bool,
) -> Result<StatusCode> {
    use crate::storage::page_access::viewers::{self, BlockOutcome};
    let page = load(state, org, page).await?;
    let pool = state.require_db()?;
    let outcome = if block {
        viewers::block(pool, org, page.id, viewer, actor).await?
    } else {
        viewers::unblock(pool, org, page.id, viewer, actor).await?
    };
    if outcome == BlockOutcome::NotFound {
        return Err(AppError::not_found(
            codes::VIEWER_NOT_FOUND,
            "no such viewer on this page",
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get, path = "/api/v1/status-pages/{id}/access/viewers/{viewer_id}/history", tag = "status-pages",
    summary = "A viewer's visit days and sign-in trail",
    params(("id" = Uuid, Path), ("viewer_id" = Uuid, Path)),
    responses((status = 200, body = ViewerHistory), (status = 404, body = ApiError)),
)]
pub async fn viewer_history(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<StatusPageRead>,
    Path((id, viewer)): Path<(Uuid, Uuid)>,
) -> Result<Json<ViewerHistory>> {
    let page = load(&state, org, id).await?;
    let pool = state.require_db()?;
    let page_ref = PageRef { page: page.id, org };
    if crate::storage::page_access::viewers::get(pool, page_ref, viewer)
        .await?
        .is_none()
    {
        return Err(AppError::not_found(
            codes::VIEWER_NOT_FOUND,
            "no such viewer on this page",
        ));
    }
    let since = (chrono::Utc::now() - chrono::Duration::days(HISTORY_DAYS)).date_naive();
    let (days, events) = tokio::try_join!(
        crate::storage::page_access::events::visit_days(pool, org, page.id, viewer, since),
        crate::storage::page_access::events::history(pool, org, page.id, viewer, HISTORY_EVENTS),
    )?;
    Ok(Json(ViewerHistory {
        visit_days: days,
        events: events
            .into_iter()
            .map(|e| ViewerEventView {
                occurred_at: e.occurred_at,
                kind: e.kind,
                method: e.method,
                failure_reason: e.failure_reason,
                actor_email: e.actor_email,
            })
            .collect(),
    }))
}

#[utoipa::path(
    get, path = "/api/v1/status-pages/{id}/access/password", tag = "status-pages",
    summary = "The page password's use: devices signed in, last sign-in, wrong tries",
    params(("id" = Uuid, Path)),
    responses((status = 200, body = PagePasswordView), (status = 404, body = ApiError)),
)]
pub async fn page_password(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<StatusPageRead>,
    Path(id): Path<Uuid>,
) -> Result<Json<PagePasswordView>> {
    let page = load(&state, org, id).await?;
    let pool = state.require_db()?;
    let page_ref = PageRef { page: page.id, org };
    let line = match crate::storage::page_access::settings::load(pool, page_ref).await? {
        Some(access) => {
            crate::storage::page_access::password::summary(pool, page_ref, &access).await?
        }
        None => Default::default(),
    };
    Ok(Json(line))
}

#[utoipa::path(
    post, path = "/api/v1/status-pages/{id}/access/password/sign-out", tag = "status-pages",
    summary = "Sign out every device signed in with the page password",
    description = "Ends the password sessions that exist now. The password still works, so \
                   anyone who knows it can sign in again; change it to shut them out.",
    params(("id" = Uuid, Path)),
    responses((status = 204, description = "Signed out"), (status = 404, body = ApiError)),
)]
pub async fn sign_out_password_sessions(
    State(state): State<AppState>,
    OwnerAuthorized(org, _): OwnerAuthorized<StatusPageWrite>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode> {
    let page = load(&state, org, id).await?;
    let pool = state.require_db()?;
    crate::storage::page_access::password::end_sessions(pool, org, page.id, user).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post, path = "/api/v1/status-pages/{id}/components/reorder", tag = "status-pages",
    summary = "Reorder a page's components",
    params(("id" = Uuid, Path)),
    request_body = ReorderRequest,
    responses((status = 204, description = "Reordered"), (status = 404, body = ApiError)),
)]
pub async fn reorder_components(
    State(state): State<AppState>,
    OwnerAuthorized(org, _): OwnerAuthorized<StatusPageWrite>,
    Path(id): Path<Uuid>,
    Json(req): Json<ReorderRequest>,
) -> Result<StatusCode> {
    load(&state, org, id).await?;
    state
        .status_page_store
        .reorder_components(org, StatusPageId(id), &req.target_ids)
        .await?;
    state.public_source.invalidate(StatusPageId(id)).await;
    Ok(StatusCode::NO_CONTENT)
}

// ── Logo ──────────────────────────────────────────────────────────────────────

#[utoipa::path(
    post, path = "/api/v1/status-pages/{id}/logo", tag = "status-pages",
    summary = "Upload a page logo (multipart)",
    params(("id" = Uuid, Path)),
    request_body(content = String, content_type = "multipart/form-data"),
    responses(
        (status = 200, body = LogoResponse),
        (status = 400, body = ApiError),
        (status = 404, body = ApiError),
        (status = 413, body = ApiError, description = "File exceeds the configured size limit"),
    ),
)]
pub async fn upload_logo(
    State(state): State<AppState>,
    OwnerAuthorized(org, _): OwnerAuthorized<StatusPageWrite>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
    mut multipart: Multipart,
) -> Result<Json<LogoResponse>> {
    let page = load(&state, org, id).await?;
    let raw = read_logo_field(&mut multipart).await?;
    let cfg = &state.cfg.public_status;
    if raw.len() > cfg.max_logo_size_bytes as usize {
        return Err(AppError::payload_too_large(
            codes::LOGO_TOO_LARGE,
            format!("logo exceeds {} bytes", cfg.max_logo_size_bytes),
        ));
    }
    let (mime, bytes, dims) = process_logo(&raw, cfg.max_logo_dimension_px)?;
    if bytes.len() > cfg.max_logo_size_bytes as usize {
        return Err(AppError::payload_too_large(
            codes::LOGO_TOO_LARGE,
            format!(
                "logo exceeds {} bytes after processing",
                cfg.max_logo_size_bytes
            ),
        ));
    }
    let (w, h) = dims;
    let meta = state
        .page_asset_store
        .put(
            org,
            StatusPageId(id),
            AssetSlot::Logo,
            mime.as_content_type(),
            &bytes,
            serde_json::json!({ "width": w, "height": h }),
            Some(user),
        )
        .await?;
    state.public_source.invalidate(StatusPageId(id)).await;

    let base = public_base(&state.cfg, &page.slug);
    let url = public_logo_url(base.as_deref(), &meta.content_hash)
        .unwrap_or_else(|| format!("{LOGO_ROUTE}?v={}", meta.content_hash));
    Ok(Json(LogoResponse { logo_url: url }))
}

#[utoipa::path(
    delete, path = "/api/v1/status-pages/{id}/logo", tag = "status-pages",
    summary = "Remove a page's logo",
    params(("id" = Uuid, Path)),
    responses((status = 204, description = "Removed (idempotent)"), (status = 404, body = ApiError)),
)]
pub async fn delete_logo(
    State(state): State<AppState>,
    OwnerAuthorized(org, _): OwnerAuthorized<StatusPageDelete>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode> {
    // Idempotent: no row → 204.
    let _ = load(&state, org, id).await?;
    if state
        .page_asset_store
        .delete(org, StatusPageId(id), AssetSlot::Logo, Some(user))
        .await?
    {
        state.public_source.invalidate(StatusPageId(id)).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

// ── Helpers ─────────────────────────────────────────────────────────────────

async fn load(state: &AppState, org: OrgId, id: Uuid) -> Result<StatusPage> {
    state
        .status_page_store
        .get(org, StatusPageId(id))
        .await?
        .ok_or_else(page_not_found)
}

fn page_not_found() -> AppError {
    AppError::not_found(codes::STATUS_PAGE_NOT_FOUND, "status page not found")
}

fn component_not_found() -> AppError {
    AppError::not_found(codes::STATUS_PAGE_NOT_FOUND, "component not on this page")
}

fn view(state: &AppState, p: StatusPage, access: Option<&PageAccess>) -> StatusPageView {
    let cfg = &state.cfg.public_status;
    let base = public_base(&state.cfg, &p.slug);
    let b = &p.branding;
    StatusPageView {
        id: p.id,
        slug: p.slug.clone(),
        name: p.name,
        enabled: p.enabled,
        public_display_name: b.public_display_name.clone(),
        public_about: b.public_about.clone(),
        public_brand_color: b.public_brand_color.clone(),
        public_style: b.public_style,
        public_locale: p.public_locale,
        public_show_powered_by: b.public_show_powered_by,
        show_powered_by: b.show_powered_by(cfg.default_show_powered_by),
        public_hide_from_search: b.public_hide_from_search,
        public_website_url: b.public_website_url.clone(),
        logo_url: b
            .logo_hash
            .as_deref()
            .and_then(|hash| public_logo_url(base.as_deref(), hash)),
        status_url: base
            .as_ref()
            .map(|origin| public_status_url(&state.cfg, origin)),
        access: access.map(AccessSpec::from),
    }
}

async fn read_logo_field(multipart: &mut Multipart) -> Result<Vec<u8>> {
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::bad_request(codes::LOGO_MISSING, e.to_string()))?
    {
        if field.name() == Some("file") {
            return field
                .bytes()
                .await
                .map(|b| b.to_vec())
                .map_err(|e| AppError::bad_request(codes::LOGO_MISSING, e.to_string()));
        }
    }
    Err(AppError::bad_request(
        codes::LOGO_MISSING,
        "expected a multipart field named `file`",
    ))
}

// SEAM: this is the only kind-specific processing in the otherwise-generic
// page-asset path (store + serve are kind-agnostic). When a 2nd asset slot
// needs its own validation/transform (background, favicon, custom_css upload,
// font), extract an `AssetProcessor` trait — `process(raw) -> {content_type,
// bytes, metadata}` keyed by `AssetSlot` — and make the upload route generic
// instead of branching per slot here. Not before: one kind doesn't need it.
/// Validates the image by sniffing the bytes (the declared content-type is
/// ignored), downscales to fit `max_dim` if either side is larger, and ALWAYS
/// re-encodes. Re-encoding is the security boundary: stored/served bytes are
/// freshly written image data, never the uploader's original — so a polyglot
/// (valid image + appended HTML/script) or trailing payload can't survive to
/// be served same-origin. Also drops EXIF. Returns the format, the re-encoded
/// bytes, and the final `(w, h)`.
fn process_logo(raw: &[u8], max_dim: u32) -> Result<(LogoMime, Vec<u8>, (u32, u32))> {
    let fmt = image::guess_format(raw).map_err(|_| {
        AppError::bad_request(codes::LOGO_TYPE_INVALID, "unrecognised image format")
    })?;
    let mime = LogoMime::from_image_format(fmt).ok_or_else(|| {
        AppError::bad_request(codes::LOGO_TYPE_INVALID, "logo must be PNG, JPEG, or WebP")
    })?;
    const HARD_DIM_PX: u32 = 10_000;
    const MAX_DECODE_ALLOC: u64 = 128 * 1024 * 1024;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(HARD_DIM_PX);
    limits.max_image_height = Some(HARD_DIM_PX);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(raw), fmt);
    reader.limits(limits);
    let img = reader.decode().map_err(|_| {
        AppError::bad_request(
            codes::LOGO_DECODE_FAILED,
            "could not decode image — malformed, or its dimensions exceed the limit",
        )
    })?;
    let img = if img.width() > max_dim || img.height() > max_dim {
        img.resize(max_dim, max_dim, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };
    let dims = (img.width(), img.height());
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, fmt).map_err(|_| {
        AppError::bad_request(codes::LOGO_DECODE_FAILED, "could not re-encode image")
    })?;
    Ok((mime, out.into_inner(), dims))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_logo_rejects_non_image() {
        let err = process_logo(b"<svg xmlns='http://www.w3.org/2000/svg'/>", 1200).unwrap_err();
        assert!(matches!(err, AppError::BadRequest { .. }));
    }

    #[test]
    fn process_logo_reencodes_small_png() {
        let png = image::RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 255]));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(png)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        let raw = buf.into_inner();
        // Even an in-bounds image is re-encoded (the security boundary): the
        // output is a freshly written PNG of the same dims, decodable, not the
        // original bytes verbatim.
        let (mime, out, dims) = process_logo(&raw, 1200).unwrap();
        assert_eq!(mime, LogoMime::Png);
        assert_eq!(dims, (1, 1));
        assert_eq!(image::guess_format(&out).unwrap(), image::ImageFormat::Png);
        let decoded = image::load_from_memory(&out).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (1, 1));
    }

    /// Always-re-encode must NOT break WebP uploads: image 0.25 ships a
    /// lossless WebP encoder, but small WebP previously passed through verbatim
    /// — this guards that `write_to(WebP)` still succeeds on the re-encode path.
    #[test]
    fn process_logo_reencodes_webp() {
        let rgba = image::RgbaImage::from_pixel(3, 3, image::Rgba([4, 5, 6, 255]));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(rgba)
            .write_to(&mut buf, image::ImageFormat::WebP)
            .unwrap();
        let raw = buf.into_inner();
        let (mime, out, dims) = process_logo(&raw, 1200).unwrap();
        assert_eq!(mime, LogoMime::Webp);
        assert_eq!(dims, (3, 3));
        assert_eq!(image::guess_format(&out).unwrap(), image::ImageFormat::WebP);
        image::load_from_memory(&out).unwrap();
    }

    /// A valid-PNG-plus-appended-bytes polyglot: re-encoding must strip the
    /// trailing payload, so the stored bytes are pure image data.
    #[test]
    fn process_logo_strips_appended_payload() {
        let png = image::RgbaImage::from_pixel(2, 2, image::Rgba([9, 9, 9, 255]));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(png)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        let mut raw = buf.into_inner();
        raw.extend_from_slice(b"<script>alert(1)</script>");
        let (_mime, out, _dims) = process_logo(&raw, 1200).unwrap();
        let needle = b"<script>";
        assert!(
            !out.windows(needle.len()).any(|w| w == needle),
            "re-encoded output must not contain the appended payload"
        );
    }
}

// ── Sections ─────────────────────────────────────────────────────────────────

/// A monitor group to show on a page as a live section.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewPageSection {
    pub group_id: Uuid,
    /// Public heading; omit or leave blank to show the group's name.
    #[serde(default)]
    #[schema(nullable = true, max_length = 50)]
    pub heading: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PageSectionUpdate {
    /// Public heading; `null` or blank shows the group's name.
    #[schema(nullable = true, max_length = 50)]
    pub heading: Option<String>,
}

fn clean_heading(heading: Option<String>) -> Result<Option<String>> {
    clean_curation(
        heading,
        "heading",
        crate::storage::page_sections::MAX_HEADING_LEN,
    )
}

#[utoipa::path(
    get, path = "/api/v1/status-pages/{id}/sections", tag = "status-pages",
    summary = "List the monitor groups shown on a page",
    params(("id" = Uuid, Path)),
    responses(
        (status = 200, body = Vec<crate::storage::page_sections::PageSection>),
        (status = 404, body = ApiError),
    ),
)]
pub async fn list_sections(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<StatusPageRead>,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<crate::storage::page_sections::PageSection>>> {
    load(&state, org, id).await?;
    let pool = state.require_db()?;
    Ok(Json(
        crate::storage::page_sections::list(pool, org, id).await?,
    ))
}

#[utoipa::path(
    post, path = "/api/v1/status-pages/{id}/sections", tag = "status-pages",
    summary = "Show a monitor group on a page",
    description = "The section mirrors the group: every member gets a row on the page, a member already on it by hand moves into the section with its settings, and monitors that join or leave the group later join or leave the page. Members past the plan's public component limit stay off the page; `left_off` counts them.",
    params(("id" = Uuid, Path)),
    request_body = NewPageSection,
    responses(
        (status = 201, body = crate::storage::page_sections::PageSection),
        (status = 400, body = ApiError, description = "Heading too long"),
        (status = 404, body = ApiError, description = "Page or group not in this org"),
        (status = 409, body = ApiError, description = "SECTION_EXISTS: the group already has a section on this page"),
    ),
)]
pub async fn add_section(
    State(state): State<AppState>,
    OwnerAuthorized(org, _): OwnerAuthorized<StatusPageWrite>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
    Json(new): Json<NewPageSection>,
) -> Result<(StatusCode, Json<crate::storage::page_sections::PageSection>)> {
    let heading = clean_heading(new.heading)?;
    let pool = state.require_db()?;
    let plan = state.quotas.limit_for_org(org).await?;
    let (section, change) = crate::storage::page_sections::add(
        pool,
        org,
        id,
        new.group_id,
        heading,
        i64::from(plan.max_public_components),
        Some(user),
    )
    .await?;
    for page in change.pages {
        state.public_source.invalidate(StatusPageId(page)).await;
    }
    let view = crate::storage::page_sections::list(pool, org, id)
        .await?
        .into_iter()
        .find(|s| s.id == section)
        .ok_or_else(crate::storage::page_sections::section_not_found)?;
    Ok((StatusCode::CREATED, Json(view)))
}

#[utoipa::path(
    patch, path = "/api/v1/status-pages/{id}/sections/{section_id}", tag = "status-pages",
    summary = "Change a section's heading",
    params(("id" = Uuid, Path), ("section_id" = Uuid, Path)),
    request_body = PageSectionUpdate,
    responses((status = 204, description = "Updated"), (status = 404, body = ApiError)),
)]
pub async fn update_section(
    State(state): State<AppState>,
    OwnerAuthorized(org, _): OwnerAuthorized<StatusPageWrite>,
    CurrentUser(user): CurrentUser,
    Path((id, section)): Path<(Uuid, Uuid)>,
    Json(upd): Json<PageSectionUpdate>,
) -> Result<StatusCode> {
    let heading = clean_heading(upd.heading)?;
    let pool = state.require_db()?;
    crate::storage::page_sections::update(pool, org, id, section, heading, Some(user)).await?;
    state.public_source.invalidate(StatusPageId(id)).await;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    delete, path = "/api/v1/status-pages/{id}/sections/{section_id}", tag = "status-pages",
    summary = "Stop showing a monitor group on a page",
    description = "Removes the section and its members' rows; the page keeps their incident and maintenance history.",
    params(("id" = Uuid, Path), ("section_id" = Uuid, Path)),
    responses((status = 204, description = "Removed"), (status = 404, body = ApiError)),
)]
pub async fn remove_section(
    State(state): State<AppState>,
    OwnerAuthorized(org, _): OwnerAuthorized<StatusPageDelete>,
    CurrentUser(user): CurrentUser,
    Path((id, section)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode> {
    let pool = state.require_db()?;
    let change = crate::storage::page_sections::remove(pool, org, id, section, Some(user)).await?;
    for page in change.pages {
        state.public_source.invalidate(StatusPageId(page)).await;
    }
    Ok(StatusCode::NO_CONTENT)
}
