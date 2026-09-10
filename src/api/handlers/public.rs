//! Public, unauthenticated handlers for the status page surface.
//!
//! Every handler in this module:
//!  * declares `security()` empty so OpenAPI documents the path as
//!    unauthenticated and any future in-process auth middleware that scans
//!    the doc skips it,
//!  * returns the narrow `PublicApiError` envelope on failure (never
//!    `ApiError`, which carries internal context),
//!  * relies on the routing layer to stamp the `Cache-Control:
//!    public, max-age=10, stale-while-revalidate=30` header.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use utoipa::IntoParams;
use uuid::Uuid;

use crate::api::cursor::IncidentCursor;
use crate::api::json_arc::JsonArc;
use crate::api::page::{CursorPage, CursorPageOfPublicIncident};
use crate::api::public_error::{PublicApiError, PublicAppError};
use crate::app::AppState;
use crate::domain::{
    ComponentHistoryResponse, PublicIncident, PublicMaintenanceList, PublicStatusPage,
};
use crate::public_status::IncidentListQuery;
use crate::public_status::badge::{component_badge, overall_badge, render_badge};
use crate::public_status::source::FeedLinks;
use crate::web::host::ResolvedStatusPage;

const NOINDEX: HeaderValue = HeaderValue::from_static("noindex");
const X_ROBOTS_TAG: HeaderName = HeaderName::from_static("x-robots-tag");

/// Carries a page's search-visibility decision onto a response. Only the HTML
/// pages can answer a crawler with a robots meta tag; every other
/// representation of the page — feed, badge, JSON — says it in this header.
pub struct Robots<T>(bool, T);

impl<T: IntoResponse> IntoResponse for Robots<T> {
    fn into_response(self) -> Response {
        let mut resp = self.1.into_response();
        if self.0 {
            resp.headers_mut().insert(X_ROBOTS_TAG, NOINDEX);
        }
        resp
    }
}

const RSS_CONTENT_TYPE: HeaderValue =
    HeaderValue::from_static("application/rss+xml; charset=utf-8");
const SVG_CONTENT_TYPE: HeaderValue = HeaderValue::from_static("image/svg+xml; charset=utf-8");

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct HistoryQuery {
    /// Number of days (1..=365, default 90).
    pub days: Option<u32>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BadgeQuery {
    /// Optional public component id. When omitted, the badge reflects the
    /// overall page state.
    pub component: Option<Uuid>,
    /// Reserved for future visual styles. Only `flat` is accepted in v1;
    /// any other value returns 400.
    pub style: Option<String>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct IncidentsQuery {
    /// Page size (default 25, max 100).
    pub limit: Option<u32>,
    /// Opaque cursor returned by the previous page's `next_cursor`. Omit
    /// to fetch the first page.
    pub cursor: Option<String>,
    /// If true, return only incidents whose `ended_at` is null.
    pub ongoing_only: Option<bool>,
}

#[utoipa::path(
    get,
    path = "/api/public/v1/status",
    tag = "public_status",
    summary = "Public status page payload",
    description = "Returns the full public-facing status snapshot: overall \
                   state, per-component current status and 90-day history, \
                   active and recent incidents, scheduled maintenance. \
                   Cached in-process for 10 seconds.",
    security(),
    responses(
        (status = 200, body = PublicStatusPage),
        (status = 429, body = PublicApiError, description = "Rate limited (Caddy edge)"),
        (status = 503, body = PublicApiError, description = "Upstream data unavailable"),
    ),
)]
pub async fn public_status(
    State(state): State<AppState>,
    ResolvedStatusPage(page): ResolvedStatusPage,
) -> Result<Robots<JsonArc<PublicStatusPage>>, PublicAppError> {
    let hidden = state.public_source.hide_from_search(page).await;
    let page = state.public_source.page(page).await?;
    Ok(Robots(hidden, JsonArc(page)))
}

