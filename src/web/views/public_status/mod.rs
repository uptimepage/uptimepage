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

use crate::api::public_error::PublicAppError;
use crate::app::AppState;
use crate::domain::AssetSlot;
use crate::domain::PublicIncident;
use crate::web::error::{NotFoundPage, UnavailablePage};
use crate::web::filters;
use crate::web::host::{is_subdomain_public_request, resolve_status_page};

mod branding;
mod og;
#[cfg(test)]
mod tests;
mod urls;
mod view;

pub use branding::{BrandingView, render_about, safe_brand_color, safe_brand_text_for};
pub use og::OgMeta;
pub use urls::{
    LOGO_ROUTE, public_base, public_host_suffix, public_logo_url, public_status_url, status_url_for,
};
pub use view::{
    ComponentView, DayCell, GroupView, IncidentDetailView, IncidentHeader, IncidentSummary,
    IncidentUpdateView, MaintenanceView, StatusView,
};

use branding::resolve_branding;
use og::build_og_meta;
use view::{RSS_URL, build_incident_summary, build_view};

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
    pub view: StatusView,
    pub branding: BrandingView,
    pub og: OgMeta,
}

#[derive(Template, WebTemplate)]
#[template(path = "public/region.html")]
pub struct StatusRegion {
    pub view: StatusView,
}

#[derive(Template, WebTemplate)]
#[template(path = "public/incident.html")]
pub struct IncidentDetailPage {
    pub branding: BrandingView,
    pub incident: IncidentDetailView,
    pub generated_at: DateTime<Utc>,
    pub rss_url: &'static str,
    pub og: OgMeta,
}

#[derive(Template, WebTemplate)]
#[template(path = "public/archive.html")]
pub struct IncidentArchivePage {
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
    /// Localised-ish label like "May 2026". Already date-formatted, so the
    /// template renders it verbatim with no extra filters.
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
    let page_ref = match resolve_status_page(&state, &headers).await {
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
    let view = build_view(&page, &markers, &silenced);
    if params.fragment.unwrap_or(0) != 0 {
        // Chrome-free auto-refresh fragment: no header/footer/style, so the
        // branding lookup is skipped on the 30s poll.
        StatusRegion { view }.into_response()
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
            branding.home,
            format!("{} Status", branding.display_name),
            format!(
                "Live and past status for {}: current uptime for every component, open and recent incidents, scheduled maintenance windows, and email or webhook updates.",
                branding.display_name
            ),
            "website",
            &branding,
        );
        StatusFullPage { view, branding, og }.into_response()
    }
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
    if !is_subdomain_public_request(&state, &headers) {
        return index(State(state), headers, query).await;
    }
    // Resolve before redirecting so a host with no live page still 404s here
    // rather than answering for one it cannot serve.
    if let Err(err) = resolve_status_page(&state, &headers).await {
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
    let page_ref = match resolve_status_page(&state, &headers).await {
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
fn incident_description(title: &str, component_name: &str, display_name: &str) -> String {
    // Every interpolated value is customer text, so each is capped: three
    // unbounded names would push the tag well past what a SERP snippet shows.
    let cap = crate::notifier::truncate_chars;
    let affected = if component_name.is_empty() {
        String::new()
    } else {
        format!(", affecting {}", cap(component_name, 30))
    };
    format!(
        "{}{affected}: current phase, when it started and ended, and every update posted on the {} status page.",
        cap(title, 60),
        cap(display_name, 30)
    )
}

pub async fn incident(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Response {
    let page_ref = match resolve_status_page(&state, &headers).await {
        Ok(p) => p,
        Err(err) => return render_public_error(err),
    };
    let (inc_res, page_res) = tokio::join!(
        state.public_source.incident_by_id(page_ref, id),
        state.public_source.page(page_ref),
    );
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
        &format!("/status/incidents/{id}"),
        format!("{} · {} Status", inc.title, branding.display_name),
        incident_description(&inc.title, &inc.component_name, &branding.display_name),
        "article",
        &branding,
    );
    IncidentDetailPage {
        branding,
        incident: IncidentDetailView::from_incident(&inc, now),
        generated_at: now,
        rss_url: RSS_URL,
        og,
    }
    .into_response()
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
    use crate::api::cursor::IncidentCursor;
    use crate::public_status::IncidentListQuery;

    let page_ref = match resolve_status_page(&state, &headers).await {
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
    let (page_res, list_res) = tokio::join!(
        state.public_source.page(page_ref),
        state.public_source.list_incidents(page_ref, query),
    );
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
    let months = bucket_by_month(&listing.items, now);
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
        &path,
        format!("Incident history · {} Status", branding.display_name),
        format!(
            "Every incident published on the {} status page, grouped by month, with the components affected, when each one started and ended, and the updates posted.",
            branding.display_name
        ),
        "website",
        &branding,
    );
    let robots = archive_robots(params.cursor.as_deref(), &branding);
    IncidentArchivePage {
        branding,
        months,
        next_cursor: listing.next_cursor,
        rss_url: RSS_URL,
        robots,
        og,
    }
    .into_response()
}

/// Group sorted-DESC incidents into per-month buckets. Sort order is
/// preserved within and across buckets because the caller hands us rows
/// already ordered by `(started_at DESC, id DESC)` via the keyset query.
fn bucket_by_month(items: &[PublicIncident], now: DateTime<Utc>) -> Vec<MonthBucket> {
    let mut out: Vec<MonthBucket> = Vec::new();
    for incident in items {
        // `%B %Y` → "May 2026". chrono's locale is C, which is exactly what
        // we want for a stable status-page header; no manual month-name table.
        let label = incident.started_at.format("%B %Y").to_string();
        match out.last_mut() {
            Some(bucket) if bucket.label == label => {
                bucket.incidents.push(build_incident_summary(incident, now));
            }
            _ => {
                out.push(MonthBucket {
                    label,
                    incidents: vec![build_incident_summary(incident, now)],
                });
            }
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

/// A cursor is opaque but forgeable, and every forgery resolves to a valid
/// page, so only the cursor-less entry point is offered to the index. The
/// rest stay crawlable: the archive is the only path to older incidents.
fn archive_robots(cursor: Option<&str>, branding: &BrandingView) -> &'static str {
    match cursor {
        Some(_) => "noindex,follow",
        None => branding.robots(),
    }
}
