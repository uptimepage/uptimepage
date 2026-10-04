//! Server-rendered public status page: the routes and the pages they return.
//!
//! Reads from the same `PublicSource` (and therefore the same in-process
//! cache) as the JSON endpoint, so a JSON and HTML request landing in the
//! same 10s window share one aggregator run.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use uuid::Uuid;

use crate::app::AppState;
use crate::domain::AssetSlot;
use crate::domain::PublicIncident;
use crate::domain::StatusPageId;
use crate::error::public::PublicAppError;
use crate::i18n::Tr;
use crate::request::host::{
    is_subdomain_public_request, published_page_origin, resolve_status_page,
};
use crate::templates::filters;
use crate::web::error::{NotFoundPage, UnavailablePage};
use crate::web::robots;

mod branding;
mod og;
#[cfg(test)]
mod tests;
mod view;

pub use branding::{
    BrandingView, render_about, resolve_branding, safe_brand_color, safe_brand_text_for,
};
pub use og::OgMeta;
pub use view::{
    ComponentView, DayCell, GroupView, IncidentDetailView, IncidentHeader, IncidentSummary,
    IncidentUpdateView, MaintenanceView, RSS_URL, StatusView,
};

use og::build_og_meta;
use view::{build_incident_summary, build_view};

/// Default page size for the archive view. Small enough that each render is
/// snappy on the unauthenticated, edge-cached path; the keyset cursor walks
/// older pages on demand.
const ARCHIVE_PAGE_SIZE: u32 = 25;

#[derive(Debug, Default, Deserialize)]
pub struct StatusParams {
    /// HTMX partial swap — return just the refresh region.
    pub fragment: Option<u8>,
}

#[derive(Template, WebTemplate)]
#[template(path = "public/status.html")]
pub struct StatusFullPage {
    pub tr: Tr,
    pub view: StatusView,
    pub branding: BrandingView,
    pub og: OgMeta,
}

#[derive(Template, WebTemplate)]
#[template(path = "public/region.html")]
pub struct StatusRegion {
    pub tr: Tr,
    pub view: StatusView,
}

#[derive(Template, WebTemplate)]
#[template(path = "public/incident.html")]
pub struct IncidentDetailPage {
    pub tr: Tr,
    pub branding: BrandingView,
    pub incident: IncidentDetailView,
    pub generated_at: DateTime<Utc>,
    pub rss_url: &'static str,
    pub og: OgMeta,
}

#[derive(Template, WebTemplate)]
#[template(path = "public/archive.html")]
pub struct IncidentArchivePage {
    pub tr: Tr,
    pub branding: BrandingView,
    /// Incidents bucketed by `(year, month-name)` in DESC chronological
    /// order. The template iterates each month as a section so the user
    /// scans by date without explicit date-pickers — UX matches the
    /// Atlassian / Statuspage.io archive convention.
    pub months: Vec<MonthBucket>,
    /// Opaque keyset cursor for the *next* page of older incidents; `None`
    /// when this is the last page. The "Older incidents →" link only
    /// renders when set.
    pub next_cursor: Option<String>,
    pub rss_url: &'static str,
    /// Per-page robots directive; see [`archive_robots`].
    pub robots: &'static str,
    pub og: OgMeta,
}