#[utoipa::path(
    get,
    path = "/api/public/v1/components/{id}/history",
    tag = "public_status",
    summary = "Daily history strip for a single public component",
    description = "Returns one DayState per day for the requested window, \
                   oldest first. Caps at 365 days; defaults to 90. Returns \
                   404 if the component does not exist or is not public.",
    security(),
    params(
        ("id" = Uuid, Path, description = "Public component id"),
        HistoryQuery,
    ),
    responses(
        (status = 200, body = ComponentHistoryResponse),
        (status = 400, body = PublicApiError, description = "days out of range"),
        (status = 404, body = PublicApiError, description = "Component not public or not found"),
    ),
)]
pub async fn component_history(
    State(state): State<AppState>,
    ResolvedStatusPage(page): ResolvedStatusPage,
    Path(id): Path<Uuid>,
    Query(q): Query<HistoryQuery>,
) -> Result<Robots<Json<ComponentHistoryResponse>>, PublicAppError> {
    let days = q.days.unwrap_or(90);
    let res = state
        .public_source
        .component_history(page, id, days)
        .await?;
    let hidden = state.public_source.hide_from_search(page).await;
    Ok(Robots(hidden, Json(res)))
}

#[utoipa::path(
    get,
    path = "/api/public/v1/incidents",
    tag = "public_status",
    summary = "Public incident list (last 90 days, paginated)",
    description = "Lists incidents for components that have public_status = true. \
                   Sorted by started_at descending. Capped at 90 days lookback.",
    security(),
    params(IncidentsQuery),
    responses(
        (status = 200, body = CursorPageOfPublicIncident),
        (status = 400, body = PublicApiError),
    ),
)]
pub async fn public_incidents(
    State(state): State<AppState>,
    ResolvedStatusPage(page): ResolvedStatusPage,
    Query(q): Query<IncidentsQuery>,
) -> Result<Robots<Json<CursorPage<PublicIncident>>>, PublicAppError> {
    // Cursor decode failure surfaces as `INVALID_CURSOR`. Map the internal
    // `AppError` shape into `PublicAppError::BadRequest` so the public
    // surface keeps its narrow error envelope (no internal context leaks
    // through the unauthenticated path).
    let cursor = q
        .cursor
        .as_deref()
        .map(IncidentCursor::decode)
        .transpose()
        .map_err(|_| PublicAppError::BadRequest("invalid cursor"))?;
    let query = IncidentListQuery {
        limit: q.limit.unwrap_or(25).clamp(1, 100),
        cursor,
        ongoing_only: q.ongoing_only.unwrap_or(false),
    };
    let hidden = state.public_source.hide_from_search(page).await;
    let listed = state.public_source.list_incidents(page, query).await?;
    Ok(Robots(hidden, Json(listed)))
}

#[utoipa::path(
    get,
    path = "/api/public/v1/incidents/{id}",
    tag = "public_status",
    summary = "Single public incident with full update timeline",
    description = "Returns the incident only if its component has \
                   public_status = true at request time. Returns 404 \
                   otherwise — never confirms existence of non-public incidents.",
    security(),
    params(("id" = Uuid, Path)),
    responses(
        (status = 200, body = PublicIncident),
        (status = 404, body = PublicApiError,
            description = "Incident not found OR component not public"),
    ),
)]
pub async fn public_incident(
    State(state): State<AppState>,
    ResolvedStatusPage(page): ResolvedStatusPage,
    Path(id): Path<Uuid>,
) -> Result<Robots<Json<PublicIncident>>, PublicAppError> {
    let hidden = state.public_source.hide_from_search(page).await;
    let inc = state.public_source.incident_by_id(page, id).await?;
    Ok(Robots(hidden, Json(inc)))
}

