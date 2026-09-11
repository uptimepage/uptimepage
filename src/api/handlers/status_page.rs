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

use crate::api::json::Json;
use axum::extract::{Multipart, Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::api::ApiError;
use crate::api::error::codes;
use crate::app::AppState;
use crate::domain::{
    AssetSlot, NewMonitorShare, NewStatusPage, NewStatusPageComponent, OrgId, PublicOrgBranding,
    PublicStyle, StatusPage, StatusPageComponent, StatusPageComponentUpdate, StatusPageId,
    StatusPageUpdate, UserId, validate_slug,
};
use crate::error::{AppError, Result};
use crate::public_status::LogoMime;
use crate::storage::{AddComponentOutcome, CreateShareOutcome};
use crate::web::views::public_status::{
    LOGO_ROUTE, public_base, public_logo_url, public_status_url,
};
use crate::web::{
    Authorized, CurrentUser, OwnerAuthorized, RequestSource, StatusPageDelete, StatusPageRead,
    StatusPageWrite,
};

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
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreatePageRequest {
    pub slug: String,
    #[schema(max_length = 80)]
    pub name: String,
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct UpdatePageRequest {
    pub name: Option<String>,
    pub slug: Option<String>,
    pub enabled: Option<bool>,
    /// When present, replaces the page's display branding wholesale (the logo
    /// has its own endpoints, so it is untouched here).
    pub branding: Option<BrandingInput>,
}

#[derive(Debug, Deserialize, ToSchema)]
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
    Ok(Json(pages.into_iter().map(|p| view(&state, p)).collect()))
}

#[utoipa::path(
    post, path = "/api/v1/status-pages", tag = "status-pages",
    summary = "Create a status page",
    request_body = CreatePageRequest,
    responses(
        (status = 201, body = StatusPageView),
        (status = 400, body = ApiError, description = "SLUG_INVALID / name invalid"),
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
    let max = i64::from(plan.max_status_pages);
    let page = state
        .status_page_store
        .create(
            org,
            NewStatusPage {
                slug,
                name,
                enabled: req.enabled,
            },
            source,
            max,
            Some(user),
        )
        .await?
        .ok_or_else(|| AppError::quota_exceeded("max_status_pages", max, max, plan.id.clone()))?;
    Ok((StatusCode::CREATED, Json(view(&state, page))))
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
    Ok(Json(view(&state, page)))
}

#[utoipa::path(
    patch, path = "/api/v1/status-pages/{id}", tag = "status-pages",
    summary = "Update a status page (identity and/or branding)",
    params(("id" = Uuid, Path)),
    request_body = UpdatePageRequest,
    responses(
        (status = 200, body = StatusPageView),
        (status = 400, body = ApiError, description = "SLUG_INVALID / BRANDING_INVALID"),
        (status = 404, body = ApiError),
        (status = 422, body = ApiError, description = "SLUG_TAKEN"),
    ),
)]
pub async fn update_page(
    State(state): State<AppState>,
    OwnerAuthorized(org, _): OwnerAuthorized<StatusPageWrite>,
    RequestSource(source): RequestSource,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdatePageRequest>,
) -> Result<Json<StatusPageView>> {
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
            },
            source,
        )
        .await?
        .ok_or_else(page_not_found)?;
    state.public_source.invalidate(StatusPageId(id)).await;
    Ok(Json(view(&state, page)))
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
                ensure_detail_share(&state, org, StatusPageId(id), target_id, user).await?;
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
        ensure_detail_share(&state, org, StatusPageId(id), target_id, user).await?;
    }
    state.public_source.invalidate(StatusPageId(id)).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Mints only when the component has no live share, so an untick then re-tick
