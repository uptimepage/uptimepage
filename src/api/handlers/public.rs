//! Public, unauthenticated handlers for the status page surface.
//!
//! Every handler in this module:
//!  * declares `security()` empty so OpenAPI documents the path as
//!    unauthenticated and any future in-process auth middleware that scans
//!    the doc skips it,
//!  * returns the narrow `PublicApiError` envelope on failure (never
//!    `ApiError`, which carries internal context),
//!  * relies on the routing layer to stamp the `Cache-Control:
//!    public, max-age=10, stale-while-revalidate=30` header, except on a
//!    private page, whose responses say `private, no-store` themselves,
//!  * reads the page through [`ReadablePage`], which answers 401 for a private
//!    page the request may not read.

use crate::request::json::Json;
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use utoipa::IntoParams;
use uuid::Uuid;

use crate::api::json_arc::JsonArc;
use crate::app::AppState;
use crate::domain::PageRef;
use crate::domain::{
    ComponentHistoryResponse, PublicIncident, PublicMaintenance, PublicMaintenanceList,
    PublicStatusPage,
};
use crate::error::public::{PublicApiError, PublicAppError};
use crate::i18n::Tr;
use crate::pagination::cursor::HistoryCursor;
use crate::pagination::page::{
    CursorPage, CursorPageOfPublicIncident, CursorPageOfPublicMaintenance,
};
use crate::public_status::access::Access;
use crate::public_status::badge::{component_badge, overall_badge, render_badge};
use crate::public_status::source::FeedLinks;
use crate::public_status::{IncidentListQuery, MaintenanceListQuery};
use crate::request::host::{ResolvedStatusPage, resolve_status_page};
use crate::robots::{NOINDEX, X_ROBOTS_TAG, mark_private};

/// A status page the request may read: resolved from the host, then past the
/// access gate. A private page with no session that may read it is a 401.
pub struct ReadablePage {
    pub page: PageRef,
    pub access: Access,
}

impl FromRequestParts<AppState> for ReadablePage {
    type Rejection = PublicAppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let page = resolve_status_page(&state.request_state(), &parts.headers).await?;
        match state
            .page_gate
            .check(state.db.as_ref(), page, &parts.headers)
            .await?
        {
            Access::SignInRequired(_) => Err(PublicAppError::SignInRequired),
            access => Ok(Self { page, access }),
        }
    }
}

/// What a response says beyond its body. Only an HTML page can answer a
/// crawler with a robots meta tag; a feed, badge or JSON body says it in a
/// header, and a private page's answer belongs to one visitor.
pub struct Served<T> {
    hidden: bool,
    private: bool,
    body: T,
}

impl<T> Served<T> {
    async fn new(state: &AppState, read: &ReadablePage, body: T) -> Self {
        Self {
            hidden: state.public_source.hide_from_search(read.page).await,
            private: read.access.is_private(),
            body,
        }
    }
}

impl<T: IntoResponse> IntoResponse for Served<T> {
    fn into_response(self) -> Response {
        let mut resp = self.body.into_response();
        if self.private {
            mark_private(&mut resp);
        } else if self.hidden {
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

/// A private page's feed link, for a reader that carries no cookie: a
/// viewer's own (`viewer` and `t`), or the page-password one (`t` alone),
/// which stops working when the password changes or is turned off.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct FeedQuery {
    pub viewer: Option<Uuid>,
    pub t: Option<String>,
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
    read: ReadablePage,
) -> Result<Served<JsonArc<PublicStatusPage>>, PublicAppError> {
    let page = state.public_source.page(read.page).await?;
    Ok(Served::new(&state, &read, JsonArc(page)).await)
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
    read: ReadablePage,
    Path(id): Path<Uuid>,
    Query(q): Query<HistoryQuery>,
) -> Result<Served<Json<ComponentHistoryResponse>>, PublicAppError> {
    let days = q.days.unwrap_or(90);
    let res = state
        .public_source
        .component_history(read.page, id, days)
        .await?;
    Ok(Served::new(&state, &read, Json(res)).await)
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
    read: ReadablePage,
    Query(q): Query<IncidentsQuery>,
) -> Result<Served<Json<CursorPage<PublicIncident>>>, PublicAppError> {
    let query = IncidentListQuery {
        limit: q.limit.unwrap_or(25).clamp(1, 100),
        cursor: public_cursor(q.cursor.as_deref())?,
        ongoing_only: q.ongoing_only.unwrap_or(false),
    };
    let listed = state.public_source.list_incidents(read.page, query).await?;
    Ok(Served::new(&state, &read, Json(listed)).await)
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
    read: ReadablePage,
    Path(id): Path<Uuid>,
) -> Result<Served<Json<PublicIncident>>, PublicAppError> {
    let inc = state.public_source.incident_by_id(read.page, id).await?;
    Ok(Served::new(&state, &read, Json(inc)).await)
}

#[utoipa::path(
    get,
    path = "/api/public/v1/incidents.rss",
    tag = "public_status",
    summary = "RSS 2.0 feed of public incidents",
    description = "Most recent 50 incidents from public components. Item \
                   title is the incident title; item description is its \
                   updates as entity-encoded HTML, each under its phase.",
    security(),
    params(FeedQuery),
    responses(
        (status = 200, description = "RSS 2.0 XML document",
            content_type = "application/rss+xml"),
        (status = 401, body = PublicApiError, description = "Private page: sign in, or use your own feed link"),
    ),
)]
pub async fn public_incidents_rss(
    State(state): State<AppState>,
    ResolvedStatusPage(page): ResolvedStatusPage,
    Query(feed): Query<FeedQuery>,
    headers: HeaderMap,
) -> Result<Served<Response>, PublicAppError> {
    let read = match state
        .page_gate
        .check(state.db.as_ref(), page, &headers)
        .await?
    {
        Access::SignInRequired(_) => {
            let Some(token) = feed.t.as_deref() else {
                return Err(PublicAppError::SignInRequired);
            };
            let access = match feed.viewer {
                Some(viewer) => state
                    .page_gate
                    .check_feed(
                        state.db.as_ref(),
                        page,
                        &state.page_feed_secret,
                        viewer,
                        token,
                    )
                    .await?
                    .map(Access::Viewer),
                None => state
                    .page_gate
                    .check_password_feed(state.db.as_ref(), page, &state.page_feed_secret, token)
                    .await?
                    .then_some(Access::Password { session: None }),
            };
            ReadablePage {
                page,
                access: access.ok_or(PublicAppError::SignInRequired)?,
            }
        }
        access => ReadablePage { page, access },
    };
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
    let subdomain_routes = state.cfg.tenancy.subdomain_public_routes;
    // Only a subdomain deploy resolves the page from the Host. Where every host
    // serves the same page, the Host names no better origin than the config.
    let origin = if subdomain_routes {
        crate::request::host::published_page_origin(&state.request_state(), &headers, page.page)
            .unwrap_or_else(configured)
    } else {
        configured()
    };
    let page_url = crate::public_status::urls::status_url_for(subdomain_routes, &origin);
    let links = FeedLinks {
        page: &page_url,
        origin: &origin,
    };
    let body = state.public_source.incidents_rss(page, links).await?;
    let mut resp = (StatusCode::OK, body).into_response();
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, RSS_CONTENT_TYPE);
    Ok(Served::new(&state, &read, resp).await)
}

