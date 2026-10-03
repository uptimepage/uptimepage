//! Public, token-gated read-only view of a single monitor (`/m/{token}`).
//!
//! A share link renders the same detail dashboard an authenticated org member
//! sees at `/targets/{id}` — status, uptime, latency/response charts, recent
//! results, incidents — to anyone with the link, no account. The only thing
//! held back is secrets: the check config is run through `redact_check_for_public`
//! (credentials, every header value, the body, and URL userinfo/query all masked)
//! before it — or the displayed address derived from it — reaches the page.
//!
//! Every sub-resource the detail JS fetches is twinned under `/m/{token}` so
//! the page never calls an operator (`/targets/{id}`, `/api/v1/…`) URL. The
//! token resolves to `(org, target_id)` exactly once per request via
//! [`MonitorShareStore::resolve_active`](crate::storage::MonitorShareStore);
//! every downstream read is org-scoped from there. A bad / expired / revoked
//! token, or a since-deleted monitor, all return the same opaque 404 — no
//! enumeration signal.
//!
//! A status page's detail link is served on that page's own host, drawn in
//! its chrome; that host answers for no other link, so it can never front
//! another tenant's monitor. Opened on the operator host of a deploy that gives
//! each page a host, it redirects there.
//!
//! Per-IP abuse protection for this anonymous surface is the reverse proxy's
//! tier (see `quotas::ratelimit` — that limiter keys on the authenticated
//! subject, which a share request has none of); app-side, the live region
//! reuses the 5s `LiveDataCache` and the chart/timeline reads inherit
//! the same 90-day window + page-size clamps as the operator API, bounding
//! per-request cost.

use std::sync::Arc;

use askama::Template;
use askama_web::WebTemplate;
use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Uri, header};
use axum::response::{IntoResponse, Redirect, Response};

use crate::api::types::LatencySeries;
use crate::app::AppState;
use crate::domain::{PageRef, ResolvedShare};
use crate::error::AppError;
use crate::error::codes;
use crate::error::public::PublicAppError;
use crate::public_status::urls::page_origin;
use crate::request::host::{is_subdomain_public_request, resolve_status_page};
use crate::request::range::{RangeQuery, latency_bucket_seconds};
use crate::security::redaction::redact_check_for_public;
use crate::storage::TimeRange;
use crate::templates::filters;
use crate::web::error::WebResult;
use crate::web::robots;
use crate::web::views::public_status::{BrandingView, resolve_branding};
use crate::web::views::targets_detail::{
    CustomWindow, DEFAULT_RANGE, DetailParams, INCIDENT_DEFAULT_RANGE, INCIDENT_RANGE_KEYS,
    IncidentRow, KpiTrend, LiveDataCache, PingTally, RANGE_KEYS, ResultRow, SUBTAB_INCIDENTS,
    SUBTAB_MONITOR, UptimeStatsView, WindowLabels, badge_status, fmt_error_display,
    load_incidents_data, load_live_data_cached, ongoing_for_target, read_liveness,
    resolve_incident_window, resolve_window,
};
use crate::web::views::{RangeOption, build_range_options, describe_check, resolve_range_key};

/// Where a share is opened, relative to the page showing it as a detail link.
enum Placement {
    /// On that page's own host.
    Page { page: PageRef, name: String },
    /// A page's link on the operator host; the page lives at this origin.
    Elsewhere(String),
    /// A link minted from the monitor, or one whose page no host here serves.
    App,
}

/// A share page view: rendered, in a page's chrome or the app's, or sent to
/// its page's host.
enum Opened {
    Render(ResolvedShare, Option<BrandingView>),
    Moved(Response),
}

/// Resolve a presented token to its monitor, or a uniform 404. The one
/// cross-tenant-by-design lookup; every read past it uses the returned org.
async fn find_share(state: &AppState, token: &str) -> WebResult<ResolvedShare> {
    Ok(state
        .monitor_share_store
        .resolve_active(token)
        .await?
        .ok_or_else(share_not_found)?)
}