/// returns the same URL. Plan share caps do not apply: the page already
/// publishes this monitor.
pub(crate) async fn ensure_detail_share(
    state: &AppState,
    org: OrgId,
    page: StatusPageId,
    target_id: Uuid,
    user: UserId,
) -> Result<()> {
    let existing = state
        .status_page_store
        .list_components(org, page)
        .await?
        .into_iter()
        .find(|c| c.target_id == target_id)
        .and_then(|c| c.share_id);
    if let Some(share) = existing {
        let now = chrono::Utc::now();
        let live = state
            .monitor_share_store
            .list_for_target(org, target_id)
            .await?
            .iter()
            .any(|s| s.id == share && s.expires_at.is_none_or(|e| e > now));
        if live {
            return Ok(());
        }
    }
    let outcome = state
        .monitor_share_store
        .create(
            org,
            target_id,
            NewMonitorShare::default(),
            Some(user),
            None,
            None,
        )
        .await?;
    let CreateShareOutcome::Created(created) = outcome else {
        return Err(component_not_found());
    };
    if !state
        .status_page_store
        .attach_share(org, page, target_id, existing, created.share.id)
        .await?
    {
        // Lost the swap or the component vanished; either way another mint owns
        // the slot now, so don't strand this one as a live public URL.
        state
            .monitor_share_store
            .revoke(org, target_id, created.share.id, Some(user))
            .await?;
        return Err(component_not_found());
    }
    Ok(())
}

#[utoipa::path(
    delete, path = "/api/v1/status-pages/{id}/components/{target_id}", tag = "status-pages",
    summary = "Remove a monitor from a page",
    params(("id" = Uuid, Path), ("target_id" = Uuid, Path)),
    responses((status = 204, description = "Removed"), (status = 404, body = ApiError)),
)]
pub async fn remove_component(
    State(state): State<AppState>,
    OwnerAuthorized(org, _): OwnerAuthorized<StatusPageDelete>,
    CurrentUser(user): CurrentUser,
    Path((id, target_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode> {
    // Read the backing share before the row goes: removal drops `share_id`, and
    // an unrevoked token would keep serving the monitor with nothing left to
    // attribute it to.
    let share = state
        .status_page_store
        .list_components(org, StatusPageId(id))
        .await?
        .into_iter()
        .find(|c| c.target_id == target_id)
        .and_then(|c| c.share_id);
    if !state
        .status_page_store
        .remove_component(org, StatusPageId(id), target_id, Some(user))
        .await?
    {
        return Err(component_not_found());
    }
    if let Some(share) = share {
        state
            .monitor_share_store
            .revoke(org, target_id, share, Some(user))
            .await?;
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

pub(crate) fn validate_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 80 {
        return Err(AppError::bad_request_field(
            codes::BRANDING_INVALID,
            "name must be 1–80 characters",
            "name",
        ));
    }
    Ok(trimmed.to_owned())
}

fn view(state: &AppState, p: StatusPage) -> StatusPageView {
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
    }
}

fn normalise_opt(s: Option<String>) -> Option<String> {
    s.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

/// Trim a per-page curation field, treat blank as cleared (`None`), and bound
/// it to the DB CHECK's max so an over-long value is a 400 with the field name
/// rather than an opaque 500 from the constraint. `max` is in characters to
/// match Postgres `char_length`.
pub(crate) fn clean_curation(
    v: Option<String>,
    field: &'static str,
    max: usize,
) -> Result<Option<String>> {
    let v = normalise_opt(v);
    if let Some(ref s) = v
        && s.chars().count() > max
    {
        return Err(AppError::bad_request_field(
            codes::BRANDING_INVALID,
            format!("{field} must be at most {max} characters"),
            field,
        ));
    }
    Ok(v)
}

/// Patch-flavoured [`clean_curation`]: preserves the present/absent distinction
/// (outer `None` = leave unchanged) while normalising + validating the inner
/// value (an explicit blank or `null` clears the override).
pub(crate) fn clean_curation_patch(
    v: Option<Option<String>>,
    field: &'static str,
    max: usize,
) -> Result<Option<Option<String>>> {
    match v {
        None => Ok(None),
        Some(inner) => Ok(Some(clean_curation(inner, field, max)?)),
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
    fn normalise_blanks_to_none() {
        assert_eq!(normalise_opt(Some("  ".into())), None);
        assert_eq!(normalise_opt(Some("  hi ".into())), Some("hi".into()));
    }

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