#[utoipa::path(
    get,
    path = "/api/public/v1/maintenance",
    tag = "public_status",
    summary = "Active and upcoming maintenance windows",
    description = "Returns active windows (now between starts_at and ends_at) \
                   plus upcoming windows within the next 30 days, sorted by \
                   starts_at ascending, each with the updates posted to it. \
                   Only includes windows that affect at least one public \
                   component.",
    security(),
    responses(
        (status = 200, body = PublicMaintenanceList),
    ),
)]
pub async fn public_maintenance(
    State(state): State<AppState>,
    read: ReadablePage,
) -> Result<Served<Json<PublicMaintenanceList>>, PublicAppError> {
    let m = state.public_source.maintenance(read.page).await?;
    Ok(Served::new(&state, &read, Json(m)).await)
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PastMaintenanceQuery {
    /// Page size (default 25, max 100).
    pub limit: Option<u32>,
    /// Opaque cursor returned by the previous page's `next_cursor`. Omit
    /// to fetch the first page.
    pub cursor: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/public/v1/maintenance/past",
    tag = "public_status",
    summary = "Past maintenance windows (last 90 days, paginated)",
    description = "Lists maintenance windows that have ended and affected at \
                   least one component on the page, sorted by starts_at \
                   descending, each with its updates. Cancelled windows are \
                   never listed. Covers windows that ran in the last 90 days, \
                   so one that began earlier and ended inside them is listed; \
                   the cursor works like the incident list's.",
    security(),
    params(PastMaintenanceQuery),
    responses(
        (status = 200, body = CursorPageOfPublicMaintenance),
        (status = 400, body = PublicApiError),
    ),
)]
pub async fn public_past_maintenance(
    State(state): State<AppState>,
    read: ReadablePage,
    Query(q): Query<PastMaintenanceQuery>,
) -> Result<Served<Json<CursorPage<PublicMaintenance>>>, PublicAppError> {
    let query = MaintenanceListQuery {
        limit: q.limit.unwrap_or(25).clamp(1, 100),
        cursor: public_cursor(q.cursor.as_deref())?,
        ended_only: true,
    };
    let listed = state
        .public_source
        .list_maintenance(read.page, query)
        .await?;
    Ok(Served::new(&state, &read, Json(listed)).await)
}

/// A bad cursor is the client's mistake, answered in the narrow public
/// envelope rather than the internal `INVALID_CURSOR` error.
fn public_cursor(raw: Option<&str>) -> Result<Option<HistoryCursor>, PublicAppError> {
    raw.map(HistoryCursor::decode)
        .transpose()
        .map_err(|_| PublicAppError::BadRequest("invalid cursor"))
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
    read: ReadablePage,
    Query(q): Query<BadgeQuery>,
) -> Result<Served<Response>, PublicAppError> {
    if let Some(style) = q.style.as_deref()
        && style != "flat"
    {
        return Err(PublicAppError::BadRequest(
            "Unsupported style. Only 'flat' is recognised.",
        ));
    }

    let tr = Tr::new(state.public_source.locale(read.page).await);
    let page = state.public_source.page(read.page).await?;
    let page = &*page;
    let (label, status_text, color) = match q.component {
        Some(id) => {
            let comp = page
                .groups
                .iter()
                .flat_map(|g| &g.components)
                .find(|c| c.id == id)
                .ok_or(PublicAppError::NotFound)?;
            let (text, color) = component_badge(comp.current_status, tr);
            (comp.name.as_str(), text, color)
        }
        None => {
            let (text, color) = overall_badge(page.overall.state, tr);
            (page.site_name.as_str(), text, color)
        }
    };
    let svg = render_badge(label, &status_text, color);

    let mut resp = (StatusCode::OK, svg).into_response();
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, SVG_CONTENT_TYPE);
    Ok(Served::new(&state, &read, resp).await)
}