/// The share behind a page's poll or chart fetch. A page host answers only for
/// its own page's detail links.
async fn resolve_share(
    state: &AppState,
    headers: &HeaderMap,
    token: &str,
) -> WebResult<ResolvedShare> {
    let share = find_share(state, token).await?;
    if is_subdomain_public_request(&state.request_state(), headers) {
        place(state, headers, &share).await?;
    }
    Ok(share)
}

/// The share behind a page view, with the chrome it renders in.
async fn open_share(
    state: &AppState,
    headers: &HeaderMap,
    uri: &Uri,
    token: &str,
) -> WebResult<Opened> {
    let share = find_share(state, token).await?;
    Ok(match place(state, headers, &share).await? {
        Placement::Page { page, name } => {
            let brand = resolve_branding(state, headers, page.org, page.page, &name).await;
            Opened::Render(share, Some(brand))
        }
        Placement::Elsewhere(origin) => {
            let path = uri.path_and_query().map_or(uri.path(), |pq| pq.as_str());
            Opened::Moved(Redirect::temporary(&format!("{origin}{path}")).into_response())
        }
        Placement::App => Opened::Render(share, None),
    })
}

/// On a page host, anything but [`Placement::Page`] is a 404.
async fn place(
    state: &AppState,
    headers: &HeaderMap,
    share: &ResolvedShare,
) -> WebResult<Placement> {
    let request = state.request_state();
    let tenancy = &state.cfg.tenancy;
    let on_page_host = is_subdomain_public_request(&request, headers);
    let page = state
        .status_page_store
        .page_for_share(share.org, share.share_id)
        .await?;
    // A path-based deploy serves its page on the operator host itself.
    let host_page = if page.is_some()
        && (on_page_host || (tenancy.path_based_public_routes && !tenancy.subdomain_public_routes))
    {
        match resolve_status_page(&request, headers).await {
            Ok(host) => Some(host),
            Err(PublicAppError::NotFound) => None,
            Err(err) if on_page_host => return Err(AppError::Other(anyhow::anyhow!(err)).into()),
            // The operator host still serves every link in its own chrome.
            Err(err) => {
                tracing::warn!(error = %err, "share: page lookup failed; rendering in app chrome");
                None
            }
        }
    } else {
        None
    };
    Ok(match (page, host_page) {
        (Some(page), Some(host)) if host.page == page.id => Placement::Page {
            page: host,
            name: page.name,
        },
        _ if on_page_host => return Err(share_not_found().into()),
        (Some(page), _) if tenancy.subdomain_public_routes => {
            let domain = state.custom_domains.published(page.id);
            Placement::Elsewhere(page_origin(
                &state.cfg.public_status.base_domain,
                &state.cfg.auth.public_base_url,
                &page.slug,
                domain.as_deref(),
                true,
            ))
        }
        _ => Placement::App,
    })
}