pub struct MonthBucket {
    /// Already formatted ("May 2026"), so the template renders it verbatim.
    pub label: String,
    pub incidents: Vec<IncidentSummary>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ArchiveParams {
    /// Keyset cursor returned by the previous archive page's next link.
    /// Same opaque-token shape used by `/api/public/v1/incidents`.
    pub cursor: Option<String>,
}
pub async fn index(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<StatusParams>,
) -> Response {
    let page_ref = match resolve_status_page(&state.request_state(), &headers).await {
        Ok(p) => p,
        Err(err) => return render_public_error(err),
    };
    let (page, markers) = match state.public_source.page_with_markers(page_ref).await {
        Ok(pair) => pair,
        Err(err) => return render_public_error(err),
    };
    let silenced: std::collections::HashSet<Uuid> = state
        .silence_store
        .open_target_ids(page_ref.org)
        .await
        .unwrap_or_default()
        .into_iter()
        .collect();
    let tr = Tr::new(state.public_source.locale(page_ref).await);
    let view = build_view(&page, &markers, &silenced, tr);
    if params.fragment.unwrap_or(0) != 0 {
        // Chrome-free auto-refresh fragment: no header/footer/style, so the
        // branding lookup is skipped on the 30s poll.
        let mut resp = with_language(StatusRegion { tr, view }, tr);
        // The page's own content at a second URL, with no <head> to say so. A
        // bare noindex on a duplicate can take the page it duplicates with it,
        // hence the canonical.
        resp.headers_mut()
            .insert(robots::X_ROBOTS_TAG, robots::NOINDEX_FOLLOW);
        if let Some(canonical) = canonical_link(&state, &headers, page_ref.page) {
            resp.headers_mut().insert(header::LINK, canonical);
        }
        resp
    } else {
        let branding = resolve_branding(
            &state,
            &headers,
            page_ref.org,
            page_ref.page,
            &page.site_name,
        )
        .await;
        let og = build_og_meta(
            &state,
            &headers,
            page_ref.page,
            branding.home,
            branding.status_title(),
            tr.t_args(
                "og-status-description",
                [("name", branding.display_name.as_str().into())],
            ),
            "website",
            &branding,
        );
        with_language(
            StatusFullPage {
                tr,
                view,
                branding,
                og,
            },
            tr,
        )
    }
}

fn with_language(page: impl IntoResponse, tr: Tr) -> Response {
    let mut resp = page.into_response();
    resp.headers_mut().insert(
        header::CONTENT_LANGUAGE,
        header::HeaderValue::from_static(tr.lang()),
    );
    resp
}

/// A tenant host answers the same page at `/`, so `/status` there is a second
/// URL for one page: it splits search ranking and gives visitors two addresses
/// to share. Redirect to the one the page links to itself. A path-based deploy
/// serves the operator dashboard at `/`, so `/status` stays the page there.
pub async fn status_path(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    query: Query<StatusParams>,
) -> Response {
    if !is_subdomain_public_request(&state.request_state(), &headers) {
        return index(State(state), headers, query).await;
    }
    // Resolve before redirecting so a host with no live page still 404s here
    // rather than answering for one it cannot serve.
    if let Err(err) = resolve_status_page(&state.request_state(), &headers).await {
        return render_public_error(err);
    }
    // The 30s refresh poll of an already-open tab still asks for `?fragment=1`
    // here, and losing the query would swap the whole page into the region.
    let target = match uri.query() {
        Some(q) => format!("/?{q}"),
        None => "/".to_owned(),
    };
    Redirect::permanent(&target).into_response()
}

/// Serves the page's uploaded logo (or 404 when none is set). Same host→page
/// resolution as the page itself; the query string is a cache-buster only,
/// never a selector — the bytes come from the page's `logo` asset row.
pub async fn logo(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let page_ref = match resolve_status_page(&state.request_state(), &headers).await {
        Ok(p) => p,
        Err(err) => return render_public_error(err),
    };
    match state
        .page_asset_store
        .get(page_ref.page, AssetSlot::Logo)
        .await
    {
        Ok(Some(asset)) => (
            [
                (header::CONTENT_TYPE, asset.content_type),
                (
                    header::CACHE_CONTROL,
                    "public, max-age=3600, immutable".to_owned(),
                ),
                // User-uploaded bytes served on the same origin as the app
                // (path-based public routing). nosniff stops MIME-guessing
                // past the forced image type; inline keeps it a passive asset.
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
                (header::CONTENT_DISPOSITION, "inline".to_owned()),
            ],
            asset.bytes,
        )
            .into_response(),
        Ok(None) => render_public_error(PublicAppError::NotFound),
        Err(e) => render_public_error(PublicAppError::from(e)),
    }
}

/// Leads with the title: every incident on one component would otherwise ship
/// a byte-identical description. The component name is a lookup that can come
/// back empty, so it drops out rather than leaving a dangling "affecting ".
fn incident_description(title: &str, component_name: &str, display_name: &str, tr: Tr) -> String {
    // Every interpolated value is customer text, so each is capped: three
    // unbounded names would push the tag well past what a SERP snippet shows.
    let cap = crate::notifier::truncate_chars;
    let full = crate::public_status::status_title(display_name);
    let page = if full.chars().count() <= 37 {
        full
    } else {
        format!("{} Status", cap(display_name, 30))
    };
    let title = cap(title, 60);
    if component_name.is_empty() {
        tr.t_args(
            "og-incident-description",
            [("title", title.into()), ("page", page.into())],
        )
    } else {
        tr.t_args(
            "og-incident-description-affecting",
            [
                ("title", title.into()),
                ("component", cap(component_name, 30).into()),
                ("page", page.into()),
            ],
        )
    }
}

pub async fn incident(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Response {
    let page_ref = match resolve_status_page(&state.request_state(), &headers).await {
        Ok(p) => p,
        Err(err) => return render_public_error(err),
    };
    let (inc_res, page_res, locale) = tokio::join!(
        state.public_source.incident_by_id(page_ref, id),
        state.public_source.page(page_ref),
        state.public_source.locale(page_ref),
    );
    let tr = Tr::new(locale);
    let inc = match inc_res {
        Ok(i) => i,
        Err(err) => return render_public_error(err),
    };
    let fallback_name = match page_res {
        Ok(p) => p.site_name.clone(),
        Err(err) => return render_public_error(err),
    };
    let branding = resolve_branding(
        &state,
        &headers,
        page_ref.org,
        page_ref.page,
        &fallback_name,
    )
    .await;
    let now = Utc::now();
    let og = build_og_meta(
        &state,
        &headers,
        page_ref.page,
        &format!("/status/incidents/{id}"),
        format!("{} · {}", inc.title, branding.status_title()),
        incident_description(&inc.title, &inc.component_name, &branding.display_name, tr),
        "article",
        &branding,
    );
    with_language(
        IncidentDetailPage {
            tr,
            branding,
            incident: IncidentDetailView::from_incident(&inc, now, tr),
            generated_at: now,
            rss_url: RSS_URL,
            og,
        },
        tr,
    )
}

/// Cursor-paginated archive view of every public incident for the org.
/// Groups visually by month in DESC order so the user scans the page like
/// a calendar without an explicit date picker. The link from the main
/// status page (`/status`) lands here without a cursor; the "Older
/// incidents →" link at the bottom passes `?cursor=…` to walk backwards.
pub async fn archive(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<ArchiveParams>,
) -> Response {
    use crate::pagination::cursor::IncidentCursor;
    use crate::public_status::IncidentListQuery;

    let page_ref = match resolve_status_page(&state.request_state(), &headers).await {
        Ok(p) => p,
        Err(err) => return render_public_error(err),
    };
    let cursor = match params.cursor.as_deref().map(IncidentCursor::decode) {
        Some(Ok(c)) => Some(c),
        Some(Err(_)) => return render_public_error(PublicAppError::BadRequest("invalid cursor")),
        None => None,
    };
    let query = IncidentListQuery {
        limit: ARCHIVE_PAGE_SIZE,
        cursor,
        ongoing_only: false,
    };
    let (page_res, list_res, locale) = tokio::join!(
        state.public_source.page(page_ref),
        state.public_source.list_incidents(page_ref, query),
        state.public_source.locale(page_ref),
    );
    let tr = Tr::new(locale);
    let fallback_name = match page_res {
        Ok(p) => p.site_name.clone(),
        Err(err) => return render_public_error(err),
    };
    let listing = match list_res {
        Ok(l) => l,
        Err(err) => return render_public_error(err),
    };
    let branding = resolve_branding(
        &state,
        &headers,
        page_ref.org,
        page_ref.page,
        &fallback_name,
    )
    .await;
    let now = Utc::now();
    let months = bucket_by_month(&listing.items, now, tr);
    // Self-canonical, cursor included. Pointing a cursor page at the entry
    // point instead would pair a canonical with the `noindex` below, and the
    // entry point can inherit that `noindex` — the one page here worth indexing.
    let path = match params.cursor.as_deref() {
        Some(cursor) => format!("/status/incidents?cursor={cursor}"),
        None => "/status/incidents".to_string(),
    };
    let og = build_og_meta(
        &state,
        &headers,
        page_ref.page,
        &path,
        format!("{} · {}", tr.t("archive-heading"), branding.status_title()),
        tr.t_args(
            "og-archive-description",
            [("page", branding.status_title().into())],
        ),
        "website",
        &branding,
    );
    let robots = archive_robots(params.cursor.as_deref(), &branding);
    with_language(
        IncidentArchivePage {
            tr,
            branding,
            months,
            next_cursor: listing.next_cursor,
            rss_url: RSS_URL,
            robots,
            og,
        },
        tr,
    )
}

/// Group sorted-DESC incidents into per-month buckets. Sort order is
/// preserved within and across buckets because the caller hands us rows
/// already ordered by `(started_at DESC, id DESC)` via the keyset query.
fn bucket_by_month(items: &[PublicIncident], now: DateTime<Utc>, tr: Tr) -> Vec<MonthBucket> {
    let mut out: Vec<MonthBucket> = Vec::new();
    for incident in items {
        let label = tr.month_year(incident.started_at);
        let summary = build_incident_summary(incident, now, tr);
        match out.last_mut() {
            Some(bucket) if bucket.label == label => bucket.incidents.push(summary),
            _ => out.push(MonthBucket {
                label,
                incidents: vec![summary],
            }),
        }
    }
    out
}

/// Maps a `PublicAppError` to an HTML response for the rendered routes —
/// avoids leaking the JSON envelope into the browser.
fn render_public_error(err: PublicAppError) -> Response {
    match err {
        PublicAppError::NotFound => {
            (StatusCode::NOT_FOUND, NotFoundPage { active_tab: "" }).into_response()
        }
        PublicAppError::InvalidDays | PublicAppError::BadRequest(_) => {
            (StatusCode::BAD_REQUEST, NotFoundPage { active_tab: "" }).into_response()
        }
        PublicAppError::Unavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            UnavailablePage { active_tab: "" },
        )
            .into_response(),
        PublicAppError::Internal(e) => {
            tracing::error!(error = %e, "public status page internal error");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                UnavailablePage { active_tab: "" },
            )
                .into_response()
        }
    }
}

/// Absolute `rel=canonical` for the page a fragment duplicates. `None` when no
/// origin is known, leaving the fragment its `noindex` alone.
fn canonical_link(
    state: &AppState,
    headers: &HeaderMap,
    page: StatusPageId,
) -> Option<header::HeaderValue> {
    let origin =
        published_page_origin(&state.request_state(), headers, page).unwrap_or_else(|| {
            state
                .cfg
                .auth
                .public_base_url
                .trim_end_matches('/')
                .to_owned()
        });
    if origin.is_empty() {
        return None;
    }
    let home = branding::status_home(state, headers);
    let home = home.strip_suffix('/').unwrap_or(home);
    header::HeaderValue::from_str(&format!("<{origin}{home}>; rel=\"canonical\"")).ok()
}

/// A cursor is opaque but forgeable, and every forgery resolves to a valid
/// page, so only the cursor-less entry point is offered to the index. The
/// rest stay crawlable: the archive is the only path to older incidents.
fn archive_robots(cursor: Option<&str>, branding: &BrandingView) -> &'static str {
    match cursor {
        Some(_) => "noindex,follow",
        None => branding.robots(),
    }
}