#[utoipa::path(
    get,
    path = "/api/public/v1/incidents.rss",
    tag = "public_status",
    summary = "RSS 2.0 feed of public incidents",
    description = "Most recent 50 incidents from public components. Item \
                   title is the incident title; item description is the \
                   concatenated update messages.",
    security(),
    responses(
        (status = 200, description = "RSS 2.0 XML document",
            content_type = "application/rss+xml"),
    ),
)]
pub async fn public_incidents_rss(
    State(state): State<AppState>,
    ResolvedStatusPage(page): ResolvedStatusPage,
    headers: HeaderMap,
) -> Result<Robots<Response>, PublicAppError> {
    // A reader keeps these links and follows them days later, so they have to
    // be the address it fetched the feed from, not whatever the process bound.
    let configured = || {
        state
            .cfg
            .auth
            .public_base_url
            .trim_end_matches('/')
            .to_owned()
    };
    let subdomain_routes = crate::api::subdomain_public_routes_enabled(&state.cfg);
    // Only a subdomain deploy resolves the page from the Host. Where every host
    // serves the same page, the Host names no better origin than the config.
    let origin = if subdomain_routes {
        crate::web::host::request_origin(&headers, &state.cfg.public_status.base_domain)
            .unwrap_or_else(configured)
    } else {
        configured()
    };
    let page_url = crate::web::views::public_status::status_url_for(subdomain_routes, &origin);
    let links = FeedLinks {
        page: &page_url,
        origin: &origin,
    };
    let hidden = state.public_source.hide_from_search(page).await;
    let body = state.public_source.incidents_rss(page, links).await?;
    let mut resp = (StatusCode::OK, body).into_response();
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, RSS_CONTENT_TYPE);
    Ok(Robots(hidden, resp))
}

#[utoipa::path(
    get,
    path = "/api/public/v1/maintenance",
    tag = "public_status",
    summary = "Active and upcoming maintenance windows",
    description = "Returns active windows (now between starts_at and ends_at) \
                   plus upcoming windows within the next 7 days, sorted by \
                   starts_at ascending. Only includes windows that affect \
                   at least one public component.",
    security(),
    responses(
        (status = 200, body = PublicMaintenanceList),
    ),
)]
pub async fn public_maintenance(
    State(state): State<AppState>,
    ResolvedStatusPage(page): ResolvedStatusPage,
) -> Result<Robots<Json<PublicMaintenanceList>>, PublicAppError> {
    let hidden = state.public_source.hide_from_search(page).await;
    let m = state.public_source.maintenance(page).await?;
    Ok(Robots(hidden, Json(m)))
}

#[utoipa::path(
    get,
    path = "/api/public/v1/badge.svg",
    tag = "public_status",
    summary = "Embeddable SVG status badge",
    description = "Returns a shields.io-style flat SVG badge for the overall \
                   page or a single public component. Reuses the cached page \
                   payload, so the badge is consistent with the /status view \
                   within the 10-second cache window. Returns 404 if the \
                   `component` id is unknown or its target is not public.",
    security(),
    params(BadgeQuery),
    responses(
        (status = 200, description = "SVG document",
            content_type = "image/svg+xml"),
        (status = 400, body = PublicApiError, description = "Unsupported style"),
        (status = 404, body = PublicApiError, description = "Component not public or not found"),
        (status = 503, body = PublicApiError, description = "Upstream data unavailable"),
    ),
)]
pub async fn public_badge(
    State(state): State<AppState>,
    ResolvedStatusPage(page): ResolvedStatusPage,
    Query(q): Query<BadgeQuery>,
) -> Result<Robots<Response>, PublicAppError> {
    if let Some(style) = q.style.as_deref()
        && style != "flat"
    {
        return Err(PublicAppError::BadRequest(
            "Unsupported style. Only 'flat' is recognised.",
        ));
    }

    let hidden = state.public_source.hide_from_search(page).await;
    let page = state.public_source.page(page).await?;
    let page = &*page;
    let (label, status_text, color) = match q.component {
        Some(id) => {
            let comp = page
                .groups
                .iter()
                .flat_map(|g| &g.components)
                .find(|c| c.id == id)
                .ok_or(PublicAppError::NotFound)?;
            let (text, color) = component_badge(comp.current_status);
            (comp.name.as_str(), text, color)
        }
        None => {
            let (text, color) = overall_badge(page.overall.state);
            (page.site_name.as_str(), text, color)
        }
    };
    let svg = render_badge(label, status_text, color);

    let mut resp = (StatusCode::OK, svg).into_response();
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, SVG_CONTENT_TYPE);
    Ok(Robots(hidden, resp))
}