/// Kept out of shared caches: a page host's edge marks responses public by
/// default, and a revoked token has to stop answering at once.
fn uncached(mut resp: Response) -> Response {
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

fn share_not_found() -> AppError {
    AppError::not_found(codes::SHARE_NOT_FOUND, "shared monitor not found")
}

/// Cap a caller-supplied `from`/`to` window so an anonymous viewer can't request
/// an unbounded ClickHouse scan. The operator detail pages are authenticated +
/// rate-limited per subject; this public surface is not. Mirrors the API reads'
/// 90-day `MAX_RANGE_DAYS` (the `/latency` and `/results` twins already inherit
/// it via `RangeQuery::resolve`).
const MAX_SHARE_WINDOW_DAYS: i64 = 90;

fn clamp_window(params: &mut DetailParams) {
    if let Some(from) = params.from {
        let to = params.to.unwrap_or_else(chrono::Utc::now);
        let floor = to - chrono::Duration::days(MAX_SHARE_WINDOW_DAYS);
        if from < floor {
            params.from = Some(floor);
        }
    }
}

/// Bump the share's view counter without blocking or failing the render. Called
/// on a page view (the detail / incidents pages), never on the live/chart/result
/// sub-resource polls, so the count tracks opens rather than refresh traffic.
fn record_view(state: &AppState, share_id: crate::domain::MonitorShareId) {
    let store = state.monitor_share_store.clone();
    tokio::spawn(async move {
        if let Err(err) = store.record_view(share_id).await {
            tracing::warn!(error = %err, "monitor_share record_view failed");
        }
    });
}

#[derive(Template, WebTemplate)]
#[template(path = "share/detail.html")]
pub struct ShareDetailPage {
    pub token: String,
    pub subtab: &'static str,
    pub ongoing_count: usize,
    pub name: String,
    pub kind: &'static str,
    pub address: String,
    pub interval_s: u64,
    pub enabled: bool,
    pub tags: Vec<String>,
    pub last_status: &'static str,
    pub last_at_iso: Arc<str>,
    pub uptime: Arc<UptimeStatsView>,
    pub pings: Option<PingTally>,
    pub kpi: Arc<KpiTrend>,
    pub results: Arc<[ResultRow]>,
    pub results_has_more: bool,
    /// Check config with credentials redacted to `***`.
    pub config_json: String,
    pub range: &'static str,
    pub range_options: Vec<RangeOption>,
    pub range_base_path: String,
    pub from_iso: String,
    pub to_iso: String,
    pub from_human: String,
    pub to_human: String,
    /// Always `None` — the public share surface is not region-filtered; the
    /// field exists so the shared range-pills partial type-checks.
    pub selected_region: Option<String>,
    /// The public results table hides the region column (matches the owner view).
    pub show_region: bool,
    /// Always `false`: remediation advice is for the monitor's owner.
    pub show_guidance: bool,
    /// Monitor runs from more than one region; the charts merge them.
    pub all_regions: bool,
    /// The status page this view is drawn for; `None` = the app's own chrome.
    pub brand: Option<BrandingView>,
}

#[derive(Template, WebTemplate)]
#[template(path = "share/partials/share_live.html")]
pub struct ShareLive {
    pub token: String,
    pub range: &'static str,
    pub enabled: bool,
    pub last_status: &'static str,
    pub uptime: Arc<UptimeStatsView>,
    pub pings: Option<PingTally>,
    pub kpi: Arc<KpiTrend>,
    pub results: Arc<[ResultRow]>,
    pub results_has_more: bool,
    pub last_at_iso: Arc<str>,
    pub show_region: bool,
    /// Always `false`: remediation advice is for the monitor's owner.
    pub show_guidance: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "share/incidents.html")]
pub struct ShareIncidentsPage {
    pub token: String,
    pub subtab: &'static str,
    pub ongoing_count: usize,
    pub name: String,
    pub kind: &'static str,
    pub address: String,
    pub interval_s: u64,
    pub enabled: bool,
    pub tags: Vec<String>,
    pub last_status: &'static str,
    pub last_at_iso: String,
    pub incidents: Vec<IncidentRow>,
    pub incidents_has_more: bool,
    pub results_base: String,
    pub range: &'static str,
    pub range_options: Vec<RangeOption>,
    pub range_base_path: String,
    pub from_iso: String,
    pub to_iso: String,
    pub from_human: String,
    pub to_human: String,
    pub selected_region: Option<String>,
    /// Always `None`: a share link is a status view, not a diagnosis.
    pub unconfirmed: Option<crate::web::views::targets_detail::UnconfirmedFailures>,
    /// The status page this view is drawn for; `None` = the app's own chrome.
    pub brand: Option<BrandingView>,
}

pub async fn detail(
    State(state): State<AppState>,
    Extension(cache): Extension<LiveDataCache>,
    Path(token): Path<String>,
    uri: Uri,
    headers: HeaderMap,
    Query(mut params): Query<DetailParams>,
) -> WebResult<Response> {
    clamp_window(&mut params);
    let (resolved, brand) = match open_share(&state, &headers, &uri, &token).await? {
        Opened::Render(share, brand) => (share, brand),
        Opened::Moved(to) => return Ok(to),
    };
    record_view(&state, resolved.share_id);
    let mut target = state
        .target_store
        .get(resolved.org, resolved.target_id)
        .await?
        .ok_or_else(share_not_found)?;
    // Public surface: never print anything that can carry a secret. The single
    // redaction chokepoint on the share path — masks credentials, every header
    // value, the body, and URL userinfo/query — before either the config JSON
    // or the displayed address is derived from the check.
    redact_check_for_public(&mut target.check);

    let range_key = resolve_range_key(params.range.as_deref(), &RANGE_KEYS, DEFAULT_RANGE);
    let (from, to) = resolve_window(range_key, params.from, params.to);
    let labels = WindowLabels::new(from, to);
    let live = load_live_data_cached(
        &state,
        &cache,
        resolved.org,
        &target,
        range_key,
        CustomWindow {
            from: params.from,
            to: params.to,
        },
        None,
    )
    .await?;
    let ongoing_count = ongoing_for_target(&state, resolved.org, resolved.target_id).await;
    let config_json = serde_json::to_string_pretty(&target.check)
        .map_err(|e| AppError::Other(anyhow::anyhow!(e)))?;
    let (kind, address) = describe_check(&target.check);
    let range_base_path = format!("/m/{token}");
    let all_regions = state
        .target_store
        .regions_for_target(resolved.org, resolved.target_id)
        .await?
        .is_some_and(|r| r.len() > 1);

    // The same override the owner's page applies, so one monitor cannot read
    // late on one surface and up on the other.
    let last_status = badge_status(
        live.last_status,
        read_liveness(&state, resolved.org, &target).await.as_ref(),
    );

    Ok(uncached(
        ShareDetailPage {
            token,
            subtab: SUBTAB_MONITOR,
            ongoing_count,
            name: target.name,
            kind,
            address,
            interval_s: target.interval.as_secs(),
            enabled: target.enabled,
            tags: target.tags,
            last_status,
            last_at_iso: Arc::clone(&live.last_at_iso),
            uptime: Arc::clone(&live.uptime),
            pings: live.pings,
            kpi: Arc::clone(&live.kpi),
            results: Arc::clone(&live.result_rows),
            results_has_more: live.results_has_more,
            config_json,
            range: range_key,
            range_options: build_range_options(range_key, &RANGE_KEYS),
            range_base_path,
            from_iso: labels.from_iso,
            to_iso: labels.to_iso,
            from_human: labels.from_human,
            to_human: labels.to_human,
            selected_region: None,
            show_region: false,
            show_guidance: false,
            all_regions,
            brand,
        }
        .into_response(),
    ))
}

/// htmx-polled live region twin of `targets_detail::live_partial`, scoped to
/// the share token. Reads the shared 5s `LiveDataCache`.
pub async fn live_partial(
    State(state): State<AppState>,
    Extension(cache): Extension<LiveDataCache>,
    Path(token): Path<String>,
    headers: HeaderMap,
    Query(mut params): Query<DetailParams>,
) -> WebResult<Response> {
    clamp_window(&mut params);
    let resolved = resolve_share(&state, &headers, &token).await?;
    let range_key = resolve_range_key(params.range.as_deref(), &RANGE_KEYS, DEFAULT_RANGE);
    // Monitor gone (cascade-deleted with its share between resolve and now) →
    // uniform 404, so the poll stops rather than rendering a dead region. Read
    // first, not alongside: its kind decides what the live read has to fetch.
    let target = state
        .target_store
        .get(resolved.org, resolved.target_id)
        .await?
        .ok_or_else(share_not_found)?;
    let live = load_live_data_cached(
        &state,
        &cache,
        resolved.org,
        &target,
        range_key,
        CustomWindow {
            from: params.from,
            to: params.to,
        },
        None,
    )
    .await?;
    let last_status = badge_status(
        live.last_status,
        read_liveness(&state, resolved.org, &target).await.as_ref(),
    );

    let page = ShareLive {
        token,
        range: range_key,
        enabled: target.enabled,
        last_status,
        uptime: Arc::clone(&live.uptime),
        pings: live.pings,
        kpi: Arc::clone(&live.kpi),
        results: Arc::clone(&live.result_rows),
        results_has_more: live.results_has_more,
        last_at_iso: Arc::clone(&live.last_at_iso),
        show_region: false,
        show_guidance: false,
    };
    let rendered = page
        .render()
        .map_err(|e| AppError::Other(anyhow::anyhow!(e)))?;
    let mut resp = (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        rendered,
    )
        .into_response();
    resp.headers_mut()
        .insert(robots::X_ROBOTS_TAG, robots::NOINDEX_NOFOLLOW);
    Ok(resp)
}

pub async fn incidents(
    State(state): State<AppState>,
    Path(token): Path<String>,
    uri: Uri,
    headers: HeaderMap,
    Query(mut params): Query<DetailParams>,
) -> WebResult<Response> {
    clamp_window(&mut params);
    let (resolved, brand) = match open_share(&state, &headers, &uri, &token).await? {
        Opened::Render(share, brand) => (share, brand),
        Opened::Moved(to) => return Ok(to),
    };
    record_view(&state, resolved.share_id);
    let mut target = state
        .target_store
        .get(resolved.org, resolved.target_id)
        .await?
        .ok_or_else(share_not_found)?;
    // The header on this page shows the monitor address too; strip URL secrets
    // before deriving it (no config JSON is rendered on the incidents tab).
    redact_check_for_public(&mut target.check);

    let range_key = resolve_range_key(
        params.range.as_deref(),
        &INCIDENT_RANGE_KEYS,
        INCIDENT_DEFAULT_RANGE,
    );
    let (from, to) = resolve_incident_window(range_key, params.from, params.to);
    let time_range = TimeRange { from, to };
    let labels = WindowLabels::new(from, to);
    let data = load_incidents_data(&state, resolved.org, resolved.target_id, time_range).await?;
    let (kind, address) = describe_check(&target.check);
    let range_base_path = format!("/m/{token}/incidents");
    let results_base = format!("/m/{token}");
    let last_status = badge_status(
        data.last_status,
        read_liveness(&state, resolved.org, &target).await.as_ref(),
    );

    Ok(uncached(
        ShareIncidentsPage {
            token,
            subtab: SUBTAB_INCIDENTS,
            ongoing_count: data.ongoing_count,
            name: target.name,
            kind,
            address,
            interval_s: target.interval.as_secs(),
            enabled: target.enabled,
            tags: target.tags,
            last_status,
            last_at_iso: data.last_at_iso,
            incidents: data.rows,
            incidents_has_more: data.has_more,
            results_base,
            range: range_key,
            range_options: build_range_options(range_key, &INCIDENT_RANGE_KEYS),
            range_base_path,
            from_iso: labels.from_iso,
            to_iso: labels.to_iso,
            from_human: labels.from_human,
            to_human: labels.to_human,
            selected_region: None,
            unconfirmed: None,
            brand,
        }
        .into_response(),
    ))
}

/// Token-scoped twin of `results::latency` — the JSON the detail charts fetch.
/// No `<head>` to carry a robots meta, and `error` strings name the checked
/// address.
fn noindexed<T: IntoResponse>(body: T) -> Response {
    let mut resp = body.into_response();
    resp.headers_mut()
        .insert(robots::X_ROBOTS_TAG, robots::NOINDEX_NOFOLLOW);
    resp
}

pub async fn latency(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: HeaderMap,
    Query(q): Query<RangeQuery>,
) -> WebResult<Response> {
    let resolved = resolve_share(&state, &headers, &token).await?;
    let range = state
        .quotas
        .clamp_history(resolved.org, q.resolve_uncapped()?)
        .await?;
    let bucket_seconds = latency_bucket_seconds(range.inner());
    let buckets = state
        .results_store
        .latency_buckets(
            resolved.org,
            resolved.target_id,
            range,
            bucket_seconds,
            None,
        )
        .await?;
    Ok(uncached(noindexed(Json(LatencySeries {
        buckets,
        bucket_seconds,
    }))))
}

/// One check result as the public timeline drawer sees it. A deliberately
/// narrow projection of `CheckResult`: no `org_id`/`target_id` (internal tenant
/// ids), and `error` run through `fmt_error_display` so the served-stale
/// annotation and raw payloads never reach the anonymous surface.
#[derive(serde::Serialize)]
struct ShareResultRow {
    timestamp: chrono::DateTime<chrono::Utc>,
    status: &'static str,
    duration_ms: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_code: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Token-scoped twin of `results::list_results` — powers the incident-timeline
/// drawer's `${base}/results` fetch. Returns `{ "items": [...] }` of the
/// redacted [`ShareResultRow`].
pub async fn results(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: HeaderMap,
    Query(q): Query<RangeQuery>,
) -> WebResult<Response> {
    let resolved = resolve_share(&state, &headers, &token).await?;
    let range = state.quotas.clamp_raw(resolved.org, q.resolve()?).await?;
    let limit = q.limit();
    let rows = state
        .results_store
        .list_results(
            resolved.org,
            resolved.target_id,
            range,
            limit,
            q.offset,
            None,
        )
        .await?;
    let items: Vec<ShareResultRow> = rows
        .into_iter()
        .map(|r| ShareResultRow {
            timestamp: r.timestamp,
            status: r.status.as_str(),
            duration_ms: r.duration_ms,
            response_code: r.response_code,
            error: r.error.as_deref().map(fmt_error_display),
        })
        .collect();
    Ok(uncached(noindexed(Json(
        serde_json::json!({ "items": items }),
    ))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::views::targets_detail::DetailParams;
    use chrono::{Duration, Utc};

    /// The shared link renders the same monitor, so it must not report health
    /// the owner's page has already qualified.
    #[test]
    fn a_late_heartbeat_reads_late_on_the_shared_link_too() {
        let mut p = share_page(false);
        p.kind = "HEARTBEAT";
        p.last_status = "late";
        let html = p.render().unwrap();
        assert!(html.contains("status-badge--late"));
        assert!(html.contains(">late<"));
    }

    fn share_page(all_regions: bool) -> ShareDetailPage {
        ShareDetailPage {
            token: "tok".into(),
            subtab: SUBTAB_MONITOR,
            ongoing_count: 0,
            name: "api".into(),
            kind: "HTTP",
            address: "https://example.com".into(),
            interval_s: 60,
            enabled: true,
            tags: vec![],
            last_status: "up",
            last_at_iso: Arc::from("2026-07-13T12:00:00Z"),
            uptime: Arc::new(UptimeStatsView {
                total: 100,
                up: 100,
                down: 0,
                degraded: 0,
                error: 0,
                uptime_pct: Some("100.00".into()),
            }),
            kpi: Arc::new(Default::default()),
            pings: None,
            results: Arc::from(vec![]),
            results_has_more: false,
            config_json: "{}".into(),
            range: "24h",
            range_options: build_range_options("24h", &RANGE_KEYS),
            range_base_path: "/m/tok".into(),
            from_iso: "2026-07-12T12:00:00Z".into(),
            to_iso: "2026-07-13T12:00:00Z".into(),
            from_human: "2026-07-12 12:00 UTC".into(),
            to_human: "2026-07-13 12:00 UTC".into(),
            selected_region: None,
            show_region: false,
            show_guidance: false,
            all_regions,
            brand: None,
        }
    }

    fn brand(show_powered_by: bool) -> BrandingView {
        BrandingView {
            display_name: "Acme".into(),
            about_html: None,
            brand_color: "#123456".into(),
            brand_text: "#ffffff",
            logo_url: None,
            show_powered_by,
            show_source_links: show_powered_by,
            style: "dark",
            home: "/",
            hide_from_search: false,
            website_url: None,
            follow_website_link: false,
        }
    }

    #[test]
    fn a_page_link_wears_the_page_chrome() {
        let mut p = share_page(false);
        p.brand = Some(brand(true));
        let html = p.render().unwrap();
        assert!(html.contains("<title>api · Acme Status</title>"));
        assert!(html.contains(r#"<html lang="en" class="theme-dark">"#));
        assert!(html.contains("--brand-color: #123456;"));
        assert!(html.contains(">← Back to status</a>"));
        assert!(html.contains("data-subscribe-open"));
        assert!(html.contains("Powered by"));
        assert!(!html.contains("Uptimepage — home"));
        assert!(!html.contains("shared · read-only"));
        assert!(!html.contains("Monitored with"));
        // A page host's CSP refuses inline scripts.
        assert!(!html.contains("<script>"));
    }

    #[test]
    fn a_white_label_page_link_says_nothing_of_uptimepage() {
        let mut p = share_page(false);
        p.brand = Some(brand(false));
        let html = p.render().unwrap();
        assert!(!html.contains("Powered by"));
        assert!(!html.contains("Licenses"));
        assert!(!html.contains("Uptimepage"));
    }

    #[test]
    fn a_monitor_link_keeps_the_app_chrome() {
        let html = share_page(false).render().unwrap();
        assert!(html.contains("<title>api · Uptimepage</title>"));
        assert!(html.contains("Uptimepage — home"));
        assert!(html.contains("shared · read-only"));
        assert!(html.contains("Monitored with"));
        assert!(!html.contains("Back to status"));
        assert!(!html.contains("--brand-color"));
    }

    #[test]
    fn live_partial_has_no_whitespace_between_its_top_level_nodes() {
        let html = ShareLive {
            token: "tok".into(),
            range: "24h",
            enabled: true,
            last_status: "up",
            uptime: Arc::new(UptimeStatsView {
                total: 100,
                up: 100,
                down: 0,
                degraded: 0,
                error: 0,
                uptime_pct: Some("100.00".into()),
            }),
            pings: None,
            kpi: Arc::new(Default::default()),
            results: Arc::from(vec![]),
            results_has_more: false,
            last_at_iso: Arc::from("2026-07-13T12:00:00Z"),
            show_region: false,
            show_guidance: false,
        }
        .render()
        .unwrap();
        // An outerHTML swap inserts every top-level node beside the target.
        assert!(html.starts_with("<section id=\"detail-live-kpi\""));
        assert!(html.contains(
            r#"hx-trigger="every 60s, sm:refresh-live from:body, sm:poll-resume from:body""#
        ));
        assert!(html.contains("data-poll-pause"));
        assert!(html.contains("data-reload-on-404"));
        assert!(!html.contains("hx-on::response-error"));
        assert!(html.contains("</section><template>"));
        assert!(html.contains("</template><span id=\"detail-recent-count\""));
        assert!(html.contains("</span><span id=\"detail-status-badge\""));
        assert!(html.ends_with("</span>"));
    }

    #[test]
    fn share_page_loads_the_polling_module() {
        let html = share_page(false).render().unwrap();
        assert!(html.contains("data-poll-pause"));
        // A revoked token 404s the live endpoint; reload onto the real 404.
        assert!(html.contains("data-reload-on-404"));
        // Without the module nothing honours either marker.
        assert!(html.contains(&crate::templates::assets::url("js/ui/polling.js")));
        // The hx-on it replaced was dead twice over: Function() under the CSP,
        // and dropping hx-trigger cancels neither timer nor from:body listeners.
        assert!(!html.contains("hx-on::response-error"));
    }

    #[test]
    fn multi_region_share_charts_say_they_merge_regions() {
        let html = share_page(true).render().unwrap();
        assert!(html.contains("latency breakdown · all regions"));
        assert!(html.contains("latency (p50/p95/p99) · all regions"));
        // The public surface stays region-blind: no per-region series, no region names.
        assert!(!html.contains("data-overlay-endpoint"));
        assert!(!html.contains("by-region"));
    }

    #[test]
    fn single_region_share_charts_claim_nothing() {
        let html = share_page(false).render().unwrap();
        assert!(html.contains("latency breakdown"));
        assert!(!html.contains("all regions"));
    }

    #[test]
    fn clamp_window_caps_an_oversized_lookback() {
        let to = Utc::now();
        let mut p = DetailParams {
            range: None,
            from: Some(to - Duration::days(3650)),
            to: Some(to),
            ..Default::default()
        };
        clamp_window(&mut p);
        let span = to - p.from.unwrap();
        assert!(span <= Duration::days(MAX_SHARE_WINDOW_DAYS));
        assert!(span >= Duration::days(MAX_SHARE_WINDOW_DAYS) - Duration::seconds(2));
    }

    #[test]
    fn clamp_window_leaves_in_bounds_and_unset_windows() {
        let to = Utc::now();
        let small = to - Duration::days(7);
        let mut p = DetailParams {
            range: None,
            from: Some(small),
            to: Some(to),
            ..Default::default()
        };
        clamp_window(&mut p);
        assert_eq!(p.from, Some(small), "an in-bounds window is untouched");

        // No explicit `from` → the preset path is already bounded; leave it.
        let mut preset = DetailParams {
            range: Some("24h".into()),
            from: None,
            to: None,
            ..Default::default()
        };
        clamp_window(&mut preset);
        assert!(preset.from.is_none());
    }
}
